use std::{
  collections::VecDeque,
  sync::{Arc, Condvar, Mutex, MutexGuard, mpsc},
  thread::{self, JoinHandle},
  time::{Duration, Instant},
};

use common::DocumentSnapshot;
use thiserror::Error;

use crate::{
  background_encode::PreparedBackground,
  performance::{PerformanceContext, PerformanceDetails, PerformanceTimer},
  storage::{GenerationId, LatestDraft, LocalStore, PersistenceContext, StashRequest},
};

#[derive(Clone)]
pub struct StashJob {
  pub context: PersistenceContext,
  pub generation_id: GenerationId,
  pub snapshot: DocumentSnapshot,
  pub prepared_background: PreparedBackground,
  pub requested_at: Instant,
}

impl StashJob {
  fn capture_sequence(&self) -> u64 {
    self.context.capture_sequence.unwrap_or_default()
  }

  fn stash_sequence(&self) -> u64 {
    self.context.stash_sequence.unwrap_or_default()
  }
}

pub enum DraftResult {
  Commit {
    context: PersistenceContext,
    generation_id: GenerationId,
    completed_at: Instant,
    is_latest: bool,
    result: Box<Result<LatestDraft, String>>,
  },
  DeleteIfGeneration {
    generation_id: GenerationId,
    result: Result<bool, String>,
  },
  DeleteLatest {
    result: Result<bool, String>,
  },
  ClearAll {
    result: Result<(), String>,
  },
}

#[derive(Debug, Error)]
pub enum DraftCoordinatorError {
  #[error("draft coordinator is shutting down")]
  ShuttingDown,
  #[error("unable to start draft coordinator: {0}")]
  Spawn(#[source] std::io::Error),
}

pub struct DraftCoordinator {
  shared: Arc<Shared>,
  results: mpsc::Receiver<DraftResult>,
  done: mpsc::Receiver<()>,
  worker: Option<JoinHandle<()>>,
  stopped: bool,
}

struct Shared {
  state: Mutex<CoordinatorState>,
  changed: Condvar,
}

#[derive(Default)]
struct CoordinatorState {
  queue: VecDeque<DraftCommand>,
  latest_presented_capture_sequence: u64,
  latest_requested_stash_sequence: u64,
  processing: Option<Processing>,
  stop_when_drained: bool,
  abandon: bool,
}

enum DraftCommand {
  Commit(Box<StashJob>),
  DeleteIfGeneration(GenerationId),
  DeleteLatest,
  ClearAll,
}

struct Processing {
  capture_sequence: u64,
  stash_sequence: u64,
  prepared_background: PreparedBackground,
  phase: ProcessingPhase,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcessingPhase {
  WaitingForBackground,
  Committing,
}

impl DraftCoordinator {
  pub fn new(
    store: LocalStore,
    wake: impl Fn() + Send + Sync + 'static,
  ) -> Result<Self, DraftCoordinatorError> {
    let shared =
      Arc::new(Shared { state: Mutex::new(CoordinatorState::default()), changed: Condvar::new() });
    let (result_sender, results) = mpsc::channel();
    let (done_sender, done) = mpsc::sync_channel(1);
    let worker_shared = Arc::clone(&shared);
    let wake = Arc::new(wake);
    let worker = thread::Builder::new()
      .name("draft-coordinator".into())
      .spawn(move || {
        run_worker(worker_shared, store, result_sender, wake);
        let _ = done_sender.try_send(());
      })
      .map_err(DraftCoordinatorError::Spawn)?;
    Ok(Self { shared, results, done, worker: Some(worker), stopped: false })
  }

  pub fn publish_capture(&self, capture_sequence: u64) {
    let mut state = lock_unpoisoned(&self.shared.state);
    if capture_sequence <= state.latest_presented_capture_sequence {
      return;
    }
    state.latest_presented_capture_sequence = capture_sequence;
    state.queue.retain(|command| match command {
      DraftCommand::Commit(job) if job.capture_sequence() < capture_sequence => {
        job.prepared_background.supersede();
        false
      }
      _ => true,
    });
    if let Some(processing) = &state.processing
      && processing.phase == ProcessingPhase::WaitingForBackground
      && processing.capture_sequence < capture_sequence
    {
      processing.prepared_background.supersede();
    }
    self.shared.changed.notify_all();
  }

