use std::{
  collections::{HashMap, HashSet, VecDeque},
  fs::File,
  io::Read,
  path::{Path, PathBuf},
  sync::mpsc::{self, Receiver, Sender},
  thread::{self, JoinHandle},
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use common::{DocumentId, Revision};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
  performance::{PerformanceContext, PerformanceDetails, PerformanceTimer},
  storage::{
    DocumentSkeleton, DocumentSummary, LocalStore, ManifestFingerprint, ScanFailure,
    write_file_atomically,
  },
};

const INDEX_FILE_NAME: &str = ".library-index-v1.json";
const INDEX_SCHEMA_VERSION: u32 = 1;
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
const METADATA_BATCH_SIZE: usize = 64;
const METADATA_BATCH_INTERVAL: Duration = Duration::from_millis(16);
const INDEX_WRITE_DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub enum LibraryIndexEvent {
  Bootstrap {
    generation: u64,
    skeletons: Vec<DocumentSkeleton>,
    cached_summaries: Vec<DocumentSummary>,
    failures: Vec<ScanFailure>,
    warning: Option<String>,
  },
  MetadataBatch {
    generation: u64,
    summaries: Vec<DocumentSummary>,
    failures: Vec<(DocumentId, String)>,
  },
  Reconciled {
    generation: u64,
  },
}

#[derive(Debug, Error)]
pub enum LibraryIndexCoordinatorError {
  #[error("library index coordinator is shutting down")]
  ShuttingDown,
  #[error("unable to start library index coordinator: {0}")]
  Spawn(#[source] std::io::Error),
}

pub struct LibraryIndexCoordinator {
  commands: Sender<Command>,
  events: Receiver<LibraryIndexEvent>,
  worker: Option<JoinHandle<()>>,
}

enum Command {
  Prioritize(Vec<DocumentId>),
  Upsert(DocumentSummary),
  Refresh { document_id: DocumentId, revision: Revision },
  Remove(DocumentId),
  Clear,
  Shutdown,
}

impl LibraryIndexCoordinator {
  pub fn new(
    store: LocalStore,
    wake: impl Fn() + Send + Sync + 'static,
  ) -> Result<Self, LibraryIndexCoordinatorError> {
    let (command_sender, command_receiver) = mpsc::channel();
    let (event_sender, event_receiver) = mpsc::channel();
    let worker = thread::Builder::new()
      .name("library-index-coordinator".into())
      .spawn(move || run_worker(store, command_receiver, event_sender, wake))
      .map_err(LibraryIndexCoordinatorError::Spawn)?;
    Ok(Self { commands: command_sender, events: event_receiver, worker: Some(worker) })
  }

  pub fn prioritize(
    &self,
    document_ids: Vec<DocumentId>,
  ) -> Result<(), LibraryIndexCoordinatorError> {
    self.send(Command::Prioritize(document_ids))
  }

  pub fn upsert(&self, summary: DocumentSummary) -> Result<(), LibraryIndexCoordinatorError> {
    self.send(Command::Upsert(summary))
  }

  pub fn refresh(
    &self,
    document_id: DocumentId,
    revision: Revision,
  ) -> Result<(), LibraryIndexCoordinatorError> {
    self.send(Command::Refresh { document_id, revision })
  }

  pub fn remove(&self, document_id: DocumentId) -> Result<(), LibraryIndexCoordinatorError> {
    self.send(Command::Remove(document_id))
  }

  pub fn clear(&self) -> Result<(), LibraryIndexCoordinatorError> {
    self.send(Command::Clear)
  }

  pub fn try_recv(&self) -> Option<LibraryIndexEvent> {
    self.events.try_recv().ok()
  }

