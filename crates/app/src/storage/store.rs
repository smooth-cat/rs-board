use std::{
  fs::File,
  path::{Path, PathBuf},
  sync::{Arc, Mutex, MutexGuard},
  time::SystemTime,
};

use chrono::{DateTime, Utc};
use common::{
  document::{BoardDocument, DocumentId, DocumentSnapshot, Revision},
  format::{
    DocumentManifestSummary, FormatError, decode_document, decode_document_summary,
    encode_document, encode_snapshot,
  },
};
use uuid::Uuid;

use super::{
  BackgroundData, GenerationId, ResourceName, StorageError, StorageResult, StorePaths,
  atomic::{
    AtomicTrace, commit_new_directory, commit_new_directory_traced, create_staging_dir,
    remove_path_if_exists, replace_directory_traced, sync_directory, write_file_atomically,
    write_file_atomically_traced, write_new_file, write_new_file_traced,
  },
  bundle_name::choose_available_bundle_names,
  image_data::{MAX_ENCODED_IMAGE_BYTES, inspect_png_dimensions},
  io::{
    MAX_MANIFEST_BYTES, MAX_METADATA_BYTES, read_bounded, read_named_file, read_regular_path,
    reject_managed_destination, require_plain_directory,
  },
  metadata::{COMMIT_MARKER_FILE_NAME, CommitKind, CommitMarker, SLOT_FILE_NAME, SlotMetadata},
  open_regular_file, open_regular_path,
};
use crate::performance::{PerformanceContext, PerformanceDetails, PerformanceTimer};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PersistenceContext {
  pub request_id: Uuid,
  pub session_id: Uuid,
  pub capture_sequence: Option<u64>,
  pub stash_sequence: Option<u64>,
  pub generation_id: Option<GenerationId>,
}

impl PersistenceContext {
  pub fn new(request_id: Uuid, session_id: Uuid) -> Self {
    Self {
      request_id,
      session_id,
      capture_sequence: None,
      stash_sequence: None,
      generation_id: None,
    }
  }

  pub fn with_sequences(
    mut self,
    capture_sequence: Option<u64>,
    stash_sequence: Option<u64>,
  ) -> Self {
    self.capture_sequence = capture_sequence;
    self.stash_sequence = stash_sequence;
    self
  }

  pub fn with_generation(mut self, generation_id: GenerationId) -> Self {
    self.generation_id = Some(generation_id);
    self
  }
}

#[derive(Clone, Debug)]
pub struct StashRequest {
  pub context: PersistenceContext,
  pub generation_id: GenerationId,
  pub snapshot: DocumentSnapshot,
  pub background: BackgroundData,
}

#[derive(Clone, Debug)]
pub struct SaveRequest {
  pub context: PersistenceContext,
  pub snapshot: DocumentSnapshot,
  pub background: BackgroundData,
}

