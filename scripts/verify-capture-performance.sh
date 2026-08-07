#!/bin/bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "Usage: scripts/verify-capture-performance.sh LOG.jsonl [STAGE]" >&2
  exit 2
fi

RS_BOARD_ROOT=$(cd "$(dirname "$0")/.." && pwd)
RS_BOARD_SUMMARY=$(
  "$RS_BOARD_ROOT/scripts/summarize-capture-performance.sh" "$@"
)
printf '%s\n' "$RS_BOARD_SUMMARY"

if ! awk -F '\t' '
  NR == 1 { next }
  {
    if ($16 != "-") {
      targets += 1
      if ($17 != "yes") {
        print "incomplete performance group: " $7 " " $8 " " $11 > "/dev/stderr"
        failed = 1
      }
      if ($18 != "yes") {
        print "performance limit exceeded: " $7 " " $8 " " $11 > "/dev/stderr"
        failed = 1
      }
    }
  }
  END {
    if (targets == 0) {
      print "no acceptance target was found in the selected measurements" > "/dev/stderr"
      failed = 1
    }
    exit failed
  }
' <<< "$RS_BOARD_SUMMARY"; then
  exit 1
fi