  pub fn enqueue_commit(&self, job: StashJob) -> Result<(), DraftCoordinatorError> {
    let mut state = lock_unpoisoned(&self.shared.state);
    ensure_accepting(&state)?;
    let capture_sequence = job.capture_sequence();
    let stash_sequence = job.stash_sequence();
    if capture_sequence < state.latest_presented_capture_sequence
      || stash_sequence < state.latest_requested_stash_sequence
    {
      job.prepared_background.supersede();
      return Ok(());
    }
    state.latest_requested_stash_sequence = stash_sequence;
    state.queue.retain(|command| match command {
      DraftCommand::Commit(queued) if queued.stash_sequence() < stash_sequence => {
        queued.prepared_background.supersede();
        false
      }
      _ => true,
    });
    if let Some(processing) = &state.processing
      && processing.phase == ProcessingPhase::WaitingForBackground
      && (processing.capture_sequence < capture_sequence
        || processing.stash_sequence < stash_sequence)
    {
      processing.prepared_background.supersede();
    }
    state.queue.push_back(DraftCommand::Commit(Box::new(job)));
    self.shared.changed.notify_one();
    Ok(())
  }

  pub fn delete_if_generation(
    &self,
    generation_id: GenerationId,
  ) -> Result<(), DraftCoordinatorError> {
    self.enqueue_command(DraftCommand::DeleteIfGeneration(generation_id))
  }

  pub fn delete_latest(&self) -> Result<(), DraftCoordinatorError> {
    self.enqueue_command(DraftCommand::DeleteLatest)
  }

  pub fn clear_all(&self) -> Result<(), DraftCoordinatorError> {
    self.enqueue_command(DraftCommand::ClearAll)
  }

  fn enqueue_command(&self, command: DraftCommand) -> Result<(), DraftCoordinatorError> {
    let mut state = lock_unpoisoned(&self.shared.state);
    ensure_accepting(&state)?;
    state.queue.push_back(command);
    self.shared.changed.notify_one();
    Ok(())
  }

  pub fn try_recv(&self) -> Option<DraftResult> {
    self.results.try_recv().ok()
  }

