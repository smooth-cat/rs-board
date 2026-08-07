#!/bin/bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/summarize-capture-performance.sh LOG.jsonl [STAGE]

Summarizes successful rs-board.performance.v1 release events. Each hot run
discards its own first five samples and must then contain 100 samples. Cold
samples are grouped by startup/wake/display_change source and report their max.
P95 uses the nearest-rank definition from plans/capture-performance.md.
EOF
}

if [[ $# -lt 1 || $# -gt 2 ]]; then
  usage >&2
  exit 2
fi

RS_BOARD_LOG=$1
RS_BOARD_STAGE=${2:-}

[[ -r "$RS_BOARD_LOG" ]] || {
  echo "performance log not found: $RS_BOARD_LOG" >&2
  exit 1
}
command -v jq >/dev/null 2>&1 || {
  echo "jq is required to summarize performance logs" >&2
  exit 1
}

jq -s -r --arg selected_stage "$RS_BOARD_STAGE" '
  def group_key:
    [
      .build_profile,
      .corpus,
      .run_kind,
      (.cold_source // "-"),
      .stage,
      (.workflow // "-"),
      (.trigger // "-"),
      (.resource // "-"),
      (.width_px // 0),
      (.height_px // 0)
    ];
  def summarize($group; $run_id; $runs; $warmup):
    ($group[$warmup:] // []) as $samples
    | ($samples | map(.duration_us) | sort) as $durations
    | $group[0] as $first
    | (
        if $first.run_kind == "cold"
          and ($first.stage == "capture.editor_frame_submitted"
            or $first.stage == "capture.request.total")
          and $first.width_px == 3840
          and $first.height_px == 2160
        then 500000
        else null
        end
      ) as $limit_us
    | {
        build_profile: $first.build_profile,
        corpus: $first.corpus,
        run_kind: $first.run_kind,
        cold_source: ($first.cold_source // "-"),
        run_id: $run_id,
        runs: $runs,
        stage: $first.stage,
        workflow: ($first.workflow // "-"),
        trigger: ($first.trigger // "-"),
        resource: ($first.resource // "-"),
        resolution: (
          if ($first.width_px // 0) > 0
          then "\($first.width_px)x\($first.height_px)"
          else "-"
          end
        ),
        samples: ($durations | length),
        required: (if $first.run_kind == "cold" then 10 else 100 end),
        p95_us: (
          if ($durations | length) == 0
          then null
          else $durations[((($durations | length) * 0.95) | ceil) - 1]
          end
        ),
        max_us: (if ($durations | length) == 0 then null else $durations[-1] end),
        limit_us: $limit_us
      }
    | . + {
        complete: (
          if .run_kind == "cold"
          then (
            if .samples >= .required and .runs >= .required and .samples == .runs
            then "yes"
            else "no"
            end
          )
          elif .samples >= .required
          then "yes"
          else "no"
          end
        ),
        within_limit: (
          if .limit_us == null or .max_us == null
          then "-"
          elif .max_us <= .limit_us
          then "yes"
          else "no"
          end
        )
      };

  . as $all
  | if any($all[]; .schema == "rs-board.performance.v1"
      and .stage == "performance_log.dropped")
    then error("performance log contains dropped events")
    else .
    end
  | ($all | map(select(
      .schema == "rs-board.performance.v1"
      and .stage == "performance_log.run_complete"
    ))) as $completions
  | ($all | map(select(
      .schema == "rs-board.performance.v1"
      and .stage != "performance_log.run_complete"
      and .stage != "performance_log.dropped"
    ))) as $measurements
  | ($measurements | map(.run_id) | unique) as $measurement_runs
  | if any($completions[];
      .outcome != "ok" or .dropped_events != 0 or .run_id == null)
    then error("performance log contains an incomplete run")
    elif any($measurements[]; .outcome != "ok")
    then error("performance log contains a non-ok measurement")
    elif any($measurements[]; .build_profile != "release")
    then error("performance log must contain only release events")
    elif any($measurements[]; .corpus != "solid" and .corpus != "ui" and .corpus != "photo")
    then error("corpus must be solid, ui, or photo")
    elif any($measurements[]; .run_kind != "hot" and .run_kind != "cold")
    then error("run_kind must be hot or cold")
    elif any($measurements[]; .run_id == null or .process_id == null)
    then error("every event must include run_id and process_id")
    elif any($measurements[]; (.event_sequence | type) != "number")
    then error("every event must include a numeric event_sequence")
    elif any($measurements[];
      .run_kind == "cold"
      and .cold_source != "startup"
      and .cold_source != "wake"
      and .cold_source != "display_change")
    then error("cold_source must be startup, wake, or display_change for cold runs")
    else .
    end
  | ($completions | sort_by(.run_id) | group_by(.run_id)) as $completion_groups
  | if any($completion_groups[]; length != 1)
    then error("every run must contain exactly one completion record")
    else .
    end
  | ($measurement_runs | map(
      . as $run_id
      | ($completions | map(select(.run_id == $run_id))) as $run_completions
      | ($measurements | map(select(.run_id == $run_id) | .event_sequence) | max) as $last_event
      | {
          completion_count: ($run_completions | length),
          terminal: (
            ($run_completions | length) == 1
            and ($run_completions[0].event_sequence | type) == "number"
            and $run_completions[0].event_sequence > $last_event
          )
        }
    )) as $run_integrity
  | if any($run_integrity[]; .completion_count != 1 or (.terminal | not))
    then error("every measured run must end with one clean terminal completion record")
    else $measurements
    end
  | map(select(
      .outcome == "ok"
      and ($selected_stage == "" or .stage == $selected_stage)
    )) as $events
  | if any($events[];
      (.stage == "capture.editor_frame_submitted"
        or .stage == "capture.request.total"
        or .stage == "persistence.request_to_ui_complete"
        or .stage == "persistence.store.total")
      and ((.width_px // 0) <= 0 or (.height_px // 0) <= 0))
    then error("acceptance-stage events must include native pixel dimensions")
    else $events
    end
  | (
      $events
      | map(select(.run_kind == "hot"))
      | sort_by(.run_id, group_key, .event_sequence)
      | group_by([.run_id, group_key])
      | map(. as $group | summarize($group; $group[0].run_id; 1; 5))
    ) as $hot
  | (
      $events
      | map(select(.run_kind == "cold"))
      | sort_by(group_key, .run_id, .event_sequence)
      | group_by(group_key)
      | map(
          . as $group
          | ($group | map(.run_id) | unique | length) as $runs
          | summarize($group; "-"; $runs; 0)
        )
    ) as $cold
  | ($hot + $cold) as $rows
  | if ($rows | length) == 0
    then error("no successful performance events matched the selection")
    else $rows
    end
  | (["build_profile", "corpus", "run_kind", "cold_source", "run_id", "runs", "stage", "workflow", "trigger", "resource", "resolution", "samples", "required", "p95_us", "max_us", "limit_us", "complete", "within_limit"] | @tsv),
    (.[] | [
      .build_profile,
      .corpus,
      .run_kind,
      .cold_source,
      .run_id,
      .runs,
      .stage,
      .workflow,
      .trigger,
      .resource,
      .resolution,
      .samples,
      .required,
      (.p95_us // "-"),
      (.max_us // "-"),
      (.limit_us // "-"),
      .complete,
      .within_limit
    ] | @tsv)
' "$RS_BOARD_LOG"
