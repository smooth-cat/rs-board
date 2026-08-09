use std::{
  collections::{HashMap, VecDeque},
  path::PathBuf,
  sync::{Arc, Condvar, Mutex, MutexGuard, mpsc},
  thread::{self, JoinHandle},
  time::Instant,
};

use common::{DocumentId, DocumentSnapshot, Revision};
use image::RgbaImage;
use thiserror::Error;

use crate::{
  background_encode::PreparedBackground,
  export::{copy_image, encode_png, make_preview},
  performance::{PerformanceContext, PerformanceDetails, PerformanceTimer},
  renderer::render_document_to_image,
  storage::LocalStore,
};

const MAX_PENDING_RENDERS: usize = 16;
const MAX_PENDING_CLIPBOARD_WRITES: usize = 32;

#[derive(Clone)]
pub struct PostSaveJob {
  pub document_id: DocumentId,
  pub revision: Revision,
  pub snapshot: Arc<DocumentSnapshot>,
  pub prepared_background: PreparedBackground,
  pub copy_to_clipboard: bool,
  pub performance: PerformanceContext,
  pub requested_at: Instant,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PostSaveEnqueueOutcome {
  pub clipboard_dropped: bool,
  pub render_evicted: bool,
}

pub enum PostSaveResult {
  ImageTasks {
    document_id: DocumentId,
    revision: Revision,
    preview_path: Option<PathBuf>,
    warnings: Vec<String>,
  },
}

#[derive(Debug, Error)]
pub enum PostSaveCoordinatorError {
  #[error("post-save coordinator is shutting down")]
  ShuttingDown,
  #[error("unable to start post-save coordinator: {0}")]
  Spawn(#[source] std::io::Error),
}

pub struct PostSaveCoordinator {
  shared: Arc<Shared>,
  results: mpsc::Receiver<PostSaveResult>,
  worker: Option<JoinHandle<()>>,
}

struct Shared {
  state: Mutex<CoordinatorState>,
  changed: Condvar,
}

#[derive(Default)]
struct CoordinatorState {
  render_order: VecDeque<DocumentId>,
  latest_render_by_document: HashMap<DocumentId, Arc<PostSaveJob>>,
  clipboard_queue: VecDeque<Arc<PostSaveJob>>,
  abandon: bool,
}

enum Work {
  Image { job: Arc<PostSaveJob>, install_preview: bool, copy_to_clipboard: bool },
}

impl CoordinatorState {
  fn enqueue(&mut self, job: PostSaveJob) -> PostSaveEnqueueOutcome {
    let job = Arc::new(job);
    let mut outcome = PostSaveEnqueueOutcome::default();
    let should_replace = self
      .latest_render_by_document
      .get(&job.document_id)
      .is_none_or(|pending| pending.revision <= job.revision);
    if should_replace {
      if !self.latest_render_by_document.contains_key(&job.document_id) {
        if self.latest_render_by_document.len() == MAX_PENDING_RENDERS
          && let Some(evicted) = self.render_order.pop_front()
        {
          self.latest_render_by_document.remove(&evicted);
          outcome.render_evicted = true;
        }
        self.render_order.push_back(job.document_id);
      }
      self.latest_render_by_document.insert(job.document_id, Arc::clone(&job));
    }

    if job.copy_to_clipboard {
      if self.clipboard_queue.len() == MAX_PENDING_CLIPBOARD_WRITES {
        outcome.clipboard_dropped = true;
      } else {
        self.clipboard_queue.push_back(Arc::clone(&job));
      }
    }
    outcome
  }

  fn next_work(&mut self) -> Option<Work> {
    if let Some(job) = self.clipboard_queue.pop_front() {
      let install_preview = self
        .latest_render_by_document
        .get(&job.document_id)
        .is_some_and(|pending| pending.revision == job.revision);
      if install_preview {
        self.latest_render_by_document.remove(&job.document_id);
        self.render_order.retain(|document_id| *document_id != job.document_id);
      }
      return Some(Work::Image { job, install_preview, copy_to_clipboard: true });
    }

    while let Some(document_id) = self.render_order.pop_front() {
      if let Some(job) = self.latest_render_by_document.remove(&document_id) {
        return Some(Work::Image { job, install_preview: true, copy_to_clipboard: false });
      }
    }
    None
  }

