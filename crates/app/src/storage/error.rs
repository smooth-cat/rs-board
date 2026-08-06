use std::{io, path::PathBuf};

use thiserror::Error;

pub type StorageResult<T> = Result<T, StorageError>;

#[derive(Debug, Error)]
pub enum StorageError {
  #[error("application data directory is unavailable")]
  AppDataUnavailable,

  #[error("I/O error while {action}")]
  Io {
    action: &'static str,
    #[source]
    source: io::Error,
  },

  #[error("unsafe resource name: {0}")]
  UnsafeResourceName(String),

  #[error("resource is missing: {0}")]
  MissingResource(String),

  #[error("resource is not a regular file: {0}")]
  InvalidResourceType(String),

  #[error("resource changed while it was being opened: {0}")]
  ResourceChanged(String),

  #[error("invalid manifest: {0}")]
  InvalidManifest(String),

  #[error("unsupported schema version {found}; maximum supported version is {supported}")]
  UnsupportedSchema { found: u32, supported: u32 },

  #[error("invalid image: {0}")]
  InvalidImage(String),

  #[error("document not found: {0}")]
  DocumentNotFound(String),

  #[error("latest draft does not exist")]
  DraftNotFound,

  #[error("storage destination already exists: {0}")]
  AlreadyExists(PathBuf),

  #[error("storage operation would target an application-managed directory")]
  ManagedDestination,

  #[error("storage commit is incomplete: {0}")]
  IncompleteCommit(String),
}

impl StorageError {
  pub(crate) fn io(action: &'static str, source: io::Error) -> Self {
    Self::Io { action, source }
  }
}
