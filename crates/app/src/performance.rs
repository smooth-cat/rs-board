use std::{
  error::Error,
  ffi::OsStr,
  fs::{File, OpenOptions},
  io::{self, Write},
  path::PathBuf,
  sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{RecvTimeoutError, SyncSender, TrySendError, sync_channel},
  },
  thread::{self, JoinHandle},
  time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

const LOG_ENV: &str = "RS_BOARD_PERF_LOG";
const CORPUS_ENV: &str = "RS_BOARD_PERF_CORPUS";
const RUN_KIND_ENV: &str = "RS_BOARD_PERF_RUN_KIND";
const COLD_SOURCE_ENV: &str = "RS_BOARD_PERF_COLD_SOURCE";
const STDERR_SINK: &str = "stderr";
const CHANNEL_CAPACITY: usize = 4_096;
const WRITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const WRITER_FORCED_SHUTDOWN_GRACE: Duration = Duration::from_millis(100);
const WRITER_IDLE_POLL: Duration = Duration::from_millis(25);
const SCHEMA: &str = "rs-board.performance.v1";

static LOGGER: OnceLock<LoggerRuntime> = OnceLock::new();
static LOGGING_ACTIVE: AtomicBool = AtomicBool::new(false);
static FORCE_WRITER_SHUTDOWN: AtomicBool = AtomicBool::new(false);
static ACTIVE_TIMERS: AtomicU64 = AtomicU64::new(0);
static EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum PerformanceLogError {
  #[error("performance log is already initialized")]
  AlreadyInitialized,
  #[error("unable to open performance log {path}: {source}")]
  Open {
    path: PathBuf,
    #[source]
    source: io::Error,
  },
  #[error("unable to start performance log writer: {0}")]
  Spawn(#[source] io::Error),
}

/// Keeps the opt-in performance writer alive and flushes queued events on drop.
pub struct PerformanceLogGuard {
  active: bool,
}

impl PerformanceLogGuard {
  pub fn from_environment() -> Result<Self, PerformanceLogError> {
    let destination = std::env::var_os(LOG_ENV);
    if destination.is_none() {
      return Ok(Self { active: false });
    }
    if LOGGER.get().is_some() {
      return Err(PerformanceLogError::AlreadyInitialized);
    }
    let destination = destination.expect("checked above");
    let writer = if destination == OsStr::new(STDERR_SINK) || destination == OsStr::new("-") {
      LogWriter::Stderr(io::stderr())
    } else {
      let path = PathBuf::from(destination);
      let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|source| PerformanceLogError::Open { path, source })?;
      LogWriter::File(file)
    };
    let metadata = RunMetadata {
      corpus: environment_choice(CORPUS_ENV, &["solid", "ui", "photo"]),
      run_kind: environment_choice(RUN_KIND_ENV, &["hot", "cold"]),
      cold_source: environment_choice(COLD_SOURCE_ENV, &["startup", "wake", "display_change"]),
      build_profile: if cfg!(debug_assertions) { "debug" } else { "release" },
      run_id: Uuid::new_v4(),
      process_id: std::process::id(),
    };
    let (sender, receiver) = sync_channel(CHANNEL_CAPACITY);
    let (finished_sender, finished_receiver) = sync_channel(1);
    FORCE_WRITER_SHUTDOWN.store(false, Ordering::SeqCst);
    LOGGING_ACTIVE.store(true, Ordering::SeqCst);
    let handle =
      match thread::Builder::new().name("rs-board-performance-log".into()).spawn(move || {
        writer_loop(writer, receiver, metadata);
        let _ = finished_sender.send(());
      }) {
        Ok(handle) => handle,
        Err(error) => {
          LOGGING_ACTIVE.store(false, Ordering::SeqCst);
          return Err(PerformanceLogError::Spawn(error));
        }
      };
    let runtime = LoggerRuntime {
      sender,
      handle: Mutex::new(Some(handle)),
      finished: Mutex::new(Some(finished_receiver)),
    };
    if LOGGER.set(runtime).is_err() {
      LOGGING_ACTIVE.store(false, Ordering::SeqCst);
      return Err(PerformanceLogError::AlreadyInitialized);
    }
    Ok(Self { active: true })
  }
}