  fn send(&self, command: Command) -> Result<(), LibraryIndexCoordinatorError> {
    self.commands.send(command).map_err(|_| LibraryIndexCoordinatorError::ShuttingDown)
  }
}

impl Drop for LibraryIndexCoordinator {
  fn drop(&mut self) {
    let _ = self.commands.send(Command::Shutdown);
    if let Some(worker) = self.worker.take() {
      let _ = worker.join();
    }
  }
}

fn run_worker(
  store: LocalStore,
  commands: Receiver<Command>,
  events: Sender<LibraryIndexEvent>,
  wake: impl Fn() + Send + Sync + 'static,
) {
  let generation = 1;
  let mut reconcile_timer = Some(PerformanceTimer::start(
    "library.reconcile",
    PerformanceContext::default(),
    PerformanceDetails::default(),
  ));
  let skeleton_timer = PerformanceTimer::start(
    "library.skeleton_scan",
    PerformanceContext::default(),
    PerformanceDetails::default(),
  );
  let skeleton_scan = store.scan_document_skeletons();
  match &skeleton_scan {
    Ok(_) => skeleton_timer.finish_ok(),
    Err(error) => skeleton_timer.finish_error(error),
  }
  let skeleton_scan = match skeleton_scan {
    Ok(scan) => scan,
    Err(error) => {
      let warning = format!("library skeleton scan failed: {error}");
      if events
        .send(LibraryIndexEvent::Bootstrap {
          generation,
          skeletons: Vec::new(),
          cached_summaries: Vec::new(),
          failures: Vec::new(),
          warning: Some(warning),
        })
        .is_ok()
      {
        wake();
      }
      if let Some(timer) = reconcile_timer.take() {
        timer.finish_error_code("library.skeleton_scan_failed");
      }
      return;
    }
  };

  let index_path = index_path(&store);
  let index_timer = PerformanceTimer::start(
    "library.index_load",
    PerformanceContext::default(),
    PerformanceDetails::default(),
  );
  let (loaded_index, warning) = match load_index(&index_path) {
    Ok(index) => {
      index_timer.finish_ok();
      (index, None)
    }
    Err(error) => {
      index_timer.finish_error_code("library.index_load_failed");
      (LibraryIndexFile::default(), Some(error))
    }
  };
  let mut index_needs_rewrite = warning.is_some();

  let mut indexed: HashMap<_, _> =
    loaded_index.documents.into_iter().map(|entry| (entry.document_id, entry)).collect();
  let present: HashSet<_> = skeleton_scan.skeletons.iter().map(|item| item.document_id).collect();
  let indexed_before_retain = indexed.len();
  indexed.retain(|document_id, _| present.contains(document_id));
  index_needs_rewrite |= indexed.len() != indexed_before_retain;

  let mut cached_summaries = Vec::with_capacity(indexed.len());
  let mut pending = HashMap::new();
  let mut background_order = VecDeque::new();
  for skeleton in &skeleton_scan.skeletons {
    match indexed.get(&skeleton.document_id) {
      Some(entry) => {
        cached_summaries.push(entry.to_summary(&store));
        if entry.manifest_fingerprint() != skeleton.manifest_fingerprint {
          pending.insert(skeleton.document_id, skeleton.clone());
          background_order.push_back(skeleton.document_id);
        }
      }
      None => {
        pending.insert(skeleton.document_id, skeleton.clone());
        background_order.push_back(skeleton.document_id);
      }
    }
  }

  if events
    .send(LibraryIndexEvent::Bootstrap {
      generation,
      skeletons: skeleton_scan.skeletons,
      cached_summaries,
      failures: skeleton_scan.failures,
      warning,
    })
    .is_err()
  {
    if let Some(timer) = reconcile_timer.take() {
      timer.finish_stale();
    }
    return;
  }
  wake();

  let mut priority = VecDeque::new();
  let mut priority_set = HashSet::new();
  let mut batch = Vec::with_capacity(METADATA_BATCH_SIZE);
  let mut batch_failures = Vec::new();
  let mut last_batch_sent = Instant::now();
  let mut dirty_since = index_needs_rewrite.then(Instant::now);
  let mut reconciled_sent = false;

  loop {
    while let Ok(command) = commands.try_recv() {
      if handle_command(
        &store,
        command,
        &mut indexed,
        &mut pending,
        &mut priority,
        &mut priority_set,
        &mut dirty_since,
      ) {
        flush_batch(
          generation,
          &events,
          &wake,
          &mut batch,
          &mut batch_failures,
          &mut last_batch_sent,
        );
        persist_and_report(&index_path, &indexed, &mut dirty_since, true);
        if let Some(timer) = reconcile_timer.take() {
          timer.finish_stale();
        }
        return;
      }
    }

    let next_id = priority
      .pop_front()
      .inspect(|document_id| {
        priority_set.remove(document_id);
      })
      .or_else(|| background_order.pop_front());
    let next = next_id.and_then(|document_id| pending.remove(&document_id));
    if let Some(skeleton) = next {
      match store.load_document_summary(&skeleton) {
        Ok(summary) => {
          indexed.insert(summary.document_id, IndexedDocument::from_summary(&summary));
          batch.push(summary);
          dirty_since.get_or_insert_with(Instant::now);
        }
        Err(error) => {
          if indexed.remove(&skeleton.document_id).is_some() {
            dirty_since.get_or_insert_with(Instant::now);
          }
          batch_failures.push((skeleton.document_id, error.to_string()));
        }
      }
      if batch.len() + batch_failures.len() >= METADATA_BATCH_SIZE
        || last_batch_sent.elapsed() >= METADATA_BATCH_INTERVAL
      {
        flush_batch(
          generation,
          &events,
          &wake,
          &mut batch,
          &mut batch_failures,
          &mut last_batch_sent,
        );
      }
      persist_and_report(&index_path, &indexed, &mut dirty_since, false);
      continue;
    }

    flush_batch(generation, &events, &wake, &mut batch, &mut batch_failures, &mut last_batch_sent);
    if !reconciled_sent {
      reconciled_sent = true;
      if events.send(LibraryIndexEvent::Reconciled { generation }).is_err() {
        if let Some(timer) = reconcile_timer.take() {
          timer.finish_stale();
        }
        return;
      }
      wake();
      if let Some(timer) = reconcile_timer.take() {
        timer.finish_ok();
      }
    }
    persist_and_report(&index_path, &indexed, &mut dirty_since, false);

    let timeout = dirty_since
      .map(|started| INDEX_WRITE_DEBOUNCE.saturating_sub(started.elapsed()))
      .unwrap_or(Duration::from_secs(1));
    match commands.recv_timeout(timeout) {
      Ok(command) => {
        if handle_command(
          &store,
          command,
          &mut indexed,
          &mut pending,
          &mut priority,
          &mut priority_set,
          &mut dirty_since,
        ) {
          persist_and_report(&index_path, &indexed, &mut dirty_since, true);
          return;
        }
      }
      Err(mpsc::RecvTimeoutError::Timeout) => {
        persist_and_report(&index_path, &indexed, &mut dirty_since, false);
      }
      Err(mpsc::RecvTimeoutError::Disconnected) => {
        persist_and_report(&index_path, &indexed, &mut dirty_since, true);
        return;
      }
    }
  }
}

#[allow(clippy::too_many_arguments)]
fn handle_command(
  store: &LocalStore,
  command: Command,
  indexed: &mut HashMap<DocumentId, IndexedDocument>,
  pending: &mut HashMap<DocumentId, DocumentSkeleton>,
  priority: &mut VecDeque<DocumentId>,
  priority_set: &mut HashSet<DocumentId>,
  dirty_since: &mut Option<Instant>,
) -> bool {
  match command {
    Command::Refresh { document_id, revision } => {
      match store.load_document_summary_by_id(document_id) {
        Ok(summary) if summary.revision == revision => {
          pending.remove(&document_id);
          indexed.insert(document_id, IndexedDocument::from_summary(&summary));
          dirty_since.get_or_insert_with(Instant::now);
        }
        Ok(_) => {}
        Err(error) => eprintln!(
          "library_index_refresh_failed document_id={document_id} revision={revision} error={error}"
        ),
      }
      false
    }
    command => apply_command(command, indexed, pending, priority, priority_set, dirty_since),
  }
}

#[allow(clippy::too_many_arguments)]
fn apply_command(
  command: Command,
  indexed: &mut HashMap<DocumentId, IndexedDocument>,
  pending: &mut HashMap<DocumentId, DocumentSkeleton>,
  priority: &mut VecDeque<DocumentId>,
  priority_set: &mut HashSet<DocumentId>,
  dirty_since: &mut Option<Instant>,
) -> bool {
  match command {
    Command::Prioritize(document_ids) => {
      for document_id in document_ids {
        if pending.contains_key(&document_id) && priority_set.insert(document_id) {
          priority.push_back(document_id);
        }
      }
    }
    Command::Upsert(summary) => {
      pending.remove(&summary.document_id);
      indexed.insert(summary.document_id, IndexedDocument::from_summary(&summary));
      dirty_since.get_or_insert_with(Instant::now);
    }
    Command::Refresh { .. } => unreachable!("refresh commands are handled before mutations"),
    Command::Remove(document_id) => {
      pending.remove(&document_id);
      if indexed.remove(&document_id).is_some() {
        dirty_since.get_or_insert_with(Instant::now);
      }
    }
    Command::Clear => {
      pending.clear();
      priority.clear();
      priority_set.clear();
      indexed.clear();
      dirty_since.get_or_insert_with(Instant::now);
    }
    Command::Shutdown => return true,
  }
  false
}

fn flush_batch(
  generation: u64,
  events: &Sender<LibraryIndexEvent>,
  wake: &impl Fn(),
  summaries: &mut Vec<DocumentSummary>,
  failures: &mut Vec<(DocumentId, String)>,
  last_sent: &mut Instant,
) {
  if summaries.is_empty() && failures.is_empty() {
    return;
  }
  let event = LibraryIndexEvent::MetadataBatch {
    generation,
    summaries: std::mem::take(summaries),
    failures: std::mem::take(failures),
  };
  if events.send(event).is_ok() {
    *last_sent = Instant::now();
    wake();
  }
}

fn persist_if_dirty(
  path: &Path,
  indexed: &HashMap<DocumentId, IndexedDocument>,
  dirty_since: &mut Option<Instant>,
  force: bool,
) -> Result<(), String> {
  let Some(started) = *dirty_since else {
    return Ok(());
  };
  if !force && started.elapsed() < INDEX_WRITE_DEBOUNCE {
    return Ok(());
  }
  let mut documents: Vec<_> = indexed.values().cloned().collect();
  documents.sort_by(|left, right| {
    right
      .updated_at
      .cmp(&left.updated_at)
      .then_with(|| left.document_id.to_string().cmp(&right.document_id.to_string()))
  });
  let bytes =
    serde_json::to_vec(&LibraryIndexFile { schema_version: INDEX_SCHEMA_VERSION, documents })
      .map_err(|error| error.to_string())?;
  write_file_atomically(path, &bytes).map_err(|error| error.to_string())?;
  *dirty_since = None;
  Ok(())
}

fn persist_and_report(
  path: &Path,
  indexed: &HashMap<DocumentId, IndexedDocument>,
  dirty_since: &mut Option<Instant>,
  force: bool,
) {
  if let Err(error) = persist_if_dirty(path, indexed, dirty_since, force) {
    eprintln!("library_index_persist_failed path={} error={error}", path.display());
    // Keep the index dirty while rate-limiting retries after a persistent I/O failure.
    *dirty_since = Some(Instant::now());
  }
}

fn index_path(store: &LocalStore) -> PathBuf {
  store.paths().documents_root().join(INDEX_FILE_NAME)
}

fn load_index(path: &Path) -> Result<LibraryIndexFile, String> {
  if !path.exists() {
    return Ok(LibraryIndexFile::default());
  }
  let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
  if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
    return Err("library index is not a regular file".into());
  }
  if metadata.len() > MAX_INDEX_BYTES {
    return Err("library index exceeds its size limit".into());
  }
  let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
  File::open(path)
    .and_then(|file| file.take(MAX_INDEX_BYTES + 1).read_to_end(&mut bytes))
    .map_err(|error| error.to_string())?;
  if bytes.len() as u64 > MAX_INDEX_BYTES {
    return Err("library index exceeds its size limit".into());
  }
  let index: LibraryIndexFile =
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
  if index.schema_version != INDEX_SCHEMA_VERSION {
    return Err(format!("unsupported library index schema {}", index.schema_version));
  }
  let mut seen = HashSet::with_capacity(index.documents.len());
  for document in &index.documents {
    if document.title.trim().is_empty()
      || document.preview_revision.is_some_and(|preview| preview > document.revision)
      || !seen.insert(document.document_id)
    {
      return Err("library index contains an invalid entry".into());
    }
  }
  Ok(index)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LibraryIndexFile {
  schema_version: u32,
  documents: Vec<IndexedDocument>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct IndexedDocument {
  document_id: DocumentId,
  title: String,
  revision: Revision,
  updated_at: DateTime<Utc>,
  preview_revision: Option<Revision>,
  manifest_byte_len: u64,
  manifest_modified_ns: Option<u128>,
}

impl IndexedDocument {
  fn from_summary(summary: &DocumentSummary) -> Self {
    Self {
      document_id: summary.document_id,
      title: summary.title.clone(),
      revision: summary.revision,
      updated_at: summary.updated_at,
      preview_revision: summary.preview_revision,
      manifest_byte_len: summary.manifest_fingerprint.byte_len,
      manifest_modified_ns: system_time_to_nanos(summary.manifest_fingerprint.modified_at),
    }
  }

  fn to_summary(&self, store: &LocalStore) -> DocumentSummary {
    let preview_path = (self.preview_revision == Some(self.revision)).then(|| {
      store
        .paths()
        .documents_root()
        .join(self.document_id.to_string())
        .join(format!("{}.preview.png", self.document_id))
    });
    DocumentSummary {
      document_id: self.document_id,
      title: self.title.clone(),
      revision: self.revision,
      updated_at: self.updated_at,
      preview_revision: self.preview_revision,
      preview_path,
      manifest_fingerprint: self.manifest_fingerprint(),
    }
  }

  fn manifest_fingerprint(&self) -> ManifestFingerprint {
    ManifestFingerprint {
      byte_len: self.manifest_byte_len,
      modified_at: self.manifest_modified_ns.and_then(nanos_to_system_time),
    }
  }
}

fn system_time_to_nanos(value: Option<SystemTime>) -> Option<u128> {
  value?.duration_since(UNIX_EPOCH).ok().map(|duration| duration.as_nanos())
}

fn nanos_to_system_time(value: u128) -> Option<SystemTime> {
  let seconds = u64::try_from(value / 1_000_000_000).ok()?;
  let nanos = u32::try_from(value % 1_000_000_000).ok()?;
  UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))
}