  pub fn shutdown(&mut self, timeout: Duration) -> bool {
    if self.stopped {
      return true;
    }
    {
      let mut state = lock_unpoisoned(&self.shared.state);
      state.stop_when_drained = true;
      self.shared.changed.notify_all();
    }
    let finished = self.done.recv_timeout(timeout).is_ok();
    if finished {
      if let Some(worker) = self.worker.take() {
        let _ = worker.join();
      }
      self.stopped = true;
      true
    } else {
      let mut state = lock_unpoisoned(&self.shared.state);
      state.abandon = true;
      state.queue.clear();
      self.shared.changed.notify_all();
      self.worker.take();
      self.stopped = true;
      false
    }
  }
}

impl Drop for DraftCoordinator {
  fn drop(&mut self) {
    if !self.stopped {
      let _ = self.shutdown(Duration::from_secs(2));
    }
  }
}

fn ensure_accepting(state: &CoordinatorState) -> Result<(), DraftCoordinatorError> {
  if state.stop_when_drained || state.abandon {
    Err(DraftCoordinatorError::ShuttingDown)
  } else {
    Ok(())
  }
}

fn run_worker(
  shared: Arc<Shared>,
  store: LocalStore,
  results: mpsc::Sender<DraftResult>,
  wake: Arc<dyn Fn() + Send + Sync>,
) {
  loop {
    let command = {
      let mut state = lock_unpoisoned(&shared.state);
      while state.queue.is_empty() && !state.stop_when_drained && !state.abandon {
        state = wait_unpoisoned(&shared.changed, state);
      }
      if state.abandon || (state.stop_when_drained && state.queue.is_empty()) {
        return;
      }
      state.queue.pop_front()
    };
    let Some(command) = command else {
      continue;
    };

    let result = match command {
      DraftCommand::Commit(job) => process_commit(&shared, &store, *job),
      DraftCommand::DeleteIfGeneration(generation_id) => DraftResult::DeleteIfGeneration {
        generation_id,
        result: store.delete_latest_if_generation(generation_id).map_err(|error| error.to_string()),
      },
      DraftCommand::DeleteLatest => DraftResult::DeleteLatest {
        result: store.delete_latest_draft().map_err(|error| error.to_string()),
      },
      DraftCommand::ClearAll => DraftResult::ClearAll {
        result: store.clear_all_content().map_err(|error| error.to_string()),
      },
    };
    {
      let mut state = lock_unpoisoned(&shared.state);
      state.processing = None;
      if state.abandon {
        return;
      }
    }
    if results.send(result).is_err() {
      return;
    }
    wake();
  }
}

fn process_commit(shared: &Shared, store: &LocalStore, job: StashJob) -> DraftResult {
  let capture_sequence = job.capture_sequence();
  let stash_sequence = job.stash_sequence();
  let context = job.context;
  let generation_id = job.generation_id;
  let requested_at = job.requested_at;
  let pixel_size = job.prepared_background.pixel_size();
  let performance = performance_context(&job);
  {
    let mut state = lock_unpoisoned(&shared.state);
    state.processing = Some(Processing {
      capture_sequence,
      stash_sequence,
      prepared_background: job.prepared_background.clone(),
      phase: ProcessingPhase::WaitingForBackground,
    });
    if !job_is_current(&state, capture_sequence, stash_sequence) {
      job.prepared_background.supersede();
    }
  }

  let wait_timer = PerformanceTimer::start(
    "stash.background.wait",
    performance,
    PerformanceDetails::default()
      .workflow("stash")
      .pixel_size(job.prepared_background.pixel_size()),
  );
  let background = match job.prepared_background.wait() {
    Ok(background) => {
      wait_timer.finish_ok();
      background
    }
    Err(error) => {
      wait_timer.finish_error(&error);
      return commit_result(
        shared,
        context,
        generation_id,
        capture_sequence,
        stash_sequence,
        Err(error.to_string()),
      );
    }
  };

  let should_commit = {
    let mut state = lock_unpoisoned(&shared.state);
    if state.abandon || !job_is_current(&state, capture_sequence, stash_sequence) {
      false
    } else {
      if let Some(processing) = state.processing.as_mut() {
        processing.phase = ProcessingPhase::Committing;
      }
      true
    }
  };
  if !should_commit {
    return commit_result(
      shared,
      context,
      generation_id,
      capture_sequence,
      stash_sequence,
      Err("draft job was superseded before atomic commit".into()),
    );
  }

  let timer = PerformanceTimer::started_at(
    "stash.request.total",
    performance,
    PerformanceDetails::default().workflow("stash").pixel_size(pixel_size),
    requested_at,
  );
  let result = store.replace_latest_draft(StashRequest {
    context,
    generation_id,
    snapshot: job.snapshot,
    background,
  });
  match &result {
    Ok(_) => timer.finish_ok(),
    Err(error) => timer.finish_error(error),
  }
  commit_result(
    shared,
    context,
    generation_id,
    capture_sequence,
    stash_sequence,
    result.map_err(|error| error.to_string()),
  )
}

fn commit_result(
  shared: &Shared,
  context: PersistenceContext,
  generation_id: GenerationId,
  capture_sequence: u64,
  stash_sequence: u64,
  result: Result<LatestDraft, String>,
) -> DraftResult {
  let state = lock_unpoisoned(&shared.state);
  DraftResult::Commit {
    context,
    generation_id,
    completed_at: Instant::now(),
    is_latest: job_is_current(&state, capture_sequence, stash_sequence),
    result: Box::new(result),
  }
}

fn job_is_current(state: &CoordinatorState, capture_sequence: u64, stash_sequence: u64) -> bool {
  capture_sequence == state.latest_presented_capture_sequence
    && stash_sequence == state.latest_requested_stash_sequence
}

fn performance_context(job: &StashJob) -> PerformanceContext {
  PerformanceContext {
    request_id: Some(job.context.request_id),
    session_id: Some(job.context.session_id),
    capture_sequence: job.context.capture_sequence,
    stash_sequence: job.context.stash_sequence,
    generation_id: Some(job.generation_id.as_uuid()),
    document_id: Some(job.snapshot.document_id.as_uuid()),
    revision: Some(job.snapshot.revision),
  }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_unpoisoned<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
  condvar.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
  use std::{path::Path, time::Duration};

  use chrono::{TimeZone, Utc};
  use common::{BoardDocument, CapturedDisplay, DocumentId, GlobalBoundsPx, SizePx};
  use uuid::Uuid;

  use super::*;
  use crate::storage::BackgroundData;

  struct TestDirectory(std::path::PathBuf);

  impl TestDirectory {
    fn new(name: &str) -> Self {
      let path = std::env::temp_dir().join(format!("rs-board-draft-{name}-{}", Uuid::new_v4()));
      std::fs::create_dir(&path).unwrap();
      Self(path)
    }

    fn path(&self) -> &Path {
      &self.0
    }
  }

  impl Drop for TestDirectory {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.0);
    }
  }

