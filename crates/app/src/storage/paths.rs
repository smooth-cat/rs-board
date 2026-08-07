use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use super::{StorageError, StorageResult};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorePaths {
  root: PathBuf,
  draft_root: PathBuf,
  latest_draft: PathBuf,
  documents_root: PathBuf,
}

impl StorePaths {
  pub fn for_current_user() -> StorageResult<Self> {
    let project =
      ProjectDirs::from("com", "linjiajian", "RS Board").ok_or(StorageError::AppDataUnavailable)?;
    Ok(Self::new(project.data_dir()))
  }

  pub fn new(root: impl Into<PathBuf>) -> Self {
    let root = root.into();
    // 草稿固定保存在 draft/latest；正式讲义按 document UUID 分目录保存在
    // documents/<document-uuid>/<document-uuid>.rsboard，并带有背景图和预览图。
    let draft_root = root.join("draft");
    let latest_draft = draft_root.join("latest");
    let documents_root = root.join("documents");
    Self { root, draft_root, latest_draft, documents_root }
  }

  pub fn ensure_layout(&self) -> StorageResult<()> {
    std::fs::create_dir_all(&self.draft_root)
      .map_err(|error| StorageError::io("creating the draft directory", error))?;
    std::fs::create_dir_all(&self.documents_root)
      .map_err(|error| StorageError::io("creating the documents directory", error))?;
    Ok(())
  }

  pub fn root(&self) -> &Path {
    &self.root
  }

  pub fn draft_root(&self) -> &Path {
    &self.draft_root
  }

  pub fn latest_draft(&self) -> &Path {
    &self.latest_draft
  }

  pub fn documents_root(&self) -> &Path {
    &self.documents_root
  }

  pub(crate) fn document_dir(&self, document_id: common::DocumentId) -> PathBuf {
    self.documents_root.join(document_id.to_string())
  }
}