#[cfg(test)]
mod tests {
  use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  };

  use chrono::TimeZone;
  use common::{BoardDocument, CapturedDisplay, GlobalBoundsPx, SizePx};
  use uuid::Uuid;

  use super::*;
  use crate::storage::{BackgroundData, PersistenceContext, SaveRequest};

  struct TestDirectory(PathBuf);

  impl TestDirectory {
    fn new(name: &str) -> Self {
      let path =
        std::env::temp_dir().join(format!("rs-board-library-index-{name}-{}", Uuid::new_v4()));
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

  fn save_document(store: &LocalStore, title: &str) -> DocumentSummary {
    let document_id = DocumentId::new();
    let mut document = BoardDocument::new_capture(
      document_id,
      SizePx::new(2, 2),
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx { x_px: 0, y_px: 0, width_px: 2, height_px: 2 },
        scale_factor: 1.0,
      },
      Utc.with_ymd_and_hms(2026, 8, 9, 12, 0, 0).unwrap(),
    )
    .unwrap();
    document.title = title.into();
    let background = BackgroundData::rgba8(
      2,
      2,
      vec![10, 20, 30, 255, 10, 20, 30, 255, 10, 20, 30, 255, 10, 20, 30, 255],
    )
    .unwrap();
    store
      .save_document(SaveRequest {
        context: PersistenceContext::new(Uuid::new_v4(), Uuid::new_v4()),
        snapshot: document.snapshot(document.revision).unwrap(),
        background,
      })
      .unwrap()
      .summary
  }

  fn collect_reconcile_events(coordinator: &LibraryIndexCoordinator) -> Vec<LibraryIndexEvent> {
    let mut events = Vec::new();
    loop {
      let event = coordinator.events.recv_timeout(Duration::from_secs(3)).unwrap();
      let reconciled = matches!(event, LibraryIndexEvent::Reconciled { .. });
      events.push(event);
      if reconciled {
        return events;
      }
    }
  }

  fn event_generation(event: &LibraryIndexEvent) -> u64 {
    match event {
      LibraryIndexEvent::Bootstrap { generation, .. }
      | LibraryIndexEvent::MetadataBatch { generation, .. }
      | LibraryIndexEvent::Reconciled { generation } => *generation,
    }
  }

  fn bootstrap(events: &[LibraryIndexEvent]) -> &LibraryIndexEvent {
    events
      .iter()
      .find(|event| matches!(event, LibraryIndexEvent::Bootstrap { .. }))
      .expect("bootstrap event")
  }

  fn hydrated_summaries(events: &[LibraryIndexEvent]) -> Vec<&DocumentSummary> {
    events
      .iter()
      .filter_map(|event| match event {
        LibraryIndexEvent::MetadataBatch { summaries, .. } => Some(summaries.iter()),
        _ => None,
      })
      .flatten()
      .collect()
  }

  #[test]
  fn index_round_trip_preserves_summary_fields() {
    let summary = DocumentSummary {
      document_id: DocumentId::new(),
      title: "讲义".into(),
      revision: 3,
      updated_at: Utc.with_ymd_and_hms(2026, 8, 9, 1, 2, 3).unwrap(),
      preview_revision: Some(3),
      preview_path: Some(PathBuf::from("ignored")),
      manifest_fingerprint: ManifestFingerprint {
        byte_len: 42,
        modified_at: Some(UNIX_EPOCH + Duration::from_nanos(123)),
      },
    };
    let indexed = IndexedDocument::from_summary(&summary);
    let bytes = serde_json::to_vec(&LibraryIndexFile {
      schema_version: INDEX_SCHEMA_VERSION,
      documents: vec![indexed.clone()],
    })
    .unwrap();
    let decoded: LibraryIndexFile = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(decoded.documents[0].document_id, summary.document_id);
    assert_eq!(decoded.documents[0].manifest_fingerprint(), summary.manifest_fingerprint);
  }

  #[test]
  fn system_time_round_trip_is_exact() {
    let value = UNIX_EPOCH + Duration::new(1_234, 567);
    assert_eq!(nanos_to_system_time(system_time_to_nanos(Some(value)).unwrap()), Some(value));
  }

  #[test]
  fn missing_index_bootstraps_from_manifests_and_is_rebuilt() {
    let (_root, store) = store("missing");
    let committed = save_document(&store, "来自 manifest");
    let path = index_path(&store);
    assert!(!path.exists());
    let wake_count = Arc::new(AtomicUsize::new(0));
    let coordinator = LibraryIndexCoordinator::new(store, {
      let wake_count = Arc::clone(&wake_count);
      move || {
        wake_count.fetch_add(1, Ordering::Relaxed);
      }
    })
    .unwrap();

    let events = collect_reconcile_events(&coordinator);
    assert!(events.iter().all(|event| event_generation(event) == 1));
    match bootstrap(&events) {
      LibraryIndexEvent::Bootstrap { skeletons, cached_summaries, warning, .. } => {
        assert_eq!(skeletons.len(), 1);
        assert!(cached_summaries.is_empty());
        assert!(warning.is_none());
      }
      _ => unreachable!(),
    }
    assert_eq!(hydrated_summaries(&events)[0].title, committed.title);
    assert!(wake_count.load(Ordering::Relaxed) >= 2);

    drop(coordinator);
    let rebuilt = load_index(&path).unwrap();
    assert_eq!(rebuilt.schema_version, INDEX_SCHEMA_VERSION);
    assert_eq!(rebuilt.documents.len(), 1);
    assert_eq!(rebuilt.documents[0].title, committed.title);
  }

  #[test]
  fn corrupt_index_is_reported_ignored_and_rebuilt() {
    let (_root, store) = store("corrupt");
    let committed = save_document(&store, "健康 manifest");
    let path = index_path(&store);
    std::fs::write(&path, b"not json").unwrap();
    let coordinator = LibraryIndexCoordinator::new(store, || {}).unwrap();

    let events = collect_reconcile_events(&coordinator);
    match bootstrap(&events) {
      LibraryIndexEvent::Bootstrap { cached_summaries, warning, .. } => {
        assert!(cached_summaries.is_empty());
        assert!(warning.is_some());
      }
      _ => unreachable!(),
    }
    assert_eq!(hydrated_summaries(&events)[0].title, committed.title);

    drop(coordinator);
    assert_eq!(load_index(&path).unwrap().documents[0].title, committed.title);
  }

  #[test]
  fn unsupported_index_version_is_reported_ignored_and_rebuilt() {
    let (_root, store) = store("unsupported-version");
    let path = index_path(&store);
    std::fs::write(
      &path,
      serde_json::to_vec(&serde_json::json!({
        "schema_version": INDEX_SCHEMA_VERSION + 1,
        "documents": [],
      }))
      .unwrap(),
    )
    .unwrap();
    let coordinator = LibraryIndexCoordinator::new(store, || {}).unwrap();

    let events = collect_reconcile_events(&coordinator);
    match bootstrap(&events) {
      LibraryIndexEvent::Bootstrap { warning, .. } => assert!(
        warning.as_deref().is_some_and(|warning| warning.contains("unsupported library index"))
      ),
      _ => unreachable!(),
    }

    drop(coordinator);
    let rebuilt = load_index(&path).unwrap();
    assert_eq!(rebuilt.schema_version, INDEX_SCHEMA_VERSION);
    assert!(rebuilt.documents.is_empty());
  }

  #[test]
  fn stale_cached_entry_is_emitted_then_reconciled_from_the_manifest() {
    let (_root, store) = store("stale");
    let committed = save_document(&store, "当前标题");
    let mut stale = IndexedDocument::from_summary(&committed);
    stale.title = "缓存旧标题".into();
    stale.manifest_byte_len = stale.manifest_byte_len.saturating_add(1);
    let path = index_path(&store);
    std::fs::write(
      &path,
      serde_json::to_vec(&LibraryIndexFile {
        schema_version: INDEX_SCHEMA_VERSION,
        documents: vec![stale],
      })
      .unwrap(),
    )
    .unwrap();
    let coordinator = LibraryIndexCoordinator::new(store, || {}).unwrap();

    let events = collect_reconcile_events(&coordinator);
    match bootstrap(&events) {
      LibraryIndexEvent::Bootstrap { cached_summaries, warning, .. } => {
        assert_eq!(cached_summaries[0].title, "缓存旧标题");
        assert!(warning.is_none());
      }
      _ => unreachable!(),
    }
    assert_eq!(hydrated_summaries(&events)[0].title, committed.title);

    drop(coordinator);
    assert_eq!(load_index(&path).unwrap().documents[0].title, committed.title);
  }

  #[test]
  fn failed_stale_hydration_evicts_the_cached_index_entry() {
    let (_root, store) = store("stale-corrupt-manifest");
    let committed = save_document(&store, "即将损坏");
    let path = index_path(&store);
    let initial = LibraryIndexCoordinator::new(store.clone(), || {}).unwrap();
    collect_reconcile_events(&initial);
    drop(initial);
    assert_eq!(load_index(&path).unwrap().documents.len(), 1);

    let manifest_path = store
      .paths()
      .documents_root()
      .join(committed.document_id.to_string())
      .join(format!("{}.rsboard", committed.document_id));
    std::fs::write(manifest_path, b"not json").unwrap();
    let coordinator = LibraryIndexCoordinator::new(store, || {}).unwrap();

    let events = collect_reconcile_events(&coordinator);
    match bootstrap(&events) {
      LibraryIndexEvent::Bootstrap { cached_summaries, .. } => {
        assert_eq!(cached_summaries.len(), 1);
      }
      _ => unreachable!(),
    }
    assert!(events.iter().any(|event| matches!(
      event,
      LibraryIndexEvent::MetadataBatch { failures, .. }
        if failures.iter().any(|(document_id, _)| *document_id == committed.document_id)
    )));

    drop(coordinator);
    assert!(load_index(&path).unwrap().documents.is_empty());
  }

  #[test]
  fn forced_rebuild_atomically_replaces_the_index_without_leaking_temporary_files() {
    let (root, _store) = store("atomic-rebuild");
    let path = root.path().join(INDEX_FILE_NAME);
    std::fs::write(&path, b"old invalid bytes").unwrap();
    let summary = DocumentSummary {
      document_id: DocumentId::new(),
      title: "重建结果".into(),
      revision: 4,
      updated_at: Utc.with_ymd_and_hms(2026, 8, 9, 1, 2, 3).unwrap(),
      preview_revision: None,
      preview_path: None,
      manifest_fingerprint: ManifestFingerprint::default(),
    };
    let indexed = HashMap::from([(summary.document_id, IndexedDocument::from_summary(&summary))]);
    let mut dirty_since = Some(Instant::now());

    persist_if_dirty(&path, &indexed, &mut dirty_since, true).unwrap();

    assert!(dirty_since.is_none());
    assert_eq!(load_index(&path).unwrap().documents[0].title, summary.title);
    let temporary_prefix = format!(".{INDEX_FILE_NAME}.tmp-");
    assert!(
      std::fs::read_dir(root.path())
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| !entry.file_name().to_string_lossy().starts_with(&temporary_prefix))
    );
  }

  #[test]
  fn persistence_failure_keeps_the_index_dirty_for_a_retry() {
    let (root, _store) = store("persist-failure");
    let destination = root.path().join("destination-is-a-directory");
    std::fs::create_dir(&destination).unwrap();
    let mut dirty_since = Some(Instant::now());

    persist_and_report(&destination, &HashMap::new(), &mut dirty_since, true);

    assert!(dirty_since.is_some());
  }

  #[test]
  fn skeleton_scan_failure_is_delivered_as_a_bootstrap_warning() {
    let (_root, store) = store("skeleton-failure");
    std::fs::remove_dir_all(store.paths().documents_root()).unwrap();
    let coordinator = LibraryIndexCoordinator::new(store, || {}).unwrap();

    let event = coordinator.events.recv_timeout(Duration::from_secs(3)).unwrap();
    match event {
      LibraryIndexEvent::Bootstrap { skeletons, cached_summaries, warning, .. } => {
        assert!(skeletons.is_empty());
        assert!(cached_summaries.is_empty());
        assert!(warning.as_deref().is_some_and(|warning| warning.contains("skeleton scan")));
      }
      _ => panic!("expected bootstrap warning"),
    }
  }

  #[test]
  fn local_mutation_commands_win_after_a_stale_hydration_result() {
    let document_id = DocumentId::new();
    let stale = DocumentSummary {
      document_id,
      title: "后台旧值".into(),
      revision: 1,
      updated_at: Utc.with_ymd_and_hms(2026, 8, 9, 1, 0, 0).unwrap(),
      preview_revision: None,
      preview_path: None,
      manifest_fingerprint: ManifestFingerprint::default(),
    };
    let mut local = stale.clone();
    local.title = "本地新值".into();
    local.revision = 2;
    let mut indexed = HashMap::from([(document_id, IndexedDocument::from_summary(&stale))]);
    let mut pending = HashMap::from([(
      document_id,
      DocumentSkeleton { document_id, manifest_fingerprint: ManifestFingerprint::default() },
    )]);
    let mut priority = VecDeque::from([document_id]);
    let mut priority_set = HashSet::from([document_id]);
    let mut dirty_since = None;

    assert!(!apply_command(
      Command::Upsert(local),
      &mut indexed,
      &mut pending,
      &mut priority,
      &mut priority_set,
      &mut dirty_since,
    ));

    assert_eq!(indexed[&document_id].title, "本地新值");
    assert_eq!(indexed[&document_id].revision, 2);
    assert!(!pending.contains_key(&document_id));
    assert!(dirty_since.is_some());

    assert!(!apply_command(
      Command::Remove(document_id),
      &mut indexed,
      &mut pending,
      &mut priority,
      &mut priority_set,
      &mut dirty_since,
    ));
    assert!(!indexed.contains_key(&document_id));
  }

  #[test]
  fn preview_refresh_reloads_the_manifest_fingerprint_without_opening_the_background() {
    let (_root, store) = store("preview-refresh");
    let committed = save_document(&store, "预览刷新");
    let document_id = committed.document_id;
    let old_fingerprint = committed.manifest_fingerprint.clone();
    let preview =
      BackgroundData::rgba8(2, 2, vec![1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255])
        .unwrap()
        .normalized_png(2, 2)
        .unwrap();
    store.install_preview_if_current(document_id, committed.revision, preview).unwrap();
    std::fs::write(
      store
        .paths()
        .documents_root()
        .join(document_id.to_string())
        .join(format!("{document_id}.png")),
      b"not a png",
    )
    .unwrap();

    let mut indexed = HashMap::from([(document_id, IndexedDocument::from_summary(&committed))]);
    let mut pending = HashMap::from([(
      document_id,
      DocumentSkeleton { document_id, manifest_fingerprint: old_fingerprint.clone() },
    )]);
    let mut priority = VecDeque::new();
    let mut priority_set = HashSet::new();
    let mut dirty_since = None;

    assert!(!handle_command(
      &store,
      Command::Refresh { document_id, revision: committed.revision },
      &mut indexed,
      &mut pending,
      &mut priority,
      &mut priority_set,
      &mut dirty_since,
    ));

    assert_eq!(indexed[&document_id].preview_revision, Some(committed.revision));
    assert_ne!(indexed[&document_id].manifest_fingerprint(), old_fingerprint);
    assert!(!pending.contains_key(&document_id));
    assert!(dirty_since.is_some());
  }
}