impl Drop for PerformanceLogGuard {
  fn drop(&mut self) {
    if !self.active {
      return;
    }
    let Some(logger) = LOGGER.get() else {
      return;
    };
    let shutdown_started = Instant::now();
    while ACTIVE_TIMERS.load(Ordering::SeqCst) > 0
      && shutdown_started.elapsed() < WRITER_SHUTDOWN_TIMEOUT
    {
      thread::sleep(WRITER_IDLE_POLL);
    }
    let forced = ACTIVE_TIMERS.load(Ordering::SeqCst) > 0;
    if forced {
      mark_forced_shutdown();
    }
    LOGGING_ACTIVE.store(false, Ordering::SeqCst);
    let handle = lock_unpoisoned(&logger.handle).take();
    let finished = lock_unpoisoned(&logger.finished).take();
    if let (Some(handle), Some(finished)) = (handle, finished) {
      let remaining = WRITER_SHUTDOWN_TIMEOUT.saturating_sub(shutdown_started.elapsed());
      let writer_wait = if forced { WRITER_FORCED_SHUTDOWN_GRACE } else { remaining };
      match finished.recv_timeout(writer_wait) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {
          let _ = handle.join();
        }
        Err(RecvTimeoutError::Timeout) => {
          if !forced {
            mark_forced_shutdown();
          }
          match finished.recv_timeout(WRITER_FORCED_SHUTDOWN_GRACE) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
              let _ = handle.join();
            }
            Err(RecvTimeoutError::Timeout) => {}
          }
        }
      }
    }
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerformanceContext {
  pub request_id: Option<Uuid>,
  pub session_id: Option<Uuid>,
  pub capture_sequence: Option<u64>,
  pub stash_sequence: Option<u64>,
  pub generation_id: Option<Uuid>,
  pub document_id: Option<Uuid>,
  pub revision: Option<u64>,
}