  fn store(name: &str) -> (TestDirectory, LocalStore) {
    let root = TestDirectory::new(name);
    let (store, _) = LocalStore::at_root(root.path()).unwrap();
    (root, store)
  }

  fn job(capture_sequence: u64, stash_sequence: u64, generation_id: GenerationId) -> StashJob {
    let document_id = DocumentId::new();
    let document = BoardDocument::new_capture(
      document_id,
      SizePx::new(2, 2),
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx { x_px: 0, y_px: 0, width_px: 2, height_px: 2 },
        scale_factor: 1.0,
      },
      Utc.with_ymd_and_hms(2026, 8, 8, 12, 0, 0).unwrap(),
    )
    .unwrap();
    let snapshot = document.snapshot(document.revision).unwrap();
    let background = BackgroundData::rgba8(2, 2, vec![capture_sequence as u8; 16]).unwrap();
    StashJob {
      context: PersistenceContext::new(Uuid::new_v4(), Uuid::new_v4())
        .with_sequences(Some(capture_sequence), Some(stash_sequence))
        .with_generation(generation_id),
      generation_id,
      snapshot,
      prepared_background: PreparedBackground::ready(capture_sequence, background).unwrap(),
      requested_at: Instant::now(),
    }
  }

  fn wait_result(coordinator: &DraftCoordinator) -> DraftResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      if let Some(result) = coordinator.try_recv() {
        return result;
      }
      assert!(Instant::now() < deadline, "timed out waiting for draft result");
      thread::sleep(Duration::from_millis(5));
    }
  }

  #[test]
  fn latest_job_wins_and_old_generation_delete_cannot_remove_it() {
    let (_root, store) = store("latest-wins");
    let mut coordinator = DraftCoordinator::new(store.clone(), || {}).unwrap();
    let first_generation = GenerationId::new();
    let second_generation = GenerationId::new();

    coordinator.publish_capture(1);
    coordinator.enqueue_commit(job(1, 1, first_generation)).unwrap();
    coordinator.publish_capture(2);
    coordinator.enqueue_commit(job(2, 2, second_generation)).unwrap();

    loop {
      if let DraftResult::Commit { context, result, .. } = wait_result(&coordinator)
        && context.capture_sequence == Some(2)
      {
        assert!(result.is_ok());
        break;
      }
    }
    assert_eq!(store.load_latest_draft().unwrap().unwrap().generation_id, second_generation);

    coordinator.delete_if_generation(first_generation).unwrap();
    assert!(matches!(
      wait_result(&coordinator),
      DraftResult::DeleteIfGeneration { result: Ok(false), .. }
    ));
    assert_eq!(store.load_latest_draft().unwrap().unwrap().generation_id, second_generation);
    assert!(coordinator.shutdown(Duration::from_secs(2)));
  }

  #[test]
  fn shutdown_wait_is_bounded_and_abandons_a_job_still_waiting_for_encoding() {
    let (_root, store) = store("shutdown-timeout");
    let mut coordinator = DraftCoordinator::new(store, || {}).unwrap();
    let generation_id = GenerationId::new();
    let mut pending_job = job(1, 1, generation_id);
    let pending = PreparedBackground::pending_for_test(1, [2, 2]);
    pending_job.prepared_background = pending.clone();
    coordinator.publish_capture(1);
    coordinator.enqueue_commit(pending_job).unwrap();

    let started_at = Instant::now();
    assert!(!coordinator.shutdown(Duration::from_millis(20)));
    assert!(started_at.elapsed() < Duration::from_millis(250));
    pending.supersede();
    thread::sleep(Duration::from_millis(20));
  }
}
