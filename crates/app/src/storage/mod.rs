mod atomic;
mod bundle_name;
mod error;
mod image_data;
mod io;
mod metadata;
mod paths;
mod resource;
mod store;

pub(crate) use atomic::write_file_atomically;
pub use error::{StorageError, StorageResult};
pub use image_data::BackgroundData;
pub use metadata::GenerationId;
pub use paths::StorePaths;
pub use resource::{ResourceName, open_regular_file, open_regular_path};
pub use store::{
  DocumentSkeleton, DocumentSummary, ExportedBundle, ImportRequest, ImportedDocument, LatestDraft,
  LoadedDocument, LoadedDraft, LocalStore, ManifestFingerprint, PersistenceContext, RecoveryReport,
  SaveRequest, SavedDocument, ScanFailure, ScanResult, SkeletonScanResult, StashRequest,
};
