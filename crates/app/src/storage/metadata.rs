use std::str::FromStr;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{StorageError, StorageResult};

pub(crate) const SLOT_FILE_NAME: &str = ".slot.json";
pub(crate) const COMMIT_MARKER_FILE_NAME: &str = ".commit-ready.json";
const STORAGE_METADATA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct GenerationId(Uuid);

impl GenerationId {
  pub fn new() -> Self {
    Self(Uuid::new_v4())
  }
}

impl Default for GenerationId {
  fn default() -> Self {
    Self::new()
  }
}

impl std::fmt::Display for GenerationId {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    self.0.fmt(formatter)
  }
}

impl FromStr for GenerationId {
  type Err = uuid::Error;

  fn from_str(value: &str) -> Result<Self, Self::Err> {
    Uuid::parse_str(value).map(Self)
  }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SlotMetadata {
  pub version: u32,
  pub generation_id: GenerationId,
  pub document_id: String,
  pub revision: u64,
}

impl SlotMetadata {
  pub fn new(generation_id: GenerationId, document_id: String, revision: u64) -> Self {
    Self { version: STORAGE_METADATA_VERSION, generation_id, document_id, revision }
  }

  pub fn encode(&self) -> StorageResult<Vec<u8>> {
    serde_json::to_vec_pretty(self)
      .map_err(|error| StorageError::InvalidManifest(error.to_string()))
  }

  pub fn decode(bytes: &[u8]) -> StorageResult<Self> {
    let metadata: Self = serde_json::from_slice(bytes)
      .map_err(|error| StorageError::InvalidManifest(error.to_string()))?;
    if metadata.version != STORAGE_METADATA_VERSION {
      return Err(StorageError::InvalidManifest(format!(
        "unsupported draft slot metadata version {}",
        metadata.version
      )));
    }
    Ok(metadata)
  }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommitKind {
  Draft,
  Document,
  Import,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitMarker {
  pub version: u32,
  pub kind: CommitKind,
  pub document_id: String,
  pub revision: u64,
  pub generation_id: Option<GenerationId>,
  pub created_at_millis: i64,
}

impl CommitMarker {
  pub fn new(
    kind: CommitKind,
    document_id: String,
    revision: u64,
    generation_id: Option<GenerationId>,
  ) -> Self {
    Self {
      version: STORAGE_METADATA_VERSION,
      kind,
      document_id,
      revision,
      generation_id,
      created_at_millis: Utc::now().timestamp_millis(),
    }
  }

  pub fn encode(&self) -> StorageResult<Vec<u8>> {
    serde_json::to_vec_pretty(self)
      .map_err(|error| StorageError::InvalidManifest(error.to_string()))
  }

  pub fn decode(bytes: &[u8]) -> StorageResult<Self> {
    let marker: Self = serde_json::from_slice(bytes)
      .map_err(|error| StorageError::InvalidManifest(error.to_string()))?;
    if marker.version != STORAGE_METADATA_VERSION {
      return Err(StorageError::InvalidManifest(format!(
        "unsupported commit marker version {}",
        marker.version
      )));
    }
    Ok(marker)
  }
}
