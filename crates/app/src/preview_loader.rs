use std::{
  collections::{HashMap, HashSet, VecDeque},
  io::BufReader,
  path::PathBuf,
  sync::{Arc, Condvar, Mutex, MutexGuard, mpsc},
  thread::{self, JoinHandle},
};

use common::{DocumentId, Revision};
use thiserror::Error;

use crate::{
  performance::{PerformanceContext, PerformanceDetails, PerformanceTimer},
  storage::open_regular_path,
};

const PREVIEW_WORKER_COUNT: usize = 2;
const MAX_PENDING_PREVIEWS: usize = 32;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PreviewKey {
  pub document_id: DocumentId,
  pub revision: Revision,
  pub path: PathBuf,
  pub target_size_px: [u32; 2],
}

impl PreviewKey {
  pub fn new(
    document_id: DocumentId,
    revision: Revision,
    path: impl Into<PathBuf>,
    target_size_px: [u32; 2],
  ) -> Self {
    Self { document_id, revision, path: path.into(), target_size_px }
  }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PreviewRequestToken(u64);

impl PreviewRequestToken {
  pub const fn get(self) -> u64 {
    self.0
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreviewTicket {
  pub key: PreviewKey,
  pub token: PreviewRequestToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PreviewEnqueueOutcome {
  Queued(PreviewTicket),
  AlreadyTracked(PreviewTicket),
  QueueFull(PreviewKey),
}

impl PreviewEnqueueOutcome {
  pub fn ticket(&self) -> Option<&PreviewTicket> {
    match self {
      Self::Queued(ticket) | Self::AlreadyTracked(ticket) => Some(ticket),
      Self::QueueFull(_) => None,
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedPreview {
  pub width_px: u32,
  pub height_px: u32,
  pub rgba: Arc<[u8]>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(crate) enum PreviewLoadError {
  #[error("preview target size must be non-zero")]
  InvalidTargetSize,
  #[error("unable to decode preview: {0}")]
  Decode(Arc<str>),
}

#[derive(Clone, Debug)]
pub(crate) struct PreviewLoadResult {
  pub ticket: PreviewTicket,
  pub image: Result<DecodedPreview, PreviewLoadError>,
}

#[derive(Debug, Error)]
pub enum PreviewLoaderError {
  #[error("unable to start preview loader: {0}")]
  Spawn(#[source] std::io::Error),
}

pub(crate) struct PreviewLoader {
  shared: Arc<Shared>,
  results: mpsc::Receiver<PreviewLoadResult>,
  workers: Vec<JoinHandle<()>>,
}

struct Shared {
  state: Mutex<LoaderState>,
  changed: Condvar,
  decoder: Arc<Decoder>,
  wake: Arc<dyn Fn() + Send + Sync>,
}

type Decoder = dyn Fn(&PreviewKey) -> Result<DecodedPreview, PreviewLoadError> + Send + Sync;

struct LoaderState {
  pending: VecDeque<PreviewTicket>,
  tracked: HashMap<PreviewKey, TrackedRequest>,
  next_token: u64,
  shutting_down: bool,
}

impl Default for LoaderState {
  fn default() -> Self {
    Self { pending: VecDeque::new(), tracked: HashMap::new(), next_token: 1, shutting_down: false }
  }
}

#[derive(Clone)]
struct TrackedRequest {
  ticket: PreviewTicket,
  phase: RequestPhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestPhase {
  Pending,
  InFlight,
  Completed,
}

impl PreviewLoader {
  pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Result<Self, PreviewLoaderError> {
    Self::start(Arc::new(decode_preview), Arc::new(wake))
  }

  fn start(
    decoder: Arc<Decoder>,
    wake: Arc<dyn Fn() + Send + Sync>,
  ) -> Result<Self, PreviewLoaderError> {
    let shared = Arc::new(Shared {
      state: Mutex::new(LoaderState::default()),
      changed: Condvar::new(),
      decoder,
      wake,
    });
    let (result_sender, results) = mpsc::sync_channel(MAX_PENDING_PREVIEWS);
    let mut workers = Vec::with_capacity(PREVIEW_WORKER_COUNT);

    for index in 0..PREVIEW_WORKER_COUNT {
      let worker_shared = Arc::clone(&shared);
      let worker_sender = result_sender.clone();
      let spawn = thread::Builder::new()
        .name(format!("preview-loader-{index}"))
        .spawn(move || run_worker(worker_shared, worker_sender));
      match spawn {
        Ok(worker) => workers.push(worker),
        Err(error) => {
          stop_workers(&shared, &mut workers);
          return Err(PreviewLoaderError::Spawn(error));
        }
      }
    }
    drop(result_sender);

    Ok(Self { shared, results, workers })
  }

  #[cfg(test)]
  fn with_decoder(
    decoder: Arc<Decoder>,
    wake: impl Fn() + Send + Sync + 'static,
  ) -> Result<Self, PreviewLoaderError> {
    Self::start(decoder, Arc::new(wake))
  }

  #[cfg(test)]
  pub fn request(&self, key: PreviewKey) -> PreviewEnqueueOutcome {
    let mut state = lock_unpoisoned(&self.shared.state);
    let outcome = enqueue_locked(&mut state, key);
    if matches!(outcome, PreviewEnqueueOutcome::Queued(_)) {
      self.shared.changed.notify_one();
    }
    outcome
  }

  /// Replaces the current viewport and prefetch set.
  ///
  /// Pending work outside `desired` is evicted. In-flight work is allowed to finish, and its
  /// ticket lets the caller reject a late result. Desired keys are deduplicated in input order and
  /// enqueued until the pending queue reaches its bound.
  pub fn update_desired(
    &self,
    desired: impl IntoIterator<Item = PreviewKey>,
  ) -> Vec<PreviewEnqueueOutcome> {
    let mut ordered = Vec::new();
    let mut desired_set = HashSet::new();
    for key in desired {
      if desired_set.insert(key.clone()) {
        ordered.push(key);
      }
    }

    let mut state = lock_unpoisoned(&self.shared.state);
    let mut evicted = Vec::new();
    state.pending.retain(|ticket| {
      let keep = desired_set.contains(&ticket.key);
      if !keep {
        evicted.push(ticket.clone());
      }
      keep
    });
    for ticket in evicted {
      if state.tracked.get(&ticket.key).is_some_and(|tracked| {
        tracked.ticket.token == ticket.token && tracked.phase == RequestPhase::Pending
      }) {
        state.tracked.remove(&ticket.key);
      }
    }

    let outcomes: Vec<_> = ordered.into_iter().map(|key| enqueue_locked(&mut state, key)).collect();
    if outcomes.iter().any(|outcome| matches!(outcome, PreviewEnqueueOutcome::Queued(_))) {
      self.shared.changed.notify_all();
    }
    outcomes
  }

  pub fn try_recv(&self) -> Option<PreviewLoadResult> {
    let result = self.results.try_recv().ok()?;
    let mut state = lock_unpoisoned(&self.shared.state);
    if state.tracked.get(&result.ticket.key).is_some_and(|tracked| {
      tracked.ticket.token == result.ticket.token && tracked.phase == RequestPhase::Completed
    }) {
      state.tracked.remove(&result.ticket.key);
    }
    Some(result)
  }
}

impl Drop for PreviewLoader {
  fn drop(&mut self) {
    stop_workers(&self.shared, &mut self.workers);
  }
}

fn enqueue_locked(state: &mut LoaderState, key: PreviewKey) -> PreviewEnqueueOutcome {
  if let Some(tracked) = state.tracked.get(&key) {
    return PreviewEnqueueOutcome::AlreadyTracked(tracked.ticket.clone());
  }
  if state.pending.len() == MAX_PENDING_PREVIEWS {
    return PreviewEnqueueOutcome::QueueFull(key);
  }

  let token = PreviewRequestToken(next_token(&mut state.next_token));
  let ticket = PreviewTicket { key: key.clone(), token };
  state.pending.push_back(ticket.clone());
  state
    .tracked
    .insert(key, TrackedRequest { ticket: ticket.clone(), phase: RequestPhase::Pending });
  PreviewEnqueueOutcome::Queued(ticket)
}

fn next_token(next: &mut u64) -> u64 {
  let token = *next;
  *next = next.wrapping_add(1).max(1);
  token
}

fn run_worker(shared: Arc<Shared>, results: mpsc::SyncSender<PreviewLoadResult>) {
  loop {
    let ticket = {
      let mut state = lock_unpoisoned(&shared.state);
      while state.pending.is_empty() && !state.shutting_down {
        state = wait_unpoisoned(&shared.changed, state);
      }
      if state.shutting_down {
        return;
      }
      let Some(ticket) = state.pending.pop_front() else {
        continue;
      };
      let Some(tracked) = state.tracked.get_mut(&ticket.key) else {
        continue;
      };
      if tracked.ticket.token != ticket.token || tracked.phase != RequestPhase::Pending {
        continue;
      }
      tracked.phase = RequestPhase::InFlight;
      ticket
    };

    let decode_timer = PerformanceTimer::start(
      "library.preview_decode",
      PerformanceContext {
        document_id: Some(ticket.key.document_id.as_uuid()),
        revision: Some(ticket.key.revision),
        ..PerformanceContext::default()
      },
      PerformanceDetails::default().pixel_size(ticket.key.target_size_px),
    );
    let image = (shared.decoder)(&ticket.key);
    match &image {
      Ok(_) => decode_timer.finish_ok(),
      Err(error) => decode_timer.finish_error(error),
    }
    {
      let mut state = lock_unpoisoned(&shared.state);
      if state.shutting_down {
        return;
      }
      if let Some(tracked) = state.tracked.get_mut(&ticket.key)
        && tracked.ticket.token == ticket.token
        && tracked.phase == RequestPhase::InFlight
      {
        tracked.phase = RequestPhase::Completed;
      }
    }
    match results.try_send(PreviewLoadResult { ticket: ticket.clone(), image }) {
      Ok(()) => {}
      Err(mpsc::TrySendError::Full(_)) => {
        let mut state = lock_unpoisoned(&shared.state);
        if state.tracked.get(&ticket.key).is_some_and(|tracked| {
          tracked.ticket.token == ticket.token && tracked.phase == RequestPhase::Completed
        }) {
          state.tracked.remove(&ticket.key);
        }
      }
      Err(mpsc::TrySendError::Disconnected(_)) => return,
    }
    (shared.wake)();
  }
}

fn stop_workers(shared: &Shared, workers: &mut Vec<JoinHandle<()>>) {
  {
    let mut state = lock_unpoisoned(&shared.state);
    state.shutting_down = true;
    state.pending.clear();
    state.tracked.clear();
    shared.changed.notify_all();
  }
  for worker in workers.drain(..) {
    let _ = worker.join();
  }
}

fn decode_preview(key: &PreviewKey) -> Result<DecodedPreview, PreviewLoadError> {
  let [target_width, target_height] = key.target_size_px;
  if target_width == 0 || target_height == 0 {
    return Err(PreviewLoadError::InvalidTargetSize);
  }
  let file = open_regular_path(&key.path)
    .map_err(|error| PreviewLoadError::Decode(Arc::from(error.to_string())))?;
  let reader = image::ImageReader::new(BufReader::new(file))
    .with_guessed_format()
    .map_err(|error| PreviewLoadError::Decode(Arc::from(error.to_string())))?;
  let image =
    reader.decode().map_err(|error| PreviewLoadError::Decode(Arc::from(error.to_string())))?;
  let image = if image.width() > target_width || image.height() > target_height {
    image.thumbnail(target_width, target_height)
  } else {
    image
  };
  let image = image.into_rgba8();
  Ok(DecodedPreview {
    width_px: image.width(),
    height_px: image.height(),
    rgba: Arc::from(image.into_raw()),
  })
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_unpoisoned<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
  condvar.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
  use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
  };

  use image::{Rgba, RgbaImage};

  use super::*;

  struct TestDirectory(PathBuf);

  impl TestDirectory {
    fn new() -> Self {
      let path =
        std::env::temp_dir().join(format!("rs-board-preview-loader-{}", DocumentId::new()));
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

  #[derive(Default)]
  struct DecoderGate {
    state: Mutex<GateState>,
    changed: Condvar,
  }

  #[derive(Default)]
  struct GateState {
    started: usize,
    released: bool,
  }

  impl DecoderGate {
    fn decoder(self: &Arc<Self>) -> Arc<Decoder> {
      let gate = Arc::clone(self);
      Arc::new(move |_| {
        let mut state = lock_unpoisoned(&gate.state);
        state.started += 1;
        gate.changed.notify_all();
        while !state.released {
          state = wait_unpoisoned(&gate.changed, state);
        }
        Ok(DecodedPreview { width_px: 1, height_px: 1, rgba: Arc::from([0, 0, 0, 255]) })
      })
    }

    fn wait_until_started(&self, expected: usize) {
      let deadline = Instant::now() + Duration::from_secs(2);
      let mut state = lock_unpoisoned(&self.state);
      while state.started < expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "preview decoder did not start in time");
        let waited = self
          .changed
          .wait_timeout(state, remaining)
          .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = waited.0;
      }
    }

    fn release(&self) {
      let mut state = lock_unpoisoned(&self.state);
      state.released = true;
      self.changed.notify_all();
    }

    fn started(&self) -> usize {
      lock_unpoisoned(&self.state).started
    }
  }

  fn key(label: u64) -> PreviewKey {
    PreviewKey::new(
      DocumentId::new(),
      label,
      PathBuf::from(format!("preview-{label}.png")),
      [144, 81],
    )
  }

  fn queued(outcome: PreviewEnqueueOutcome) -> PreviewTicket {
    match outcome {
      PreviewEnqueueOutcome::Queued(ticket) => ticket,
      other => panic!("expected queued preview, got {other:?}"),
    }
  }

  fn receive(loader: &PreviewLoader, count: usize) -> Vec<PreviewLoadResult> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut results = Vec::new();
    while results.len() < count {
      if let Some(result) = loader.try_recv() {
        results.push(result);
      } else {
        assert!(Instant::now() < deadline, "preview results did not arrive in time");
        thread::sleep(Duration::from_millis(1));
      }
    }
    results
  }

  fn pending_keys(loader: &PreviewLoader) -> Vec<PreviewKey> {
    lock_unpoisoned(&loader.shared.state).pending.iter().map(|ticket| ticket.key.clone()).collect()
  }

  fn wait_until(mut predicate: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !predicate() {
      assert!(Instant::now() < deadline, "{message}");
      thread::sleep(Duration::from_millis(1));
    }
  }

  #[test]
  fn identical_requests_are_deduplicated() {
    let gate = Arc::new(DecoderGate::default());
    let loader = PreviewLoader::with_decoder(gate.decoder(), || {}).unwrap();
    let key = key(1);
    let first = queued(loader.request(key.clone()));
    let duplicate = loader.request(key);

    assert_eq!(duplicate.ticket(), Some(&first));
    assert!(matches!(duplicate, PreviewEnqueueOutcome::AlreadyTracked(_)));
    gate.release();
    let results = receive(&loader, 1);
    assert_eq!(results[0].ticket, first);
    assert_eq!(gate.started(), 1);
    assert!(loader.try_recv().is_none());
  }

  #[test]
  fn pending_queue_is_bounded_at_32_requests() {
    let gate = Arc::new(DecoderGate::default());
    let loader = PreviewLoader::with_decoder(gate.decoder(), || {}).unwrap();
    queued(loader.request(key(1)));
    queued(loader.request(key(2)));
    gate.wait_until_started(PREVIEW_WORKER_COUNT);

    for label in 3..(3 + MAX_PENDING_PREVIEWS as u64) {
      queued(loader.request(key(label)));
    }
    assert_eq!(pending_keys(&loader).len(), MAX_PENDING_PREVIEWS);
    assert!(matches!(loader.request(key(99)), PreviewEnqueueOutcome::QueueFull(_)));
    assert_eq!(pending_keys(&loader).len(), MAX_PENDING_PREVIEWS);
    gate.release();
  }

  #[test]
  fn completed_result_channel_is_bounded_and_full_results_can_be_requeued() {
    let gate = Arc::new(DecoderGate::default());
    let loader = PreviewLoader::with_decoder(gate.decoder(), || {}).unwrap();
    let keys: Vec<_> =
      (1..=(MAX_PENDING_PREVIEWS + PREVIEW_WORKER_COUNT) as u64).map(key).collect();
    for preview_key in keys.iter().take(PREVIEW_WORKER_COUNT) {
      queued(loader.request(preview_key.clone()));
    }
    gate.wait_until_started(PREVIEW_WORKER_COUNT);
    for preview_key in keys.iter().skip(PREVIEW_WORKER_COUNT) {
      queued(loader.request(preview_key.clone()));
    }

    gate.release();
    gate.wait_until_started(keys.len());
    wait_until(
      || {
        let state = lock_unpoisoned(&loader.shared.state);
        state.pending.is_empty()
          && state.tracked.len() == MAX_PENDING_PREVIEWS
          && state.tracked.values().all(|request| request.phase == RequestPhase::Completed)
      },
      "preview workers did not settle after filling the result channel",
    );

    let tracked: HashSet<_> =
      lock_unpoisoned(&loader.shared.state).tracked.keys().cloned().collect();
    assert_eq!(tracked.len(), MAX_PENDING_PREVIEWS);
    let dropped: Vec<_> =
      keys.into_iter().filter(|preview_key| !tracked.contains(preview_key)).collect();
    assert_eq!(dropped.len(), PREVIEW_WORKER_COUNT);
    for preview_key in dropped {
      assert!(matches!(loader.request(preview_key), PreviewEnqueueOutcome::Queued(_)));
    }
  }

  #[test]
  fn rapid_desired_updates_keep_only_the_latest_unstarted_window() {
    let gate = Arc::new(DecoderGate::default());
    let loader = PreviewLoader::with_decoder(gate.decoder(), || {}).unwrap();
    queued(loader.request(key(1)));
    queued(loader.request(key(2)));
    gate.wait_until_started(PREVIEW_WORKER_COUNT);

    let first_window: Vec<_> = (10..16).map(key).collect();
    let first_outcomes = loader.update_desired(first_window.clone());
    let first_token = first_outcomes[0].ticket().unwrap().token;
    assert_eq!(pending_keys(&loader), first_window);

    let second_window: Vec<_> = (20..26).map(key).collect();
    loader.update_desired(second_window.clone());
    assert_eq!(pending_keys(&loader), second_window);

    let third_window: Vec<_> = (30..36).map(key).collect();
    let mut duplicated_third_window = third_window.clone();
    duplicated_third_window.push(third_window[0].clone());
    let third_outcomes = loader.update_desired(duplicated_third_window);
    assert_eq!(third_outcomes.len(), third_window.len());
    assert_eq!(pending_keys(&loader), third_window);

    let requeued = queued(loader.request(first_window[0].clone()));
    assert_ne!(requeued.token, first_token);
    gate.release();
  }

  #[test]
  fn desired_update_evicts_pending_but_not_in_flight_requests() {
    let gate = Arc::new(DecoderGate::default());
    let loader = PreviewLoader::with_decoder(gate.decoder(), || {}).unwrap();
    let active = key(1);
    queued(loader.request(active.clone()));
    queued(loader.request(key(2)));
    gate.wait_until_started(PREVIEW_WORKER_COUNT);

    let keep = key(3);
    let evict = key(4);
    let replacement = key(5);
    let keep_ticket = queued(loader.request(keep.clone()));
    let evicted_ticket = queued(loader.request(evict.clone()));
    let outcomes = loader.update_desired([keep, replacement]);

    assert_eq!(outcomes[0].ticket(), Some(&keep_ticket));
    assert!(matches!(outcomes[0], PreviewEnqueueOutcome::AlreadyTracked(_)));
    assert!(matches!(outcomes[1], PreviewEnqueueOutcome::Queued(_)));
    assert!(matches!(loader.request(active), PreviewEnqueueOutcome::AlreadyTracked(_)));
    let requeued = queued(loader.request(evict));
    assert_ne!(requeued.token, evicted_ticket.token);
    gate.release();
  }

  #[test]
  fn successful_and_failed_decodes_are_both_returned() {
    let directory = TestDirectory::new();
    let image_path = directory.path().join("preview.png");
    RgbaImage::from_pixel(400, 200, Rgba([11, 22, 33, 255])).save(&image_path).unwrap();
    let wakes = Arc::new(AtomicUsize::new(0));
    let loader = PreviewLoader::new({
      let wakes = Arc::clone(&wakes);
      move || {
        wakes.fetch_add(1, Ordering::SeqCst);
      }
    })
    .unwrap();

    let success_key = PreviewKey::new(DocumentId::new(), 1, image_path, [100, 100]);
    let failure_key =
      PreviewKey::new(DocumentId::new(), 1, directory.path().join("missing.png"), [100, 100]);
    loader.update_desired([success_key.clone(), failure_key.clone()]);
    let results = receive(&loader, 2);

    let success = results.iter().find(|result| result.ticket.key == success_key).unwrap();
    let image = success.image.as_ref().unwrap();
    assert_eq!([image.width_px, image.height_px], [100, 50]);
    assert_eq!(image.rgba.len(), 100 * 50 * 4);
    assert_eq!(&image.rgba[..4], &[11, 22, 33, 255]);
    let failure = results.iter().find(|result| result.ticket.key == failure_key).unwrap();
    assert!(matches!(failure.image, Err(PreviewLoadError::Decode(_))));
    assert_eq!(wakes.load(Ordering::SeqCst), 2);
  }

  #[cfg(unix)]
  #[test]
  fn decoder_rejects_a_symlink_preview() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new();
    let image_path = directory.path().join("preview.png");
    let symlink_path = directory.path().join("linked-preview.png");
    RgbaImage::from_pixel(2, 2, Rgba([11, 22, 33, 255])).save(&image_path).unwrap();
    symlink(&image_path, &symlink_path).unwrap();

    let error = decode_preview(&PreviewKey::new(DocumentId::new(), 1, symlink_path, [2, 2]))
      .expect_err("preview symlinks must not be followed");

    assert!(matches!(error, PreviewLoadError::Decode(_)));
  }

  #[test]
  fn duplicate_failed_request_decodes_and_reports_only_once() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(AtomicUsize::new(0));
    let decoder: Arc<Decoder> = {
      let attempts = Arc::clone(&attempts);
      Arc::new(move |_| {
        attempts.fetch_add(1, Ordering::SeqCst);
        Err(PreviewLoadError::Decode(Arc::from("test decode failure")))
      })
    };
    let loader = PreviewLoader::with_decoder(decoder, {
      let wakes = Arc::clone(&wakes);
      move || {
        wakes.fetch_add(1, Ordering::SeqCst);
      }
    })
    .unwrap();
    let failed_key = key(1);
    let ticket = queued(loader.request(failed_key.clone()));

    wait_until(|| wakes.load(Ordering::SeqCst) == 1, "failed preview result was not published");
    let duplicate = loader.request(failed_key.clone());
    assert_eq!(duplicate.ticket(), Some(&ticket));
    assert!(matches!(duplicate, PreviewEnqueueOutcome::AlreadyTracked(_)));
    let desired = loader.update_desired([failed_key.clone(), failed_key]);
    assert_eq!(desired.len(), 1);
    assert_eq!(desired[0].ticket(), Some(&ticket));

    let results = receive(&loader, 1);
    assert_eq!(results[0].ticket, ticket);
    assert!(matches!(results[0].image, Err(PreviewLoadError::Decode(_))));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert!(loader.try_recv().is_none());
  }

  #[test]
  fn late_in_flight_results_keep_their_original_ticket() {
    let gate = Arc::new(DecoderGate::default());
    let loader = PreviewLoader::with_decoder(gate.decoder(), || {}).unwrap();
    let document_id = DocumentId::new();
    let path = PathBuf::from("same-preview.png");
    let late_key = PreviewKey::new(document_id, 1, path.clone(), [144, 81]);
    let late_ticket = queued(loader.request(late_key.clone()));
    gate.wait_until_started(1);

    let current_key = PreviewKey::new(document_id, 2, path, [144, 81]);
    let current_outcomes = loader.update_desired([current_key.clone()]);
    let current_ticket = current_outcomes[0].ticket().unwrap().clone();
    assert_ne!(late_ticket.token, current_ticket.token);
    gate.release();

    let results = receive(&loader, 2);
    let late = results.iter().find(|result| result.ticket.key == late_key).unwrap();
    let current = results.iter().find(|result| result.ticket.key == current_key).unwrap();
    assert_eq!(late.ticket, late_ticket);
    assert_eq!(current.ticket, current_ticket);
    assert_ne!(late.ticket.token, current.ticket.token);
    assert_eq!(late.ticket.key.document_id, current.ticket.key.document_id);
    assert!(late.ticket.key.revision < current.ticket.key.revision);
  }
}