impl PerformanceContext {
  pub fn capture(request_id: Uuid, capture_sequence: u64) -> Self {
    Self {
      request_id: Some(request_id),
      capture_sequence: Some(capture_sequence),
      ..Self::default()
    }
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PerformanceDetails {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub trigger: Option<&'static str>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub workflow: Option<&'static str>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub resource: Option<&'static str>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub byte_count: Option<u64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub width_px: Option<u32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub height_px: Option<u32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub display_id: Option<u32>,
}

impl PerformanceDetails {
  pub fn trigger(mut self, trigger: &'static str) -> Self {
    self.trigger = Some(trigger);
    self
  }

  pub fn workflow(mut self, workflow: &'static str) -> Self {
    self.workflow = Some(workflow);
    self
  }

  pub fn resource(mut self, resource: &'static str) -> Self {
    self.resource = Some(resource);
    self
  }

  pub fn byte_count(mut self, byte_count: usize) -> Self {
    self.byte_count = Some(u64::try_from(byte_count).unwrap_or(u64::MAX));
    self
  }

  pub fn pixel_size(mut self, [width_px, height_px]: [u32; 2]) -> Self {
    self.width_px = Some(width_px);
    self.height_px = Some(height_px);
    self
  }

  pub fn display_id(mut self, display_id: u32) -> Self {
    self.display_id = Some(display_id);
    self
  }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceOutcome {
  Ok,
  Error,
  Rejected,
  Stale,
}

#[must_use = "performance timers must be finished with an outcome"]
pub struct PerformanceTimer {
  stage: &'static str,
  context: PerformanceContext,
  details: PerformanceDetails,
  started_at: Option<Instant>,
}

impl PerformanceTimer {
  pub fn start(
    stage: &'static str,
    context: PerformanceContext,
    details: PerformanceDetails,
  ) -> Self {
    let started_at = begin_measurement().then(Instant::now);
    Self { stage, context, details, started_at }
  }

  pub fn started_at(
    stage: &'static str,
    context: PerformanceContext,
    details: PerformanceDetails,
    started_at: Instant,
  ) -> Self {
    let started_at = begin_measurement().then_some(started_at);
    Self { stage, context, details, started_at }
  }

  pub fn finish_ok(self) {
    self.finish(PerformanceOutcome::Ok, None);
  }

  pub fn finish_rejected(self) {
    self.finish(PerformanceOutcome::Rejected, None);
  }

  pub fn finish_stale(self) {
    self.finish(PerformanceOutcome::Stale, None);
  }

  pub fn finish_error<E: Error + 'static>(self, error: &E) {
    let codes = self.started_at.is_some().then(|| error_chain_codes(error));
    self.finish(PerformanceOutcome::Error, codes);
  }

  pub fn finish_error_code(self, code: &'static str) {
    let codes = self.started_at.is_some().then(|| vec![code]);
    self.finish(PerformanceOutcome::Error, codes);
  }

  fn finish(self, outcome: PerformanceOutcome, error_codes: Option<Vec<&'static str>>) {
    let Some(started_at) = self.started_at else {
      return;
    };
    submit(build_event(
      self.stage,
      self.context,
      self.details,
      started_at,
      Instant::now(),
      outcome,
      error_codes,
    ));
    finish_measurement();
  }
}

pub fn record(
  stage: &'static str,
  context: PerformanceContext,
  details: PerformanceDetails,
  outcome: PerformanceOutcome,
) {
  if !begin_measurement() {
    return;
  }
  let now = Instant::now();
  submit(build_event(stage, context, details, now, now, outcome, None));
  finish_measurement();
}

fn logging_enabled() -> bool {
  LOGGING_ACTIVE.load(Ordering::SeqCst)
}

fn begin_measurement() -> bool {
  ACTIVE_TIMERS.fetch_add(1, Ordering::SeqCst);
  if logging_enabled() {
    true
  } else {
    finish_measurement();
    false
  }
}

fn finish_measurement() {
  ACTIVE_TIMERS.fetch_sub(1, Ordering::SeqCst);
}

fn mark_forced_shutdown() {
  let incomplete = ACTIVE_TIMERS.load(Ordering::SeqCst).max(1);
  DROPPED_EVENTS.fetch_add(incomplete, Ordering::Relaxed);
  FORCE_WRITER_SHUTDOWN.store(true, Ordering::SeqCst);
}

#[derive(Debug, Serialize)]
struct PerformanceEvent {
  schema: &'static str,
  event_sequence: u64,
  recorded_at: DateTime<Utc>,
  stage: &'static str,
  outcome: PerformanceOutcome,
  duration_us: u64,
  request_id: Option<Uuid>,
  session_id: Option<Uuid>,
  capture_sequence: Option<u64>,
  stash_sequence: Option<u64>,
  generation_id: Option<Uuid>,
  document_id: Option<Uuid>,
  revision: Option<u64>,
  #[serde(flatten)]
  details: PerformanceDetails,
  #[serde(skip_serializing_if = "Option::is_none")]
  error_codes: Option<Vec<&'static str>>,
}

fn build_event(
  stage: &'static str,
  context: PerformanceContext,
  details: PerformanceDetails,
  started_at: Instant,
  finished_at: Instant,
  outcome: PerformanceOutcome,
  error_codes: Option<Vec<&'static str>>,
) -> PerformanceEvent {
  let duration = finished_at.saturating_duration_since(started_at);
  PerformanceEvent {
    schema: SCHEMA,
    event_sequence: EVENT_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    recorded_at: Utc::now(),
    stage,
    outcome,
    duration_us: duration_to_micros(duration),
    request_id: context.request_id,
    session_id: context.session_id,
    capture_sequence: context.capture_sequence,
    stash_sequence: context.stash_sequence,
    generation_id: context.generation_id,
    document_id: context.document_id,
    revision: context.revision,
    details,
    error_codes,
  }
}

fn submit(event: PerformanceEvent) {
  let Some(logger) = LOGGER.get() else {
    return;
  };
  match logger.sender.try_send(event) {
    Ok(()) => {}
    Err(TrySendError::Full(_)) => {
      DROPPED_EVENTS.fetch_add(1, Ordering::Relaxed);
    }
    Err(TrySendError::Disconnected(_)) => {
      DROPPED_EVENTS.fetch_add(1, Ordering::Relaxed);
      LOGGING_ACTIVE.store(false, Ordering::SeqCst);
    }
  }
}

struct LoggerRuntime {
  sender: SyncSender<PerformanceEvent>,
  handle: Mutex<Option<JoinHandle<()>>>,
  finished: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

#[derive(Clone)]
struct RunMetadata {
  corpus: Option<Arc<str>>,
  run_kind: Option<Arc<str>>,
  cold_source: Option<Arc<str>>,
  build_profile: &'static str,
  run_id: Uuid,
  process_id: u32,
}

#[derive(Serialize)]
struct LoggedEvent<'a> {
  #[serde(flatten)]
  event: &'a PerformanceEvent,
  #[serde(skip_serializing_if = "Option::is_none")]
  corpus: Option<&'a str>,
  #[serde(skip_serializing_if = "Option::is_none")]
  run_kind: Option<&'a str>,
  #[serde(skip_serializing_if = "Option::is_none")]
  cold_source: Option<&'a str>,
  build_profile: &'static str,
  run_id: Uuid,
  process_id: u32,
}

#[derive(Serialize)]
struct RunCompletion<'a> {
  schema: &'static str,
  event_sequence: u64,
  recorded_at: DateTime<Utc>,
  stage: &'static str,
  outcome: &'static str,
  dropped_events: u64,
  #[serde(skip_serializing_if = "Option::is_none")]
  corpus: Option<&'a str>,
  #[serde(skip_serializing_if = "Option::is_none")]
  run_kind: Option<&'a str>,
  #[serde(skip_serializing_if = "Option::is_none")]
  cold_source: Option<&'a str>,
  build_profile: &'static str,
  run_id: Uuid,
  process_id: u32,
}

enum LogWriter {
  File(File),
  Stderr(io::Stderr),
}

impl Write for LogWriter {
  fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
    match self {
      Self::File(file) => file.write(buffer),
      Self::Stderr(stderr) => stderr.write(buffer),
    }
  }