  fn has_work(&self) -> bool {
    !self.clipboard_queue.is_empty() || !self.latest_render_by_document.is_empty()
  }
}

impl PostSaveCoordinator {
  pub fn new(
    store: LocalStore,
    wake: impl Fn() + Send + Sync + 'static,
  ) -> Result<Self, PostSaveCoordinatorError> {
    Self::with_before_work(store, wake, Arc::new(|| {}))
  }

  fn with_before_work(
    store: LocalStore,
    wake: impl Fn() + Send + Sync + 'static,
    before_work: Arc<dyn Fn() + Send + Sync>,
  ) -> Result<Self, PostSaveCoordinatorError> {
    let shared =
      Arc::new(Shared { state: Mutex::new(CoordinatorState::default()), changed: Condvar::new() });
    let (result_sender, results) = mpsc::channel();
    let worker_shared = Arc::clone(&shared);
    let wake = Arc::new(wake);
    let worker = thread::Builder::new()
      .name("post-save-coordinator".into())
      .spawn(move || run_worker(worker_shared, store, result_sender, wake, before_work))
      .map_err(PostSaveCoordinatorError::Spawn)?;
    Ok(Self { shared, results, worker: Some(worker) })
  }

  pub fn enqueue(
    &self,
    job: PostSaveJob,
  ) -> Result<PostSaveEnqueueOutcome, PostSaveCoordinatorError> {
    let mut state = lock_unpoisoned(&self.shared.state);
    if state.abandon {
      return Err(PostSaveCoordinatorError::ShuttingDown);
    }
    let outcome = state.enqueue(job);
    self.shared.changed.notify_one();
    Ok(outcome)
  }