#[derive(Clone, Debug)]
pub struct ImportRequest {
  pub context: PersistenceContext,
  pub manifest_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct LoadedDocument {
  pub document: BoardDocument,
  pub background: BackgroundData,
  pub directory_path: PathBuf,
  pub manifest_path: PathBuf,
  pub background_path: PathBuf,
  pub preview_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct LoadedDraft {
  pub generation_id: GenerationId,
  pub loaded: LoadedDocument,
}

#[derive(Clone, Debug)]
pub struct LatestDraft {
  pub context: PersistenceContext,
  pub generation_id: GenerationId,
  pub document_id: DocumentId,
  pub revision: Revision,
  pub directory_path: PathBuf,
  pub manifest_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct SavedDocument {
  pub context: PersistenceContext,
  pub document_id: DocumentId,
  pub revision: Revision,
  pub directory_path: PathBuf,
  pub manifest_path: PathBuf,
  pub background_path: PathBuf,
  pub preview_path: Option<PathBuf>,
  pub summary: DocumentSummary,
}

impl SavedDocument {
  pub fn committed_summary(&self) -> &DocumentSummary {
    &self.summary
  }
}

#[derive(Clone, Debug)]
pub struct ImportedDocument {
  pub source_manifest: PathBuf,
  pub saved: SavedDocument,
}

impl ImportedDocument {
  pub fn committed_summary(&self) -> &DocumentSummary {
    self.saved.committed_summary()
  }
}

#[derive(Clone, Debug)]
pub struct ExportedBundle {
  pub stem: String,
  pub manifest_path: PathBuf,
  pub background_path: PathBuf,
  pub preview_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct DocumentSummary {
  pub document_id: DocumentId,
  pub title: String,
  pub revision: Revision,
  pub updated_at: DateTime<Utc>,
  pub preview_revision: Option<Revision>,
  pub preview_path: Option<PathBuf>,
  pub manifest_fingerprint: ManifestFingerprint,
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct ManifestFingerprint {
  pub byte_len: u64,
  pub modified_at: Option<SystemTime>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentSkeleton {
  pub document_id: DocumentId,
  pub manifest_fingerprint: ManifestFingerprint,
}

#[derive(Clone, Debug, Default)]
pub struct SkeletonScanResult {
  pub skeletons: Vec<DocumentSkeleton>,
  pub failures: Vec<ScanFailure>,
}

#[derive(Clone, Debug)]
pub struct ScanFailure {
  pub entry_name: String,
  pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct ScanResult {
  pub documents: Vec<DocumentSummary>,
  pub failures: Vec<ScanFailure>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
  pub recovered_draft: bool,
  pub recovered_documents: usize,
  pub removed_artifacts: usize,
}

#[derive(Clone, Debug)]
pub struct LocalStore {
  paths: StorePaths,
  gate: Arc<Mutex<()>>,
}

struct LoadedManifest {
  document: BoardDocument,
  directory_path: PathBuf,
  manifest_path: PathBuf,
}

impl LocalStore {
  pub fn open(paths: StorePaths) -> StorageResult<(Self, RecoveryReport)> {
    paths.ensure_layout()?;
    let store = Self { paths, gate: Arc::new(Mutex::new(())) };
    let report = store.recover_interrupted_commits()?;
    Ok((store, report))
  }

  pub fn at_root(root: impl Into<PathBuf>) -> StorageResult<(Self, RecoveryReport)> {
    Self::open(StorePaths::new(root))
  }

  pub fn paths(&self) -> &StorePaths {
    &self.paths
  }

  pub fn replace_latest_draft(&self, request: StashRequest) -> StorageResult<LatestDraft> {
    let performance = persistence_performance_context(
      request.context,
      &request.snapshot,
      Some(request.generation_id),
    );
    let pixel_size =
      [request.snapshot.canvas_size_px.width_px, request.snapshot.canvas_size_px.height_px];
    let total_timer = PerformanceTimer::start(
      "persistence.store.total",
      performance,
      PerformanceDetails::default().workflow("stash").pixel_size(pixel_size),
    );
    let result = (|| {
      let lock_timer = PerformanceTimer::start(
        "persistence.store.lock_wait",
        performance,
        PerformanceDetails::default().workflow("stash").pixel_size(pixel_size),
      );
      let guard_result = self.lock();
      match &guard_result {
        Ok(_) => lock_timer.finish_ok(),
        Err(error) => lock_timer.finish_error(error),
      }
      let _guard = guard_result?;
      request.snapshot.validate().map_err(invalid_manifest)?;
      validate_managed_names(&request.snapshot.to_document())?;
      let document_id = request.snapshot.document_id;
      let revision = request.snapshot.revision;
      let encode_timer = PerformanceTimer::start(
        "persistence.background.normalize_png",
        performance,
        PerformanceDetails::default().workflow("stash").pixel_size([
          request.snapshot.canvas_size_px.width_px,
          request.snapshot.canvas_size_px.height_px,
        ]),
      );
      let background_result = request.background.normalized_png(
        request.snapshot.canvas_size_px.width_px,
        request.snapshot.canvas_size_px.height_px,
      );
      match &background_result {
        Ok(_) => encode_timer.finish_ok(),
        Err(error) => encode_timer.finish_error(error),
      }
      let background = background_result?;
      let manifest_timer = PerformanceTimer::start(
        "persistence.manifest.encode",
        performance,
        PerformanceDetails::default().workflow("stash"),
      );
      let manifest_result = encode_snapshot(&request.snapshot).map_err(format_error);
      match &manifest_result {
        Ok(_) => manifest_timer.finish_ok(),
        Err(error) => manifest_timer.finish_error(error),
      }
      let manifest = manifest_result?;
      let staging_timer = PerformanceTimer::start(
        "persistence.staging.create",
        performance,
        PerformanceDetails::default().workflow("stash"),
      );
      let staging_result = create_staging_dir(self.paths.draft_root(), "tmp-draft");
      match &staging_result {
        Ok(_) => staging_timer.finish_ok(),
        Err(error) => staging_timer.finish_error(error),
      }
      let staging = staging_result?;
      let commit_result = (|| {
        let manifest_name = managed_manifest_name(document_id)?;
        let background_name = ResourceName::new(request.snapshot.background.file.clone())?;
        write_new_file_traced(
          &staging.join(background_name.as_os_str()),
          &background,
          AtomicTrace::new(&performance, "stash", "background"),
        )?;
        write_new_file_traced(
          &staging.join(manifest_name.as_os_str()),
          &manifest,
          AtomicTrace::new(&performance, "stash", "manifest"),
        )?;
        let slot = SlotMetadata::new(request.generation_id, document_id.to_string(), revision);
        write_new_file_traced(
          &staging.join(SLOT_FILE_NAME),
          &slot.encode()?,
          AtomicTrace::new(&performance, "stash", "slot"),
        )?;
        let marker = CommitMarker::new(
          CommitKind::Draft,
          document_id.to_string(),
          revision,
          Some(request.generation_id),
        );
        write_new_file_traced(
          &staging.join(COMMIT_MARKER_FILE_NAME),
          &marker.encode()?,
          AtomicTrace::new(&performance, "stash", "commit_marker"),
        )?;
        replace_directory_traced(
          &staging,
          self.paths.latest_draft(),
          AtomicTrace::new(&performance, "stash", "latest_draft"),
        )
      })();
      if commit_result.is_err() {
        let _ = remove_path_if_exists(&staging);
      }
      commit_result?;

      Ok(LatestDraft {
        context: request.context,
        generation_id: request.generation_id,
        document_id,
        revision,
        directory_path: self.paths.latest_draft().to_path_buf(),
        manifest_path: self
          .paths
          .latest_draft()
          .join(managed_manifest_name(document_id)?.as_os_str()),
      })
    })();
    match &result {
      Ok(_) => total_timer.finish_ok(),
      Err(error) => total_timer.finish_error(error),
    }
    result
  }

  pub fn load_latest_draft(&self) -> StorageResult<Option<LoadedDraft>> {
    let _guard = self.lock()?;
    if !self.paths.latest_draft().exists() {
      return Ok(None);
    }
    require_plain_directory(self.paths.latest_draft())?;
    let slot = self.load_slot(self.paths.latest_draft())?;
    let loaded = self.load_managed_package(
      self.paths.latest_draft(),
      &slot.document_id,
      Some(slot.revision),
    )?;
    Ok(Some(LoadedDraft { generation_id: slot.generation_id, loaded }))
  }

  pub fn delete_latest_if_generation(&self, generation_id: GenerationId) -> StorageResult<bool> {
    let _guard = self.lock()?;
    if !self.paths.latest_draft().exists() {
      return Ok(false);
    }
    let slot = self.load_slot(self.paths.latest_draft())?;
    if slot.generation_id != generation_id {
      return Ok(false);
    }
    let deleting = self.paths.draft_root().join(format!(".deleting-{generation_id}"));
    remove_path_if_exists(&deleting)?;
    std::fs::rename(self.paths.latest_draft(), &deleting)
      .map_err(|error| StorageError::io("detaching the latest draft", error))?;
    sync_directory(self.paths.draft_root())?;
    remove_path_if_exists(&deleting)?;
    sync_directory(self.paths.draft_root())?;
    Ok(true)
  }

  pub fn delete_latest_draft(&self) -> StorageResult<bool> {
    let _guard = self.lock()?;
    if !self.paths.latest_draft().exists() {
      return Ok(false);
    }
    remove_path_if_exists(self.paths.latest_draft())?;
    sync_directory(self.paths.draft_root())?;
    Ok(true)
  }

  pub fn clear_all_content(&self) -> StorageResult<()> {
    let _guard = self.lock()?;
    remove_path_if_exists(self.paths.latest_draft())?;
    for entry in std::fs::read_dir(self.paths.documents_root())
      .map_err(|error| StorageError::io("scanning documents for removal", error))?
    {
      let entry = entry.map_err(|error| StorageError::io("reading document entry", error))?;
      remove_path_if_exists(&entry.path())?;
    }
    sync_directory(self.paths.draft_root())?;
    sync_directory(self.paths.documents_root())
  }

  pub fn save_document(&self, request: SaveRequest) -> StorageResult<SavedDocument> {
    let performance = persistence_performance_context(request.context, &request.snapshot, None);
    let pixel_size =
      [request.snapshot.canvas_size_px.width_px, request.snapshot.canvas_size_px.height_px];
    let total_timer = PerformanceTimer::start(
      "persistence.store.total",
      performance,
      PerformanceDetails::default().workflow("save").pixel_size(pixel_size),
    );
    let lock_timer = PerformanceTimer::start(
      "persistence.store.lock_wait",
      performance,
      PerformanceDetails::default().workflow("save").pixel_size(pixel_size),
    );
    let guard_result = self.lock();
    match &guard_result {
      Ok(_) => lock_timer.finish_ok(),
      Err(error) => lock_timer.finish_error(error),
    }
    let result = match guard_result {
      Ok(_guard) => {
        self.save_document_locked_impl(request, CommitKind::Document, Some((&performance, "save")))
      }
      Err(error) => Err(error),
    };
    match &result {
      Ok(_) => total_timer.finish_ok(),
      Err(error) => total_timer.finish_error(error),
    }
    result
  }

  pub fn open_document(&self, document_id: DocumentId) -> StorageResult<LoadedDocument> {
    let _guard = self.lock()?;
    self.open_document_locked(document_id)
  }

  pub fn scan_document_skeletons(&self) -> StorageResult<SkeletonScanResult> {
    // Directory commits are atomic and staging entries are hidden. Avoid holding the
    // store gate while walking a large library so an in-progress bootstrap cannot
    // delay a save; per-entry races are reported and reconciled by the index worker.
    self.scan_document_skeletons_unlocked()
  }

  pub fn load_document_summary(
    &self,
    skeleton: &DocumentSkeleton,
  ) -> StorageResult<DocumentSummary> {
    let _guard = self.lock()?;
    self.load_document_summary_locked(skeleton.document_id)
  }

  pub fn load_document_summary_by_id(
    &self,
    document_id: DocumentId,
  ) -> StorageResult<DocumentSummary> {
    let _guard = self.lock()?;
    self.load_document_summary_locked(document_id)
  }

  pub fn scan_documents(&self) -> StorageResult<ScanResult> {
    let _guard = self.lock()?;
    let skeleton_scan = self.scan_document_skeletons_unlocked()?;
    let mut result = ScanResult { documents: Vec::new(), failures: skeleton_scan.failures };
    for skeleton in skeleton_scan.skeletons {
      match self.load_document_summary_locked(skeleton.document_id) {
        Ok(summary) => result.documents.push(summary),
        Err(error) => result.failures.push(ScanFailure {
          entry_name: skeleton.document_id.to_string(),
          message: error.to_string(),
        }),
      }
    }
    sort_document_summaries(&mut result.documents);
    Ok(result)
  }

  fn scan_document_skeletons_unlocked(&self) -> StorageResult<SkeletonScanResult> {
    let mut result = SkeletonScanResult::default();
    let entries = std::fs::read_dir(self.paths.documents_root())
      .map_err(|error| StorageError::io("scanning documents", error))?;
    for entry in entries {
      let entry = match entry {
        Ok(entry) => entry,
        Err(error) => {
          result
            .failures
            .push(ScanFailure { entry_name: "<unreadable>".into(), message: error.to_string() });
          continue;
        }
      };
      let name = entry.file_name().to_string_lossy().into_owned();
      if name.starts_with('.') {
        continue;
      }
      let Some(document_id) = parse_document_id(&name) else {
        result.failures.push(ScanFailure {
          entry_name: name,
          message: "directory name is not a document ID".into(),
        });
        continue;
      };
      if name != document_id.to_string() {
        result.failures.push(ScanFailure {
          entry_name: name,
          message: "directory name is not a canonical document ID".into(),
        });
        continue;
      }
      let file_type = match entry.file_type() {
        Ok(file_type) => file_type,
        Err(error) => {
          result.failures.push(ScanFailure { entry_name: name, message: error.to_string() });
          continue;
        }
      };
      if !file_type.is_dir() || file_type.is_symlink() {
        result.failures.push(ScanFailure {
          entry_name: name,
          message: "document entry is not a plain directory".into(),
        });
        continue;
      }
      let manifest_name = managed_manifest_name(document_id)?;
      match open_regular_file(&entry.path(), &manifest_name).and_then(|file| {
        manifest_fingerprint(&file)
          .map(|manifest_fingerprint| DocumentSkeleton { document_id, manifest_fingerprint })
      }) {
        Ok(skeleton) => result.skeletons.push(skeleton),
        Err(error) => {
          result.failures.push(ScanFailure { entry_name: name, message: error.to_string() })
        }
      }
    }
    result.skeletons.sort_by(|left, right| {
      right
        .manifest_fingerprint
        .modified_at
        .cmp(&left.manifest_fingerprint.modified_at)
        .then_with(|| left.document_id.to_string().cmp(&right.document_id.to_string()))
    });
    Ok(result)
  }

  pub fn delete_document(&self, document_id: DocumentId) -> StorageResult<()> {
    let _guard = self.lock()?;
    let directory = self.paths.document_dir(document_id);
    if !directory.exists() {
      return Err(StorageError::DocumentNotFound(document_id.to_string()));
    }
    require_plain_directory(&directory)?;
    remove_path_if_exists(&directory)?;
    sync_directory(self.paths.documents_root())
  }

  pub fn rename_document(
    &self,
    document_id: DocumentId,
    title: impl Into<String>,
  ) -> StorageResult<DocumentSummary> {
    let _guard = self.lock()?;
    let title = title.into();
    if title.trim().is_empty() || title.chars().count() > 512 {
      return Err(StorageError::InvalidManifest(
        "document title must contain 1 to 512 characters".into(),
      ));
    }
    let directory = self.paths.document_dir(document_id);
    if !directory.exists() {
      return Err(StorageError::DocumentNotFound(document_id.to_string()));
    }
    let mut loaded = self.load_managed_manifest(&directory, &document_id.to_string(), None)?;
    loaded.document.title = title;
    loaded.document.updated_at = Utc::now().max(loaded.document.updated_at);
    loaded.document.validate().map_err(invalid_manifest)?;
    let manifest = encode_document(&loaded.document).map_err(format_error)?;
    write_file_atomically(&loaded.manifest_path, &manifest)?;
    let fingerprint = manifest_fingerprint_at(&loaded.manifest_path, manifest.len() as u64);
    Ok(document_summary_from_document(&loaded.document, &loaded.directory_path, fingerprint))
  }

  pub fn install_preview_if_current(
    &self,
    document_id: DocumentId,
    revision: Revision,
    png_bytes: impl Into<Arc<[u8]>>,
  ) -> StorageResult<Option<PathBuf>> {
    let _guard = self.lock()?;
    let directory = self.paths.document_dir(document_id);
    if !directory.exists() {
      return Err(StorageError::DocumentNotFound(document_id.to_string()));
    }
    let mut loaded = self.load_managed_manifest(&directory, &document_id.to_string(), None)?;
    if loaded.document.revision != revision {
      return Ok(None);
    }
    let png_bytes = png_bytes.into();
    let (width_px, height_px) = inspect_png_dimensions(&png_bytes)?;
    if width_px.max(height_px) > 480 {
      return Err(StorageError::InvalidImage("preview long edge exceeds 480px".into()));
    }
    loaded.document.preview_revision = Some(revision);
    loaded.document.validate().map_err(invalid_manifest)?;
    let manifest = encode_document(&loaded.document).map_err(format_error)?;
    let preview_name = ResourceName::new(loaded.document.preview_file.clone())?;
    let preview_path = loaded.directory_path.join(preview_name.as_os_str());
    // Replacing the image before the manifest is conservative: a crash can leave
    // an unconfirmed image, but never a manifest that confirms a missing image.
    write_file_atomically(&preview_path, &png_bytes)?;
    write_file_atomically(&loaded.manifest_path, &manifest)?;
    Ok(Some(preview_path))
  }

  pub fn import_document(&self, request: ImportRequest) -> StorageResult<ImportedDocument> {
    let _guard = self.lock()?;
    let source_manifest = request.manifest_path;
    if self.path_is_managed(&source_manifest)? {
      return Err(StorageError::ManagedDestination);
    }
    let bytes = read_regular_path(&source_manifest, MAX_MANIFEST_BYTES)?;
    let mut document = decode_document(&bytes).map_err(format_error)?;
    let parent = source_manifest.parent().ok_or_else(|| {
      StorageError::InvalidManifest("import manifest has no parent directory".into())
    })?;
    require_plain_directory(parent)?;
    let background_name = ResourceName::new(document.background.file.clone())?;
    let background_bytes = read_named_file(parent, &background_name, MAX_ENCODED_IMAGE_BYTES)?;
    let source_background = BackgroundData::encoded_png(background_bytes)?;
    let normalized = source_background
      .normalized_png(document.canvas_size_px.width_px, document.canvas_size_px.height_px)?;

    let document_id = DocumentId::new();
    document.document_id = document_id;
    document.background.file = format!("{document_id}.png");
    document.preview_file = format!("{document_id}.preview.png");
    document.preview_revision = None;
    document.created_at = Utc::now();
    document.updated_at = document.created_at;
    document.validate().map_err(invalid_manifest)?;
    let saved = self.save_document_locked(
      SaveRequest {
        context: request.context,
        snapshot: DocumentSnapshot::from(&document),
        background: BackgroundData::encoded_png(normalized)?,
      },
      CommitKind::Import,
    )?;
    Ok(ImportedDocument { source_manifest, saved })
  }

  pub fn export_document(
    &self,
    document_id: DocumentId,
    destination: &Path,
  ) -> StorageResult<ExportedBundle> {
    let _guard = self.lock()?;
    reject_managed_destination(self.paths.root(), destination)?;
    let loaded = self.open_document_locked(document_id)?;
    let names = choose_available_bundle_names(destination, &loaded.document.title);
    let mut exported = loaded.document.clone();
    exported.background.file = format!("{}.png", names.stem);
    exported.preview_file = format!("{}.preview.png", names.stem);
    if loaded.preview_path.is_none() {
      exported.preview_revision = None;
    }
    exported.validate().map_err(invalid_manifest)?;
    let manifest = encode_document(&exported).map_err(format_error)?;
    let background = match &loaded.background {
      BackgroundData::EncodedPng(bytes) => Arc::clone(bytes),
      other => {
        other.normalized_png(exported.canvas_size_px.width_px, exported.canvas_size_px.height_px)?
      }
    };

    let mut created = Vec::new();
    let export_result = (|| {
      write_new_file(&names.background, &background)?;
      created.push(names.background.clone());
      let preview_path = if let Some(source) = &loaded.preview_path {
        let preview = read_regular_path(source, MAX_ENCODED_IMAGE_BYTES)?;
        write_new_file(&names.preview, &preview)?;
        created.push(names.preview.clone());
        Some(names.preview.clone())
      } else {
        None
      };
      write_new_file(&names.manifest, &manifest)?;
      created.push(names.manifest.clone());
      sync_directory(destination)?;
      Ok(ExportedBundle {
        stem: names.stem,
        manifest_path: names.manifest,
        background_path: names.background,
        preview_path,
      })
    })();
    if export_result.is_err() {
      for path in created.into_iter().rev() {
        let _ = remove_path_if_exists(&path);
      }
    }
    export_result
  }

  pub fn recover_interrupted_commits(&self) -> StorageResult<RecoveryReport> {
    let _guard = self.lock()?;
    let mut report = RecoveryReport::default();
    self.recover_draft(&mut report)?;
    self.recover_documents(&mut report)?;
    Ok(report)
  }

  fn save_document_locked(
    &self,
    request: SaveRequest,
    commit_kind: CommitKind,
  ) -> StorageResult<SavedDocument> {
    self.save_document_locked_impl(request, commit_kind, None)
  }

  fn save_document_locked_impl(
    &self,
    request: SaveRequest,
    commit_kind: CommitKind,
    trace: Option<(&PerformanceContext, &'static str)>,
  ) -> StorageResult<SavedDocument> {
    request.snapshot.validate().map_err(invalid_manifest)?;
    validate_managed_names(&request.snapshot.to_document())?;
    let document_id = request.snapshot.document_id;
    let revision = request.snapshot.revision;
    let directory = self.paths.document_dir(document_id);
    let manifest_name = managed_manifest_name(document_id)?;
    let background_name = ResourceName::new(request.snapshot.background.file.clone())?;
    let manifest_result =
      measured_store(trace, "persistence.manifest.encode", PerformanceDetails::default(), || {
        encode_snapshot(&request.snapshot).map_err(format_error)
      });
    let manifest = manifest_result?;

    if directory.exists() {
      require_plain_directory(&directory)?;
      // Decode before overwriting so a future schema is never silently replaced.
      let existing_manifest = read_named_file(&directory, &manifest_name, MAX_MANIFEST_BYTES)?;
      let existing = decode_document(&existing_manifest).map_err(format_error)?;
      if existing.document_id != document_id {
        return Err(StorageError::InvalidManifest(
          "document ID does not match its managed directory".into(),
        ));
      }
      let validation = measured_store(
        trace,
        "persistence.background.validate_existing",
        PerformanceDetails::default(),
        || self.validate_managed_background(&directory, &request.snapshot),
      );
      if validation.is_err() {
        let background = measured_store(
          trace,
          "persistence.background.normalize_png",
          PerformanceDetails::default().pixel_size([
            request.snapshot.canvas_size_px.width_px,
            request.snapshot.canvas_size_px.height_px,
          ]),
          || {
            request.background.normalized_png(
              request.snapshot.canvas_size_px.width_px,
              request.snapshot.canvas_size_px.height_px,
            )
          },
        )?;
        if let Some((performance, workflow)) = trace {
          write_file_atomically_traced(
            &directory.join(background_name.as_os_str()),
            &background,
            AtomicTrace::new(performance, workflow, "background"),
          )?;
        } else {
          write_file_atomically(&directory.join(background_name.as_os_str()), &background)?;
        }
      }
      if let Some((performance, workflow)) = trace {
        write_file_atomically_traced(
          &directory.join(manifest_name.as_os_str()),
          &manifest,
          AtomicTrace::new(performance, workflow, "manifest"),
        )?;
      } else {
        write_file_atomically(&directory.join(manifest_name.as_os_str()), &manifest)?;
      }
    } else {
      let background = measured_store(
        trace,
        "persistence.background.normalize_png",
        PerformanceDetails::default().pixel_size([
          request.snapshot.canvas_size_px.width_px,
          request.snapshot.canvas_size_px.height_px,
        ]),
        || {
          request.background.normalized_png(
            request.snapshot.canvas_size_px.width_px,
            request.snapshot.canvas_size_px.height_px,
          )
        },
      )?;
      let staging =
        measured_store(trace, "persistence.staging.create", PerformanceDetails::default(), || {
          create_staging_dir(self.paths.documents_root(), "tmp-document")
        })?;
      let commit_result = (|| {
        if let Some((performance, workflow)) = trace {
          write_new_file_traced(
            &staging.join(background_name.as_os_str()),
            &background,
            AtomicTrace::new(performance, workflow, "background"),
          )?;
          write_new_file_traced(
            &staging.join(manifest_name.as_os_str()),
            &manifest,
            AtomicTrace::new(performance, workflow, "manifest"),
          )?;
        } else {
          write_new_file(&staging.join(background_name.as_os_str()), &background)?;
          write_new_file(&staging.join(manifest_name.as_os_str()), &manifest)?;
        }
        let marker = CommitMarker::new(commit_kind, document_id.to_string(), revision, None);
        if let Some((performance, workflow)) = trace {
          write_new_file_traced(
            &staging.join(COMMIT_MARKER_FILE_NAME),
            &marker.encode()?,
            AtomicTrace::new(performance, workflow, "commit_marker"),
          )?;
          commit_new_directory_traced(
            &staging,
            &directory,
            AtomicTrace::new(performance, workflow, "document"),
          )
        } else {
          write_new_file(&staging.join(COMMIT_MARKER_FILE_NAME), &marker.encode()?)?;
          commit_new_directory(&staging, &directory)
        }
      })();
      if commit_result.is_err() {
        let _ = remove_path_if_exists(&staging);
      }
      commit_result?;
    }

    let committed_document = request.snapshot.to_document();
    let preview_path = valid_preview_path(&directory, &committed_document);
    let manifest_path = directory.join(manifest_name.as_os_str());
    let mut summary = document_summary_from_document(
      &committed_document,
      &directory,
      manifest_fingerprint_at(&manifest_path, manifest.len() as u64),
    );
    summary.preview_path = preview_path.clone();
    Ok(SavedDocument {
      context: request.context,
      document_id,
      revision,
      directory_path: directory.clone(),
      manifest_path,
      background_path: directory.join(background_name.as_os_str()),
      preview_path,
      summary,
    })
  }

  fn open_document_locked(&self, document_id: DocumentId) -> StorageResult<LoadedDocument> {
    let directory = self.paths.document_dir(document_id);
    if !directory.exists() {
      return Err(StorageError::DocumentNotFound(document_id.to_string()));
    }
    self.load_managed_package(&directory, &document_id.to_string(), None)
  }

  fn load_document_summary_locked(
    &self,
    document_id: DocumentId,
  ) -> StorageResult<DocumentSummary> {
    let directory = self.paths.document_dir(document_id);
    if !directory.exists() {
      return Err(StorageError::DocumentNotFound(document_id.to_string()));
    }
    require_plain_directory(&directory)?;
    let manifest_name = managed_manifest_name(document_id)?;
    let file = open_regular_file(&directory, &manifest_name)?;
    let manifest_fingerprint = manifest_fingerprint(&file)?;
    let manifest_bytes = read_bounded(file, MAX_MANIFEST_BYTES)?;
    let summary = decode_document_summary(&manifest_bytes).map_err(format_error)?;
    validate_managed_summary(document_id, &summary)?;
    Ok(document_summary_from_manifest(summary, &directory, manifest_fingerprint))
  }

  fn load_managed_manifest(
    &self,
    directory: &Path,
    expected_document_id: &str,
    expected_revision: Option<Revision>,
  ) -> StorageResult<LoadedManifest> {
    require_plain_directory(directory)?;
    let document_id = parse_document_id(expected_document_id)
      .ok_or_else(|| StorageError::InvalidManifest("document ID is not a UUID".into()))?;
    let manifest_name = managed_manifest_name(document_id)?;
    let manifest_path = directory.join(manifest_name.as_os_str());
    let manifest_bytes = read_named_file(directory, &manifest_name, MAX_MANIFEST_BYTES)?;
    let document = decode_document(&manifest_bytes).map_err(format_error)?;
    if document.document_id != document_id {
      return Err(StorageError::InvalidManifest("document ID does not match its package".into()));
    }
    if expected_revision.is_some_and(|revision| revision != document.revision) {
      return Err(StorageError::InvalidManifest(
        "draft slot revision does not match its manifest".into(),
      ));
    }
    validate_managed_names(&document)?;
    Ok(LoadedManifest { document, directory_path: directory.to_path_buf(), manifest_path })
  }

  fn load_managed_package(
    &self,
    directory: &Path,
    expected_document_id: &str,
    expected_revision: Option<Revision>,
  ) -> StorageResult<LoadedDocument> {
    let loaded = self.load_managed_manifest(directory, expected_document_id, expected_revision)?;
    let document = loaded.document;
    let background_name = ResourceName::new(document.background.file.clone())?;
    let background_bytes = read_named_file(directory, &background_name, MAX_ENCODED_IMAGE_BYTES)?;
    let background = BackgroundData::encoded_png(background_bytes)?;
    background.decode_rgba8()?;
    let dimensions = background.dimensions()?;
    if dimensions != (document.canvas_size_px.width_px, document.canvas_size_px.height_px) {
      return Err(StorageError::InvalidImage(
        "stored background dimensions do not match the document".into(),
      ));
    }
    let preview_path = valid_preview_path(directory, &document);
    Ok(LoadedDocument {
      manifest_path: loaded.manifest_path,
      background_path: directory.join(background_name.as_os_str()),
      directory_path: loaded.directory_path,
      document,
      background,
      preview_path,
    })
  }

  fn validate_managed_background(
    &self,
    directory: &Path,
    snapshot: &DocumentSnapshot,
  ) -> StorageResult<()> {
    let name = ResourceName::new(snapshot.background.file.clone())?;
    let bytes = read_named_file(directory, &name, MAX_ENCODED_IMAGE_BYTES)?;
    let background = BackgroundData::encoded_png(bytes)?;
    background.decode_rgba8()?;
    if background.dimensions()?
      != (snapshot.canvas_size_px.width_px, snapshot.canvas_size_px.height_px)
    {
      return Err(StorageError::InvalidImage(
        "stored background dimensions do not match the document".into(),
      ));
    }
    Ok(())
  }

  fn load_slot(&self, directory: &Path) -> StorageResult<SlotMetadata> {
    let name = ResourceName::new(SLOT_FILE_NAME)?;
    let bytes = read_named_file(directory, &name, MAX_METADATA_BYTES)?;
    SlotMetadata::decode(&bytes)
  }

  fn path_is_managed(&self, path: &Path) -> StorageResult<bool> {
    let canonical_root = self
      .paths
      .root()
      .canonicalize()
      .map_err(|error| StorageError::io("resolving the application data directory", error))?;
    let canonical =
      path.canonicalize().map_err(|error| StorageError::io("resolving an import path", error))?;
    Ok(canonical.starts_with(canonical_root))
  }

  fn recover_draft(&self, report: &mut RecoveryReport) -> StorageResult<()> {
    let mut candidates = collect_staging_directories(self.paths.draft_root())?;
    if !self.paths.latest_draft().exists() {
      candidates.sort_by_key(|candidate| candidate.marker.created_at_millis);
      while let Some(candidate) = candidates.pop() {
        if candidate.marker.kind == CommitKind::Draft
          && self.validate_draft_candidate(&candidate.path).is_ok()
        {
          commit_new_directory(&candidate.path, self.paths.latest_draft())?;
          report.recovered_draft = true;
          break;
        }
      }
    }
    for candidate in candidates {
      if candidate.path.exists() {
        remove_path_if_exists(&candidate.path)?;
        report.removed_artifacts += 1;
      }
    }
    self.cleanup_unknown_artifacts(self.paths.draft_root(), report)?;
    Ok(())
  }

  fn validate_draft_candidate(&self, directory: &Path) -> StorageResult<()> {
    let slot = self.load_slot(directory)?;
    self.load_managed_package(directory, &slot.document_id, Some(slot.revision))?;
    Ok(())
  }

  fn recover_documents(&self, report: &mut RecoveryReport) -> StorageResult<()> {
    let candidates = collect_staging_directories(self.paths.documents_root())?;
    for candidate in candidates {
      if !matches!(candidate.marker.kind, CommitKind::Document | CommitKind::Import) {
        remove_path_if_exists(&candidate.path)?;
        report.removed_artifacts += 1;
        continue;
      }
      let Some(document_id) = parse_document_id(&candidate.marker.document_id) else {
        remove_path_if_exists(&candidate.path)?;
        report.removed_artifacts += 1;
        continue;
      };
      let destination = self.paths.document_dir(document_id);
      if !destination.exists()
        && self
          .load_managed_package(
            &candidate.path,
            &candidate.marker.document_id,
            Some(candidate.marker.revision),
          )
          .is_ok()
      {
        commit_new_directory(&candidate.path, &destination)?;
        report.recovered_documents += 1;
      } else {
        remove_path_if_exists(&candidate.path)?;
        report.removed_artifacts += 1;
      }
    }
    self.cleanup_unknown_artifacts(self.paths.documents_root(), report)
  }

  fn cleanup_unknown_artifacts(
    &self,
    root: &Path,
    report: &mut RecoveryReport,
  ) -> StorageResult<()> {
    for entry in std::fs::read_dir(root)
      .map_err(|error| StorageError::io("scanning storage artifacts", error))?
    {
      let entry = entry.map_err(|error| StorageError::io("reading a storage artifact", error))?;
      let name = entry.file_name();
      let name = name.to_string_lossy();
      if name.starts_with(".tmp-") || name.starts_with(".old-") || name.starts_with(".deleting-") {
        remove_path_if_exists(&entry.path())?;
        report.removed_artifacts += 1;
      }
    }
    Ok(())
  }

  fn lock(&self) -> StorageResult<MutexGuard<'_, ()>> {
    self
      .gate
      .lock()
      .map_err(|_| StorageError::IncompleteCommit("storage operation lock is poisoned".into()))
  }
}

#[derive(Debug)]
struct StagingCandidate {
  path: PathBuf,
  marker: CommitMarker,
}

fn collect_staging_directories(root: &Path) -> StorageResult<Vec<StagingCandidate>> {
  let mut candidates = Vec::new();
  for entry in std::fs::read_dir(root)
    .map_err(|error| StorageError::io("scanning staging directories", error))?
  {
    let entry = entry.map_err(|error| StorageError::io("reading a staging directory", error))?;
    let name = entry.file_name();
    let name = name.to_string_lossy();
    if !(name.starts_with(".tmp-") || name.starts_with(".old-")) {
      continue;
    }
    let metadata = match entry.file_type() {
      Ok(metadata) => metadata,
      Err(_) => continue,
    };
    if !metadata.is_dir() || metadata.is_symlink() {
      continue;
    }
    let marker_name = ResourceName::new(COMMIT_MARKER_FILE_NAME)?;
    let marker = read_named_file(&entry.path(), &marker_name, MAX_METADATA_BYTES)
      .and_then(|bytes| CommitMarker::decode(&bytes));
    if let Ok(marker) = marker {
      candidates.push(StagingCandidate { path: entry.path(), marker });
    }
  }
  Ok(candidates)
}

fn validate_managed_names(document: &BoardDocument) -> StorageResult<()> {
  let expected_background = format!("{}.png", document.document_id);
  let expected_preview = format!("{}.preview.png", document.document_id);
  if document.background.file != expected_background || document.preview_file != expected_preview {
    return Err(StorageError::InvalidManifest(
      "managed document resources must use the document ID as their base name".into(),
    ));
  }
  Ok(())
}

fn validate_managed_summary(
  expected_document_id: DocumentId,
  summary: &DocumentManifestSummary,
) -> StorageResult<()> {
  if summary.document_id != expected_document_id {
    return Err(StorageError::InvalidManifest("document ID does not match its package".into()));
  }
  if summary.preview_file != format!("{expected_document_id}.preview.png") {
    return Err(StorageError::InvalidManifest(
      "managed document resources must use the document ID as their base name".into(),
    ));
  }
  Ok(())
}

fn managed_manifest_name(document_id: DocumentId) -> StorageResult<ResourceName> {
  ResourceName::new(format!("{document_id}.rsboard"))
}

fn valid_preview_path(directory: &Path, document: &BoardDocument) -> Option<PathBuf> {
  let path = declared_preview_path(
    directory,
    &document.preview_file,
    document.preview_revision,
    document.revision,
  )?;
  open_regular_path(&path).ok()?;
  Some(path)
}

fn declared_preview_path(
  directory: &Path,
  preview_file: &str,
  preview_revision: Option<Revision>,
  revision: Revision,
) -> Option<PathBuf> {
  if preview_revision != Some(revision) {
    return None;
  }
  let name = ResourceName::new(preview_file.to_owned()).ok()?;
  Some(directory.join(name.as_os_str()))
}

fn document_summary_from_manifest(
  summary: DocumentManifestSummary,
  directory: &Path,
  manifest_fingerprint: ManifestFingerprint,
) -> DocumentSummary {
  DocumentSummary {
    document_id: summary.document_id,
    title: summary.title,
    revision: summary.revision,
    updated_at: summary.updated_at,
    preview_revision: summary.preview_revision,
    preview_path: declared_preview_path(
      directory,
      &summary.preview_file,
      summary.preview_revision,
      summary.revision,
    ),
    manifest_fingerprint,
  }
}

fn document_summary_from_document(
  document: &BoardDocument,
  directory: &Path,
  manifest_fingerprint: ManifestFingerprint,
) -> DocumentSummary {
  DocumentSummary {
    document_id: document.document_id,
    title: document.title.clone(),
    revision: document.revision,
    updated_at: document.updated_at,
    preview_revision: document.preview_revision,
    preview_path: declared_preview_path(
      directory,
      &document.preview_file,
      document.preview_revision,
      document.revision,
    ),
    manifest_fingerprint,
  }
}

fn manifest_fingerprint(file: &File) -> StorageResult<ManifestFingerprint> {
  let metadata =
    file.metadata().map_err(|error| StorageError::io("inspecting a manifest", error))?;
  Ok(ManifestFingerprint { byte_len: metadata.len(), modified_at: metadata.modified().ok() })
}

fn manifest_fingerprint_at(path: &Path, fallback_byte_len: u64) -> ManifestFingerprint {
  open_regular_path(path)
    .and_then(|file| manifest_fingerprint(&file))
    .unwrap_or(ManifestFingerprint { byte_len: fallback_byte_len, modified_at: None })
}

fn sort_document_summaries(documents: &mut [DocumentSummary]) {
  documents.sort_by(|left, right| {
    right
      .updated_at
      .cmp(&left.updated_at)
      .then_with(|| left.document_id.to_string().cmp(&right.document_id.to_string()))
  });
}

fn parse_document_id(value: &str) -> Option<DocumentId> {
  Uuid::parse_str(value).ok().map(DocumentId::from_uuid)
}

fn persistence_performance_context(
  context: PersistenceContext,
  snapshot: &DocumentSnapshot,
  generation_id: Option<GenerationId>,
) -> PerformanceContext {
  PerformanceContext {
    request_id: Some(context.request_id),
    session_id: Some(context.session_id),
    capture_sequence: context.capture_sequence,
    stash_sequence: context.stash_sequence,
    generation_id: generation_id.or(context.generation_id).map(GenerationId::as_uuid),
    document_id: Some(snapshot.document_id.as_uuid()),
    revision: Some(snapshot.revision),
  }
}

fn measured_store<T>(
  trace: Option<(&PerformanceContext, &'static str)>,
  stage: &'static str,
  details: PerformanceDetails,
  operation: impl FnOnce() -> StorageResult<T>,
) -> StorageResult<T> {
  let timer = trace.map(|(performance, workflow)| {
    PerformanceTimer::start(stage, *performance, details.workflow(workflow))
  });
  let result = operation();
  if let Some(timer) = timer {
    match &result {
      Ok(_) => timer.finish_ok(),
      Err(error) => timer.finish_error(error),
    }
  }
  result
}

fn invalid_manifest(error: impl std::fmt::Display) -> StorageError {
  StorageError::InvalidManifest(error.to_string())
}

fn format_error(error: FormatError) -> StorageError {
  match error {
    FormatError::UnsupportedSchema { found, supported } => {
      StorageError::UnsupportedSchema { found, supported }
    }
    other => StorageError::InvalidManifest(other.to_string()),
  }
}

#[cfg(test)]
mod tests {
  use chrono::{TimeZone, Utc};
  use common::{CapturedDisplay, GlobalBoundsPx, SizePx};

  use super::*;
  use crate::storage::atomic::with_injected_fault;

  struct TestDirectory(PathBuf);

  impl TestDirectory {
    fn new(name: &str) -> Self {
      let path = std::env::temp_dir().join(format!("rs-board-{name}-{}", Uuid::new_v4()));
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

  fn context() -> PersistenceContext {
    PersistenceContext::new(Uuid::new_v4(), Uuid::new_v4())
  }

  fn document(document_id: DocumentId) -> BoardDocument {
    BoardDocument::new_capture(
      document_id,
      SizePx::new(2, 2),
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx { x_px: 0, y_px: 0, width_px: 2, height_px: 2 },
        scale_factor: 1.0,
      },
      Utc.with_ymd_and_hms(2026, 8, 6, 12, 0, 0).unwrap(),
    )
    .unwrap()
  }

  fn background(red: u8) -> BackgroundData {
    let mut pixels = Vec::with_capacity(16);
    for _ in 0..4 {
      pixels.extend_from_slice(&[red, 20, 30, 255]);
    }
    BackgroundData::rgba8(2, 2, pixels).unwrap()
  }

  fn save_request(document: &BoardDocument, red: u8) -> SaveRequest {
    SaveRequest {
      context: context(),
      snapshot: document.snapshot(document.revision).unwrap(),
      background: background(red),
    }
  }

  fn stash_request(document: &BoardDocument, red: u8) -> StashRequest {
    StashRequest {
      context: context(),
      generation_id: GenerationId::new(),
      snapshot: document.snapshot(document.revision).unwrap(),
      background: background(red),
    }
  }

  #[test]
  fn draft_commit_faults_never_destroy_the_last_recoverable_generation() {
    let fault_points = [
      ("persistence.file.write", "background"),
      ("persistence.file.sync", "background"),
      ("persistence.file.write", "manifest"),
      ("persistence.file.write", "slot"),
      ("persistence.file.write", "commit_marker"),
      ("persistence.directory.sync", "staging_directory"),
      (
        if cfg!(target_os = "macos") {
          "persistence.directory.swap"
        } else {
          "persistence.directory.rename"
        },
        "latest_draft",
      ),
      ("persistence.directory.sync", "parent_directory"),
    ];

    for (stage, resource) in fault_points {
      let (_root, store) = store(&format!("draft-fault-{}-{resource}", stage.replace('.', "-")));
      let healthy = document(DocumentId::new());
      let healthy_request = stash_request(&healthy, 10);
      let healthy_generation = healthy_request.generation_id;
      store.replace_latest_draft(healthy_request).unwrap();

      let replacement = document(DocumentId::new());
      let replacement_request = stash_request(&replacement, 90);
      let replacement_generation = replacement_request.generation_id;
      let result =
        with_injected_fault(stage, resource, || store.replace_latest_draft(replacement_request));
      assert!(result.is_err(), "fault at {stage}/{resource} did not fire");

      let recovered = store.load_latest_draft().unwrap().expect("a healthy draft must remain");
      assert!(
        recovered.generation_id == healthy_generation
          || (stage == "persistence.directory.sync"
            && resource == "parent_directory"
            && recovered.generation_id == replacement_generation),
        "unexpected generation after {stage}/{resource}: {}",
        recovered.generation_id
      );
      assert!(recovered.loaded.background.decode_rgba8().is_ok());
      let staging_count = std::fs::read_dir(store.paths().draft_root())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-draft-"))
        .count();
      assert_eq!(staging_count, 0, "staging leaked after {stage}/{resource}");
    }
  }

  #[test]
  fn draft_round_trip_replaces_the_single_slot() {
    let (_root, store) = store("draft-round-trip");
    let first = document(DocumentId::new());
    let first_request = stash_request(&first, 10);
    let first_generation = first_request.generation_id;
    let first_result = store.replace_latest_draft(first_request).unwrap();
    assert_eq!(first_result.document_id, first.document_id);
    assert_eq!(store.load_latest_draft().unwrap().unwrap().generation_id, first_generation);

    let second = document(DocumentId::new());
    let second_request = stash_request(&second, 200);
    let second_generation = second_request.generation_id;
    store.replace_latest_draft(second_request).unwrap();
    let loaded = store.load_latest_draft().unwrap().unwrap();
    assert_eq!(loaded.generation_id, second_generation);
    assert_eq!(loaded.loaded.document.document_id, second.document_id);
    assert!(!store.delete_latest_if_generation(first_generation).unwrap());
    assert!(store.delete_latest_if_generation(second_generation).unwrap());
    assert!(store.load_latest_draft().unwrap().is_none());
  }

  #[test]
  fn save_open_scan_rename_and_delete_form_one_lifecycle() {
    let (_root, store) = store("document-lifecycle");
    let document = document(DocumentId::new());
    let request = save_request(&document, 80);
    let expected_context = request.context;
    let saved = store.save_document(request).unwrap();
    assert_eq!(saved.context, expected_context);
    assert_eq!(saved.revision, document.revision);
    assert_eq!(saved.summary.document_id, document.document_id);
    assert_eq!(saved.summary.title, document.title);
    assert_eq!(saved.summary.revision, document.revision);
    assert_eq!(saved.summary.updated_at, document.updated_at);
    assert_eq!(saved.summary.preview_revision, None);
    assert_eq!(saved.summary.preview_path, None);

    let opened = store.open_document(document.document_id).unwrap();
    assert_eq!(opened.document, document);
    assert_eq!(opened.background.decode_rgba8().unwrap().0, 2);
    let scan = store.scan_documents().unwrap();
    assert_eq!(scan.documents.len(), 1);
    assert!(scan.failures.is_empty());

    let renamed = store.rename_document(document.document_id, "重命名讲义").unwrap();
    assert_eq!(renamed.title, "重命名讲义");
    assert_eq!(store.open_document(document.document_id).unwrap().document.title, "重命名讲义");
    store.delete_document(document.document_id).unwrap();
    assert!(matches!(
      store.open_document(document.document_id),
      Err(StorageError::DocumentNotFound(_))
    ));
  }

  #[test]
  fn missing_source_directory_is_rebuilt_from_the_snapshot_background() {
    let (_root, store) = store("rebuild-document");
    let document = document(DocumentId::new());
    let saved = store.save_document(save_request(&document, 1)).unwrap();
    std::fs::remove_dir_all(&saved.directory_path).unwrap();

    store.save_document(save_request(&document, 220)).unwrap();

    let opened = store.open_document(document.document_id).unwrap();
    let (_, _, pixels) = opened.background.decode_rgba8().unwrap();
    assert_eq!(pixels[0], 220);
  }

  #[test]
  fn future_schema_is_not_overwritten() {
    let (_root, store) = store("future-schema");
    let document = document(DocumentId::new());
    let saved = store.save_document(save_request(&document, 1)).unwrap();
    let mut json: serde_json::Value =
      serde_json::from_slice(&std::fs::read(&saved.manifest_path).unwrap()).unwrap();
    json["schema_version"] = 999.into();
    std::fs::write(&saved.manifest_path, serde_json::to_vec(&json).unwrap()).unwrap();

    assert!(store.save_document(save_request(&document, 2)).is_err());

    let unchanged: serde_json::Value =
      serde_json::from_slice(&std::fs::read(&saved.manifest_path).unwrap()).unwrap();
    assert_eq!(unchanged["schema_version"], 999);
  }

  #[test]
  fn stale_preview_result_is_ignored() {
    let (_root, store) = store("preview-revision");
    let document = document(DocumentId::new());
    let saved = store.save_document(save_request(&document, 1)).unwrap();
    let preview = background(42).normalized_png(2, 2).unwrap();

    assert_eq!(
      store
        .install_preview_if_current(
          document.document_id,
          document.revision + 1,
          Arc::<[u8]>::from(&b"not a png"[..]),
        )
        .unwrap(),
      None
    );
    std::fs::write(&saved.background_path, b"not a png").unwrap();
    let preview_path = store
      .install_preview_if_current(document.document_id, document.revision, preview)
      .unwrap()
      .expect("current preview should be installed without decoding the background");
    assert_eq!(preview_path, saved.directory_path.join(&document.preview_file));

    let manifest = decode_document(&std::fs::read(&saved.manifest_path).unwrap()).unwrap();
    assert_eq!(manifest.preview_revision, Some(document.revision));
    assert!(matches!(
      store.open_document(document.document_id),
      Err(StorageError::InvalidImage(_))
    ));
  }

  #[test]
  fn export_rewrites_resource_names_and_import_creates_a_new_document() {
    let (_root, store) = store("import-export");
    let export_root = TestDirectory::new("export-destination");
    let document = document(DocumentId::new());
    store.save_document(save_request(&document, 90)).unwrap();

    let first = store.export_document(document.document_id, export_root.path()).unwrap();
    let second = store.export_document(document.document_id, export_root.path()).unwrap();
    assert_eq!(second.stem, format!("{}-2", first.stem));
    let exported = decode_document(&std::fs::read(&first.manifest_path).unwrap()).unwrap();
    assert_eq!(exported.background.file, format!("{}.png", first.stem));

    let imported = store
      .import_document(ImportRequest { context: context(), manifest_path: first.manifest_path })
      .unwrap();
    assert_ne!(imported.saved.document_id, document.document_id);
    assert_eq!(imported.committed_summary().document_id, imported.saved.document_id);
    let loaded = store.open_document(imported.saved.document_id).unwrap();
    assert_eq!(loaded.document.background.file, format!("{}.png", imported.saved.document_id));
    assert_eq!(store.scan_documents().unwrap().documents.len(), 2);
  }

  #[test]
  fn import_rejects_parent_directory_resource_reference() {
    let (_root, store) = store("unsafe-import");
    let source = TestDirectory::new("unsafe-import-source");
    let document = document(DocumentId::new());
    let mut json = serde_json::to_value(document).unwrap();
    json["background"]["file"] = "../outside.png".into();
    let manifest = source.path().join("unsafe.rsboard");
    std::fs::write(&manifest, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(
      store.import_document(ImportRequest { context: context(), manifest_path: manifest }).is_err()
    );
  }

  #[test]
  fn startup_recovers_a_complete_draft_staging_directory() {
    let (root, store) = store("draft-recovery");
    let document = document(DocumentId::new());
    let generation = stash_request(&document, 8);
    let expected_generation = generation.generation_id;
    store.replace_latest_draft(generation).unwrap();
    let staged = store.paths().draft_root().join(".tmp-draft-crash");
    std::fs::rename(store.paths().latest_draft(), &staged).unwrap();
    drop(store);

    let (reopened, report) = LocalStore::at_root(root.path()).unwrap();

    assert!(report.recovered_draft);
    assert_eq!(reopened.load_latest_draft().unwrap().unwrap().generation_id, expected_generation);
  }

  #[test]
  fn startup_removes_incomplete_staging_directories() {
    let root = TestDirectory::new("incomplete-recovery");
    let paths = StorePaths::new(root.path());
    paths.ensure_layout().unwrap();
    let incomplete = paths.draft_root().join(".tmp-draft-incomplete");
    std::fs::create_dir(&incomplete).unwrap();
    std::fs::write(incomplete.join("partial"), b"partial").unwrap();

    let (_store, report) = LocalStore::open(paths).unwrap();

    assert!(!incomplete.exists());
    assert_eq!(report.removed_artifacts, 1);
  }

  #[test]
  fn startup_recovers_a_complete_document_staging_directory() {
    let (root, store) = store("document-recovery");
    let document = document(DocumentId::new());
    let saved = store.save_document(save_request(&document, 33)).unwrap();
    let staged = store.paths().documents_root().join(".tmp-document-crash");
    std::fs::rename(&saved.directory_path, &staged).unwrap();
    drop(store);

    let (reopened, report) = LocalStore::at_root(root.path()).unwrap();

    assert_eq!(report.recovered_documents, 1);
    assert_eq!(reopened.open_document(document.document_id).unwrap().document, document);
  }

  #[test]
  fn scan_isolates_a_corrupt_document() {
    let (_root, store) = store("scan-corruption");
    let healthy = document(DocumentId::new());
    let corrupt = document(DocumentId::new());
    store.save_document(save_request(&healthy, 1)).unwrap();
    let corrupt_saved = store.save_document(save_request(&corrupt, 2)).unwrap();
    std::fs::write(&corrupt_saved.manifest_path, b"not json").unwrap();

    let scan = store.scan_documents().unwrap();

    assert_eq!(scan.documents.len(), 1);
    assert_eq!(scan.documents[0].document_id, healthy.document_id);
    assert_eq!(scan.failures.len(), 1);
    assert_eq!(scan.failures[0].entry_name, corrupt.document_id.to_string());
  }

  #[test]
  fn open_rejects_a_corrupt_background() {
    let (_root, store) = store("corrupt-background");
    let document = document(DocumentId::new());
    let saved = store.save_document(save_request(&document, 1)).unwrap();
    std::fs::write(&saved.background_path, b"not a png").unwrap();

    assert!(matches!(
      store.open_document(document.document_id),
      Err(StorageError::InvalidImage(_))
    ));
  }

  #[test]
  fn summaries_and_compatible_scan_do_not_read_a_corrupt_background() {
    let (_root, store) = store("summary-corrupt-background");
    let document = document(DocumentId::new());
    let saved = store.save_document(save_request(&document, 1)).unwrap();
    std::fs::write(&saved.background_path, b"not a png").unwrap();

    let skeletons = store.scan_document_skeletons().unwrap();
    assert_eq!(skeletons.skeletons.len(), 1);
    assert!(skeletons.failures.is_empty());
    let summary = store.load_document_summary(&skeletons.skeletons[0]).unwrap();
    assert_eq!(summary.document_id, document.document_id);
    assert_eq!(summary.manifest_fingerprint, skeletons.skeletons[0].manifest_fingerprint);

    let scan = store.scan_documents().unwrap();
    assert_eq!(scan.documents.len(), 1);
    assert!(scan.failures.is_empty());
    assert!(matches!(
      store.open_document(document.document_id),
      Err(StorageError::InvalidImage(_))
    ));
  }

  #[test]
  fn skeleton_scan_does_not_wait_for_the_store_gate() {
    let (_root, store) = store("unlocked-skeleton-scan");
    let guard = store.gate.lock().unwrap();
    let scanning_store = store.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
      let _ = sender.send(scanning_store.scan_document_skeletons());
    });

    let result = receiver.recv_timeout(std::time::Duration::from_secs(1));
    drop(guard);
    worker.join().unwrap();

    assert!(result.expect("skeleton scan was blocked by the store gate").is_ok());
  }

  #[test]
  fn summary_does_not_open_a_declared_preview() {
    let (_root, store) = store("summary-missing-preview");
    let document = document(DocumentId::new());
    let saved = store.save_document(save_request(&document, 1)).unwrap();
    let preview = background(42).normalized_png(2, 2).unwrap();
    let preview_path = store
      .install_preview_if_current(document.document_id, document.revision, preview)
      .unwrap()
      .unwrap();
    std::fs::remove_file(&preview_path).unwrap();

    let summary = store.load_document_summary_by_id(document.document_id).unwrap();
    assert_eq!(summary.preview_revision, Some(document.revision));
    assert_eq!(summary.preview_path, Some(preview_path));
    assert_eq!(store.open_document(document.document_id).unwrap().preview_path, None);
    assert!(saved.background_path.exists());
  }

  #[test]
  fn rename_only_needs_a_valid_manifest() {
    let (_root, store) = store("rename-corrupt-background");
    let document = document(DocumentId::new());
    let saved = store.save_document(save_request(&document, 1)).unwrap();
    std::fs::write(&saved.background_path, b"not a png").unwrap();

    let renamed = store.rename_document(document.document_id, "仍可重命名").unwrap();
    assert_eq!(renamed.title, "仍可重命名");
    assert!(matches!(
      store.open_document(document.document_id),
      Err(StorageError::InvalidImage(_))
    ));
  }

  #[cfg(unix)]
  #[test]
  fn skeleton_scan_skips_hidden_entries_and_rejects_invalid_and_symlink_entries() {
    use std::os::unix::fs::symlink;

    let (_root, store) = store("skeleton-filtering");
    let document = document(DocumentId::new());
    let saved = store.save_document(save_request(&document, 1)).unwrap();
    std::fs::create_dir(store.paths().documents_root().join(".hidden")).unwrap();
    std::fs::create_dir(store.paths().documents_root().join("not-a-document-id")).unwrap();
    let symlink_id = DocumentId::new();
    symlink(&saved.directory_path, store.paths().documents_root().join(symlink_id.to_string()))
      .unwrap();

    let scan = store.scan_document_skeletons().unwrap();

    assert_eq!(scan.skeletons.len(), 1);
    assert_eq!(scan.skeletons[0].document_id, document.document_id);
    assert_eq!(scan.failures.len(), 2);
    assert!(scan.failures.iter().any(|failure| failure.entry_name == "not-a-document-id"));
    assert!(scan.failures.iter().any(|failure| failure.entry_name == symlink_id.to_string()));
    assert!(!scan.failures.iter().any(|failure| failure.entry_name == ".hidden"));
  }

  #[test]
  fn clear_all_removes_documents_and_latest_draft() {
    let (_root, store) = store("clear-all");
    let document = document(DocumentId::new());
    store.save_document(save_request(&document, 1)).unwrap();
    store.replace_latest_draft(stash_request(&document, 1)).unwrap();

    store.clear_all_content().unwrap();

    assert!(store.scan_documents().unwrap().documents.is_empty());
    assert!(store.load_latest_draft().unwrap().is_none());
  }
}