  fn flush(&mut self) -> io::Result<()> {
    match self {
      Self::File(file) => file.flush(),
      Self::Stderr(stderr) => stderr.flush(),
    }
  }
}

fn writer_loop(
  mut writer: LogWriter,
  receiver: std::sync::mpsc::Receiver<PerformanceEvent>,
  metadata: RunMetadata,
) {
  let mut writer_failed = false;
  loop {
    let event = match receiver.recv_timeout(WRITER_IDLE_POLL) {
      Ok(event) => event,
      Err(RecvTimeoutError::Timeout) if logging_enabled() => continue,
      Err(RecvTimeoutError::Timeout)
        if !FORCE_WRITER_SHUTDOWN.load(Ordering::SeqCst)
          && ACTIVE_TIMERS.load(Ordering::SeqCst) > 0 =>
      {
        continue;
      }
      Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
    };
    let logged = LoggedEvent {
      event: &event,
      corpus: metadata.corpus.as_deref(),
      run_kind: metadata.run_kind.as_deref(),
      cold_source: metadata.cold_source.as_deref(),
      build_profile: metadata.build_profile,
      run_id: metadata.run_id,
      process_id: metadata.process_id,
    };
    if serde_json::to_writer(&mut writer, &logged).is_err()
      || writer.write_all(b"\n").is_err()
      || writer.flush().is_err()
    {
      writer_failed = true;
      break;
    }
  }
  LOGGING_ACTIVE.store(false, Ordering::SeqCst);
  if writer_failed {
    DROPPED_EVENTS.fetch_add(1, Ordering::Relaxed);
  }
  let dropped = DROPPED_EVENTS.load(Ordering::Relaxed);
  let completion = RunCompletion {
    schema: SCHEMA,
    event_sequence: EVENT_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    recorded_at: Utc::now(),
    stage: "performance_log.run_complete",
    outcome: if dropped == 0 { "ok" } else { "error" },
    dropped_events: dropped,
    corpus: metadata.corpus.as_deref(),
    run_kind: metadata.run_kind.as_deref(),
    cold_source: metadata.cold_source.as_deref(),
    build_profile: metadata.build_profile,
    run_id: metadata.run_id,
    process_id: metadata.process_id,
  };
  let _ = serde_json::to_writer(&mut writer, &completion);
  let _ = writer.write_all(b"\n");
  let _ = writer.flush();
}

fn environment_choice(name: &str, allowed: &[&str]) -> Option<Arc<str>> {
  std::env::var(name)
    .ok()
    .map(|value| value.trim().to_owned())
    .filter(|value| allowed.contains(&value.as_str()))
    .map(Arc::from)
}