  pub fn try_recv(&self) -> Option<PostSaveResult> {
    self.results.try_recv().ok()
  }
}

impl Drop for PostSaveCoordinator {
  fn drop(&mut self) {
    {
      let mut state = lock_unpoisoned(&self.shared.state);
      state.abandon = true;
      state.render_order.clear();
      state.latest_render_by_document.clear();
      state.clipboard_queue.clear();
      self.shared.changed.notify_all();
    }
    self.worker.take();
  }
}

fn run_worker(
  shared: Arc<Shared>,
  store: LocalStore,
  results: mpsc::Sender<PostSaveResult>,
  wake: Arc<dyn Fn() + Send + Sync>,
  before_work: Arc<dyn Fn() + Send + Sync>,
) {
  loop {
    let work = {
      let mut state = lock_unpoisoned(&shared.state);
      while !state.has_work() && !state.abandon {
        state = wait_unpoisoned(&shared.changed, state);
      }
      if state.abandon {
        return;
      }
      state.next_work()
    };
    let Some(work) = work else {
      continue;
    };
    before_work();
    let result = match work {
      Work::Image { job, install_preview, copy_to_clipboard } => {
        process_image_tasks(&store, &job, install_preview, copy_to_clipboard)
      }
    };
    if results.send(result).is_err() {
      return;
    }
    wake();
  }
}

fn process_image_tasks(
  store: &LocalStore,
  job: &PostSaveJob,
  install_preview: bool,
  copy_to_clipboard: bool,
) -> PostSaveResult {
  let details = PerformanceDetails::default()
    .workflow("save")
    .pixel_size([job.snapshot.canvas_size_px.width_px, job.snapshot.canvas_size_px.height_px]);
  let total_timer =
    PerformanceTimer::started_at("post_save.total", job.performance, details, job.requested_at);
  let mut warnings = Vec::new();

  let background_timer =
    PerformanceTimer::start("post_save.background_decode", job.performance, details);
  let background = job
    .prepared_background
    .wait()
    .map_err(|error| error.to_string())
    .and_then(|background| background.decode_rgba8().map_err(|error| error.to_string()))
    .and_then(|(width_px, height_px, pixels)| {
      RgbaImage::from_raw(width_px, height_px, pixels.to_vec())
        .ok_or_else(|| "invalid background RGBA buffer".to_owned())
    });
  let background = match background {
    Ok(background) => {
      background_timer.finish_ok();
      background
    }
    Err(error) => {
      background_timer.finish_error_code("post_save.background_decode_failed");
      warnings.push(format!("保存后图片生成失败: {error}"));
      total_timer.finish_error_code("post_save.task_failed");
      return PostSaveResult::ImageTasks {
        document_id: job.document_id,
        revision: job.revision,
        preview_path: None,
        warnings,
      };
    }
  };

  let render_timer = PerformanceTimer::start("post_save.render", job.performance, details);
  let image = render_document_to_image(job.snapshot.as_ref(), &background);
  render_timer.finish_ok();

  let mut preview_path = None;
  if install_preview {
    let preview_timer = PerformanceTimer::start(
      "post_save.preview_resize",
      job.performance,
      PerformanceDetails::default().workflow("save"),
    );
    let preview = make_preview(&image, 480);
    preview_timer.finish_ok();
    let encode_timer = PerformanceTimer::start(
      "post_save.preview_encode",
      job.performance,
      PerformanceDetails::default().workflow("save"),
    );
    let encoded = encode_png(&preview);
    match &encoded {
      Ok(_) => encode_timer.finish_ok(),
      Err(error) => encode_timer.finish_error(error),
    }
    match encoded {
      Ok(bytes) => {
        let install_timer = PerformanceTimer::start(
          "post_save.preview_install",
          job.performance,
          PerformanceDetails::default().workflow("save").byte_count(bytes.len()),
        );
        let result = store.install_preview_if_current(job.document_id, job.revision, bytes);
        match &result {
          Ok(_) => install_timer.finish_ok(),
          Err(error) => install_timer.finish_error(error),
        }
        match result {
          Ok(installed) => preview_path = installed,
          Err(error) => warnings.push(format!("预览生成失败: {error}")),
        }
      }
      Err(error) => warnings.push(format!("预览生成失败: {error}")),
    }
  }

  if copy_to_clipboard {
    let clipboard_timer = PerformanceTimer::start(
      "post_save.clipboard",
      job.performance,
      PerformanceDetails::default().workflow("save"),
    );
    let result = copy_image(&image);
    match &result {
      Ok(_) => clipboard_timer.finish_ok(),
      Err(error) => clipboard_timer.finish_error(error),
    }
    if let Err(error) = result {
      warnings.push(format!("剪贴板写入失败: {error}"));
    }
  }

  if warnings.is_empty() {
    total_timer.finish_ok();
  } else {
    total_timer.finish_error_code("post_save.task_failed");
  }
  PostSaveResult::ImageTasks {
    document_id: job.document_id,
    revision: job.revision,
    preview_path,
    warnings,
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
  use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
  };

  use chrono::Local;
  use common::{BoardDocument, CapturedDisplay, GlobalBoundsPx, SizePx};

  use super::*;
  use crate::storage::BackgroundData;

  struct TestDirectory(std::path::PathBuf);

  impl TestDirectory {
    fn new() -> Self {
      let path = std::env::temp_dir().join(format!("rs-board-post-save-{}", uuid::Uuid::new_v4()));
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

  fn test_store() -> (TestDirectory, LocalStore) {
    let temporary = TestDirectory::new();
    let (store, _) = LocalStore::at_root(temporary.path()).unwrap();
    (temporary, store)
  }

  fn job(document_id: DocumentId, revision: Revision, copy_to_clipboard: bool) -> PostSaveJob {
    let mut document = BoardDocument::new_capture(
      document_id,
      SizePx::new(1, 1),
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx { x_px: 0, y_px: 0, width_px: 1, height_px: 1 },
        scale_factor: 1.0,
      },
      Local::now(),
    )
    .unwrap();
    document.revision = revision;
    let snapshot = Arc::new(document.snapshot(revision).unwrap());
    let background = BackgroundData::rgba8(1, 1, vec![0, 0, 0, 255]).unwrap();
    PostSaveJob {
      document_id,
      revision,
      snapshot,
      prepared_background: PreparedBackground::ready(0, background).unwrap(),
      copy_to_clipboard,
      performance: PerformanceContext::default(),
      requested_at: Instant::now(),
    }
  }

  #[test]
  fn render_queue_keeps_only_the_latest_revision_per_document() {
    let document_id = DocumentId::new();
    let mut state = CoordinatorState::default();
    state.enqueue(job(document_id, 1, false));
    state.enqueue(job(document_id, 3, false));
    state.enqueue(job(document_id, 2, false));

    assert_eq!(state.latest_render_by_document.len(), 1);
    assert_eq!(state.latest_render_by_document[&document_id].revision, 3);
    assert_eq!(state.render_order, [document_id]);
  }

  #[test]
  fn enqueue_does_not_schedule_a_full_library_refresh() {
    let document_id = DocumentId::new();
    let first = job(document_id, 1, false);
    let second = job(document_id, 2, false);
    let mut state = CoordinatorState::default();
    state.enqueue(first);
    state.enqueue(second);

    assert!(matches!(state.next_work(), Some(Work::Image { .. })));
    assert!(!state.has_work());
  }

  #[test]
  fn clipboard_queue_preserves_commit_order() {
    let document_id = DocumentId::new();
    let mut state = CoordinatorState::default();
    state.enqueue(job(document_id, 1, true));
    state.enqueue(job(document_id, 2, true));
    state.enqueue(job(document_id, 3, true));

    let revisions: Vec<_> = state.clipboard_queue.iter().map(|job| job.revision).collect();
    assert_eq!(revisions, [1, 2, 3]);
  }

  #[test]
  fn queues_are_bounded_and_report_eviction_without_blocking_commits() {
    let mut state = CoordinatorState::default();
    let mut saw_render_eviction = false;
    for _ in 0..MAX_PENDING_CLIPBOARD_WRITES {
      let outcome = state.enqueue(job(DocumentId::new(), 1, true));
      saw_render_eviction |= outcome.render_evicted;
      assert!(!outcome.clipboard_dropped);
    }
    let overflow = state.enqueue(job(DocumentId::new(), 1, true));

    assert!(saw_render_eviction);
    assert!(overflow.render_evicted);
    assert!(overflow.clipboard_dropped);
    assert_eq!(state.latest_render_by_document.len(), MAX_PENDING_RENDERS);
    assert_eq!(state.clipboard_queue.len(), MAX_PENDING_CLIPBOARD_WRITES);
  }

  #[test]
  fn superseded_render_releases_its_snapshot_reference() {
    let document_id = DocumentId::new();
    let first = job(document_id, 1, false);
    let first_snapshot = Arc::downgrade(&first.snapshot);
    let mut state = CoordinatorState::default();
    state.enqueue(first);
    state.enqueue(job(document_id, 2, false));

    assert!(first_snapshot.upgrade().is_none());
    assert_eq!(state.latest_render_by_document[&document_id].revision, 2);
  }

  #[test]
  fn enqueue_does_not_wait_for_a_busy_worker() {
    let (_temporary, store) = test_store();
    let blocked = Arc::new((Mutex::new(false), Condvar::new()));
    let entered = Arc::new(AtomicBool::new(false));
    let before_work = {
      let blocked = Arc::clone(&blocked);
      let entered = Arc::clone(&entered);
      Arc::new(move || {
        entered.store(true, Ordering::Release);
        let (lock, changed) = &*blocked;
        let mut released = lock_unpoisoned(lock);
        while !*released {
          released = wait_unpoisoned(changed, released);
        }
      }) as Arc<dyn Fn() + Send + Sync>
    };
    let coordinator = PostSaveCoordinator::with_before_work(store, || {}, before_work).unwrap();
    coordinator.enqueue(job(DocumentId::new(), 1, false)).unwrap();
    while !entered.load(Ordering::Acquire) {
      thread::yield_now();
    }

    let started_at = Instant::now();
    coordinator.enqueue(job(DocumentId::new(), 1, false)).unwrap();
    assert!(started_at.elapsed() < Duration::from_millis(250));

    let (lock, changed) = &*blocked;
    *lock_unpoisoned(lock) = true;
    changed.notify_all();
    let deadline = Instant::now() + Duration::from_secs(2);
    while coordinator.try_recv().is_none() {
      assert!(Instant::now() < deadline, "timed out waiting for post-save worker");
      thread::sleep(Duration::from_millis(5));
    }
  }
}