fn duration_to_micros(duration: Duration) -> u64 {
  u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn error_chain_codes<E: Error + 'static>(error: &E) -> Vec<&'static str> {
  let mut codes = vec![error_code(error, std::any::type_name::<E>())];
  let mut source = error.source();
  while let Some(error) = source {
    codes.push(error_code(error, "source_error"));
    source = error.source();
  }
  codes
}

fn error_code(error: &(dyn Error + 'static), fallback: &'static str) -> &'static str {
  let Some(error) = error.downcast_ref::<io::Error>() else {
    return fallback;
  };
  match error.kind() {
    io::ErrorKind::NotFound => "io.not_found",
    io::ErrorKind::PermissionDenied => "io.permission_denied",
    io::ErrorKind::AlreadyExists => "io.already_exists",
    io::ErrorKind::InvalidInput => "io.invalid_input",
    io::ErrorKind::InvalidData => "io.invalid_data",
    io::ErrorKind::TimedOut => "io.timed_out",
    io::ErrorKind::Interrupted => "io.interrupted",
    io::ErrorKind::UnexpectedEof => "io.unexpected_eof",
    io::ErrorKind::WriteZero => "io.write_zero",
    _ => "io.other",
  }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
  use std::process::{Command, Output};

  use super::*;

  #[test]
  fn event_uses_supplied_monotonic_instants_and_preserves_correlation() {
    let request_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let generation_id = Uuid::new_v4();
    let document_id = Uuid::new_v4();
    let started_at = Instant::now();
    let context = PerformanceContext {
      request_id: Some(request_id),
      session_id: Some(session_id),
      capture_sequence: Some(7),
      stash_sequence: Some(11),
      generation_id: Some(generation_id),
      document_id: Some(document_id),
      revision: Some(13),
    };
    let event = build_event(
      "test.stage",
      context,
      PerformanceDetails::default().pixel_size([3_840, 2_160]),
      started_at,
      started_at + Duration::from_micros(1_234),
      PerformanceOutcome::Ok,
      None,
    );

    assert_eq!(event.duration_us, 1_234);
    assert_eq!(event.request_id, Some(request_id));
    assert_eq!(event.session_id, Some(session_id));
    assert_eq!(event.capture_sequence, Some(7));
    assert_eq!(event.stash_sequence, Some(11));
    assert_eq!(event.generation_id, Some(generation_id));
    assert_eq!(event.document_id, Some(document_id));
    assert_eq!(event.revision, Some(13));
  }

  #[test]
  fn serialized_schema_contains_no_image_payload_or_paths() {
    let now = Instant::now();
    let event = build_event(
      "capture.pixel_convert",
      PerformanceContext::capture(Uuid::new_v4(), 3),
      PerformanceDetails::default().pixel_size([7_680, 4_320]).byte_count(128),
      now,
      now,
      PerformanceOutcome::Ok,
      None,
    );
    let json = serde_json::to_string(&event).unwrap();

    assert!(json.contains("\"schema\":\"rs-board.performance.v1\""));
    assert!(json.contains("\"duration_us\":0"));
    for forbidden in ["rgba_pixels", "encoded_png", "pixels", "path"] {
      assert!(!json.contains(forbidden), "unexpected field {forbidden}: {json}");
    }
  }

  #[test]
  fn error_codes_do_not_serialize_error_messages() {
    #[derive(Debug, Error)]
    #[error("private path /Users/example/secret.rsboard with title Quarterly Plan")]
    struct SensitiveError;

    let now = Instant::now();
    let event = build_event(
      "test.error",
      PerformanceContext::default(),
      PerformanceDetails::default(),
      now,
      now,
      PerformanceOutcome::Error,
      Some(error_chain_codes(&SensitiveError)),
    );
    let json = serde_json::to_string(&event).unwrap();

    assert!(json.contains("SensitiveError"));
    assert!(!json.contains("/Users/example"));
    assert!(!json.contains("Quarterly Plan"));
  }

  #[test]
  fn writer_emits_jsonl_with_run_metadata() {
    let path = std::env::temp_dir().join(format!("rs-board-performance-{}.jsonl", Uuid::new_v4()));
    let file = File::create(&path).unwrap();
    let (sender, receiver) = sync_channel(1);
    let metadata = RunMetadata {
      corpus: Some(Arc::from("ui")),
      run_kind: Some(Arc::from("hot")),
      cold_source: None,
      build_profile: "test",
      run_id: Uuid::nil(),
      process_id: 42,
    };
    let handle = thread::spawn(move || writer_loop(LogWriter::File(file), receiver, metadata));
    let now = Instant::now();
    sender
      .send(build_event(
        "capture.editor_frame_submitted",
        PerformanceContext::capture(Uuid::new_v4(), 1),
        PerformanceDetails::default().pixel_size([3_840, 2_160]),
        now,
        now + Duration::from_micros(42),
        PerformanceOutcome::Ok,
        None,
      ))
      .unwrap();
    drop(sender);
    handle.join().unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    let mut lines = contents.lines();
    let json: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(json["corpus"], "ui");
    assert_eq!(json["run_kind"], "hot");
    assert!(json.get("cold_source").is_none());
    assert_eq!(json["build_profile"], "test");
    assert_eq!(json["run_id"], Uuid::nil().to_string());
    assert_eq!(json["process_id"], 42);
    assert_eq!(json["duration_us"], 42);
    let completion: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(completion["stage"], "performance_log.run_complete");
    assert_eq!(completion["outcome"], "ok");
    assert_eq!(completion["dropped_events"], 0);
    assert!(lines.next().is_none());
    std::fs::remove_file(path).unwrap();
  }

  #[test]
  fn logger_shutdown_waits_for_in_flight_measurements() {
    const CHILD_ENV: &str = "RS_BOARD_PERF_LOGGER_TEST_CHILD";

    if std::env::var_os(CHILD_ENV).is_some() {
      let guard = PerformanceLogGuard::from_environment().unwrap();
      let timer = PerformanceTimer::start(
        "test.in_flight",
        PerformanceContext::default(),
        PerformanceDetails::default(),
      );
      let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        record(
          "test.nested",
          PerformanceContext::default(),
          PerformanceDetails::default(),
          PerformanceOutcome::Ok,
        );
        timer.finish_ok();
      });
      drop(guard);
      worker.join().unwrap();
      return;
    }

    let path = std::env::temp_dir().join(format!("rs-board-performance-{}.jsonl", Uuid::new_v4()));
    let output = Command::new(std::env::current_exe().unwrap())
      .arg("--exact")
      .arg("performance::tests::logger_shutdown_waits_for_in_flight_measurements")
      .env(CHILD_ENV, "1")
      .env(LOG_ENV, &path)
      .env(CORPUS_ENV, "ui")
      .env(RUN_KIND_ENV, "hot")
      .output()
      .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let contents = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<serde_json::Value> =
      contents.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["stage"], "test.nested");
    assert_eq!(lines[0]["outcome"], "ok");
    assert_eq!(lines[1]["stage"], "test.in_flight");
    assert_eq!(lines[1]["outcome"], "ok");
    assert_eq!(lines[2]["stage"], "performance_log.run_complete");
    assert_eq!(lines[2]["outcome"], "ok");
    assert_eq!(lines[2]["dropped_events"], 0);
    assert!(
      lines[2]["event_sequence"].as_u64().unwrap() > lines[1]["event_sequence"].as_u64().unwrap()
    );
    std::fs::remove_file(path).unwrap();
  }

  #[test]
  fn summary_keeps_hot_runs_independent_and_checks_cold_maximum() {
    let Some(_) = jq_available() else {
      return;
    };
    let mut hot_events = Vec::new();
    for run in 0..2 {
      for sample in 0..55 {
        hot_events.push(summary_event(&format!("hot-{run}"), "hot", None, sample + 1, sample + 1));
      }
    }
    let hot = run_summary(&hot_events);
    assert!(hot.status.success(), "{}", String::from_utf8_lossy(&hot.stderr));
    let hot_stdout = String::from_utf8(hot.stdout).unwrap();
    let hot_rows: Vec<Vec<&str>> =
      hot_stdout.lines().skip(1).map(|line| line.split('\t').collect()).collect();
    assert_eq!(hot_rows.len(), 2);
    assert!(hot_rows.iter().all(|row| row[11] == "50" && row[16] == "no"));

    let mut complete_hot_events = Vec::new();
    for run in 0..2 {
      for sample in 0..105 {
        complete_hot_events.push(summary_event(
          &format!("complete-hot-{run}"),
          "hot",
          None,
          sample + 1,
          sample + 1,
        ));
      }
    }
    let complete_hot = run_summary(&complete_hot_events);
    assert!(complete_hot.status.success(), "{}", String::from_utf8_lossy(&complete_hot.stderr));
    let complete_hot_stdout = String::from_utf8(complete_hot.stdout).unwrap();
    let complete_hot_rows: Vec<Vec<&str>> =
      complete_hot_stdout.lines().skip(1).map(|line| line.split('\t').collect()).collect();
    assert_eq!(complete_hot_rows.len(), 2);
    assert!(
      complete_hot_rows.iter().all(|row| row[11] == "100" && row[13] == "100" && row[16] == "yes")
    );

    let cold_events: Vec<_> = (0..20)
      .map(|sample| {
        summary_event(
          &format!("cold-{sample}"),
          "cold",
          Some("startup"),
          1,
          if sample == 19 { 900_000 } else { 100_000 },
        )
      })
      .collect();
    let cold = run_summary(&cold_events);
    assert!(cold.status.success(), "{}", String::from_utf8_lossy(&cold.stderr));
    let cold_stdout = String::from_utf8(cold.stdout).unwrap();
    let cold_row: Vec<_> = cold_stdout.lines().nth(1).unwrap().split('\t').collect();
    assert_eq!(cold_row[11], "20");
    assert_eq!(cold_row[14], "900000");
    assert_eq!(cold_row[15], "500000");
    assert_eq!(cold_row[16], "yes");
    assert_eq!(cold_row[17], "no");
  }

  #[test]
  fn summary_requires_independent_cold_runs_and_successful_attempts() {
    let Some(_) = jq_available() else {
      return;
    };
    let repeated_cold: Vec<_> = (0..10)
      .map(|sample| summary_event("one-cold-run", "cold", Some("startup"), sample + 1, 100_000))
      .collect();
    let cold = run_summary(&repeated_cold);
    assert!(cold.status.success(), "{}", String::from_utf8_lossy(&cold.stderr));
    let cold_stdout = String::from_utf8(cold.stdout).unwrap();
    let cold_row: Vec<_> = cold_stdout.lines().nth(1).unwrap().split('\t').collect();
    assert_eq!(cold_row[5], "1");
    assert_eq!(cold_row[11], "10");
    assert_eq!(cold_row[16], "no");

    let mut failed = summary_event("failed-run", "hot", None, 1, 900_000);
    failed["outcome"] = serde_json::Value::String("error".into());
    let failed_output = run_summary(&[failed]);
    assert!(!failed_output.status.success());
    assert!(String::from_utf8_lossy(&failed_output.stderr).contains("non-ok measurement"));
  }

  #[test]
  fn summary_rejects_dropped_events() {
    let Some(_) = jq_available() else {
      return;
    };
    let output = run_summary(&[serde_json::json!({
      "schema": SCHEMA,
      "stage": "performance_log.dropped",
      "dropped_events": 1
    })]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("dropped events"));

    let output = run_summary_inner(&[summary_event("unfinished", "hot", None, 1, 100_000)], false);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("terminal completion"));

    let measurement = summary_event("nonterminal", "hot", None, 2, 100_000);
    let completion = serde_json::json!({
      "schema": SCHEMA,
      "stage": "performance_log.run_complete",
      "outcome": "ok",
      "dropped_events": 0,
      "run_id": "nonterminal",
      "event_sequence": 1
    });
    let output = run_summary_inner(&[completion, measurement], false);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("terminal completion"));
  }

  #[test]
  fn summary_applies_every_planned_hot_path_limit() {
    let Some(_) = jq_available() else {
      return;
    };
    let cases = [
      ("capture.editor_frame_submitted", None, 3_840, 2_160, 50_000),
      ("persistence.request_to_ui_complete", Some("stash"), 3_840, 2_160, 50_000),
      ("persistence.request_to_ui_complete", Some("stash"), 7_680, 4_320, 50_000),
      ("persistence.request_to_ui_complete", Some("save"), 3_840, 2_160, 1_000_000),
      ("persistence.request_to_ui_complete", Some("save"), 7_680, 4_320, 6_000_000),
      ("stash.request.total", Some("stash"), 7_680, 4_320, 6_000_000),
    ];

    for (stage, workflow, width_px, height_px, limit_us) in cases {
      let events: Vec<_> = (0..105)
        .map(|sample| {
          serde_json::json!({
            "schema": SCHEMA,
            "outcome": "ok",
            "build_profile": "release",
            "corpus": "ui",
            "run_kind": "hot",
            "run_id": format!("{stage}-{workflow:?}"),
            "process_id": 1,
            "event_sequence": sample + 1,
            "stage": stage,
            "workflow": workflow,
            "trigger": "hotkey",
            "width_px": width_px,
            "height_px": height_px,
            "duration_us": limit_us
          })
        })
        .collect();
      let output = run_summary_stage(&events, true, stage);
      assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
      let stdout = String::from_utf8(output.stdout).unwrap();
      let row: Vec<_> = stdout.lines().nth(1).unwrap().split('\t').collect();
      assert_eq!(row[15], limit_us.to_string());
      assert_eq!(row[16], "yes");
      assert_eq!(row[17], "yes");

      let mut failing = events;
      for event in &mut failing {
        event["duration_us"] = serde_json::Value::from(limit_us + 1);
      }
      let output = run_summary_stage(&failing, true, stage);
      assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
      let stdout = String::from_utf8(output.stdout).unwrap();
      let row: Vec<_> = stdout.lines().nth(1).unwrap().split('\t').collect();
      assert_eq!(row[17], "no");
    }
  }

  #[test]
  fn verifier_fails_incomplete_or_over_limit_measurements() {
    let Some(_) = jq_available() else {
      return;
    };
    let passing: Vec<_> = (0..105)
      .map(|sample| summary_event("verify-pass", "hot", None, sample + 1, 50_000))
      .collect();
    let output = run_performance_tool(
      &passing,
      true,
      "capture.editor_frame_submitted",
      "verify-capture-performance.sh",
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let failing: Vec<_> = (0..105)
      .map(|sample| summary_event("verify-fail", "hot", None, sample + 1, 50_001))
      .collect();
    let output = run_performance_tool(
      &failing,
      true,
      "capture.editor_frame_submitted",
      "verify-capture-performance.sh",
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("performance limit exceeded"));

    let incomplete = [summary_event("verify-incomplete", "hot", None, 1, 1)];
    let output = run_performance_tool(
      &incomplete,
      true,
      "capture.editor_frame_submitted",
      "verify-capture-performance.sh",
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("incomplete performance group"));
  }

  fn jq_available() -> Option<()> {
    Command::new("jq").arg("--version").output().ok().filter(|output| output.status.success())?;
    Some(())
  }

  fn summary_event(
    run_id: &str,
    run_kind: &str,
    cold_source: Option<&str>,
    event_sequence: u64,
    duration_us: u64,
  ) -> serde_json::Value {
    serde_json::json!({
      "schema": SCHEMA,
      "outcome": "ok",
      "build_profile": "release",
      "corpus": "ui",
      "run_kind": run_kind,
      "cold_source": cold_source,
      "run_id": run_id,
      "process_id": 1,
      "event_sequence": event_sequence,
      "stage": "capture.editor_frame_submitted",
      "trigger": "hotkey",
      "width_px": 3840,
      "height_px": 2160,
      "duration_us": duration_us
    })
  }

  fn run_summary(events: &[serde_json::Value]) -> Output {
    run_summary_inner(events, true)
  }

  fn run_summary_inner(events: &[serde_json::Value], add_completions: bool) -> Output {
    run_summary_stage(events, add_completions, "capture.editor_frame_submitted")
  }

  fn run_summary_stage(events: &[serde_json::Value], add_completions: bool, stage: &str) -> Output {
    run_performance_tool(events, add_completions, stage, "summarize-capture-performance.sh")
  }

  fn run_performance_tool(
    events: &[serde_json::Value],
    add_completions: bool,
    stage: &str,
    script_name: &str,
  ) -> Output {
    let path =
      std::env::temp_dir().join(format!("rs-board-performance-summary-{}.jsonl", Uuid::new_v4()));
    let mut file = File::create(&path).unwrap();
    for event in events {
      serde_json::to_writer(&mut file, event).unwrap();
      file.write_all(b"\n").unwrap();
    }
    if add_completions {
      let mut runs = std::collections::BTreeMap::<String, u64>::new();
      for event in events {
        let (Some(run_id), Some(event_sequence)) =
          (event["run_id"].as_str(), event["event_sequence"].as_u64())
        else {
          continue;
        };
        runs
          .entry(run_id.to_owned())
          .and_modify(|sequence| *sequence = (*sequence).max(event_sequence))
          .or_insert(event_sequence);
      }
      for (run_id, last_event_sequence) in runs {
        serde_json::to_writer(
          &mut file,
          &serde_json::json!({
            "schema": SCHEMA,
            "stage": "performance_log.run_complete",
            "outcome": "ok",
            "dropped_events": 0,
            "run_id": run_id,
            "event_sequence": last_event_sequence + 1
          }),
        )
        .unwrap();
        file.write_all(b"\n").unwrap();
      }
    }
    drop(file);
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts").join(script_name);
    let output = Command::new(script).arg(&path).arg(stage).output().unwrap();
    std::fs::remove_file(path).unwrap();
    output
  }
}
