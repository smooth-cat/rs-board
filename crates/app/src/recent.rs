use common::DocumentId;

use crate::storage::{DocumentSummary, LocalStore, ScanFailure, ScanResult, StorageResult};

#[derive(Debug, Default)]
pub struct RecentDocuments {
  pub query: String,
  pub documents: Vec<DocumentSummary>,
  pub failures: Vec<ScanFailure>,
  pub highlighted: Option<DocumentId>,
}

impl RecentDocuments {
  pub fn refresh(&mut self, store: &LocalStore) -> StorageResult<()> {
    self.apply_scan(store.scan_documents()?);
    Ok(())
  }

  pub fn apply_scan(&mut self, scan: ScanResult) {
    self.documents = scan.documents;
    self.failures = scan.failures;
    if self
      .highlighted
      .is_some_and(|id| !self.documents.iter().any(|document| document.document_id == id))
    {
      self.highlighted = None;
    }
  }

  pub fn visible_documents(&self) -> impl Iterator<Item = &DocumentSummary> {
    let needle = self.query.trim().to_lowercase();
    self
      .documents
      .iter()
      .filter(move |document| needle.is_empty() || document.title.to_lowercase().contains(&needle))
  }

  pub fn highlight(&mut self, document_id: DocumentId) {
    self.highlighted = Some(document_id);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::{TimeZone, Utc};

  fn summary(title: &str, day: u32) -> DocumentSummary {
    DocumentSummary {
      document_id: DocumentId::new(),
      title: title.to_owned(),
      revision: 0,
      updated_at: Utc.with_ymd_and_hms(2026, 8, day, 0, 0, 0).unwrap(),
      preview_path: None,
    }
  }

  #[test]
  fn search_is_immediate_and_case_insensitive() {
    let mut recent = RecentDocuments {
      documents: vec![summary("API Review", 1), summary("中文讲义", 2)],
      query: "api".into(),
      ..RecentDocuments::default()
    };
    assert_eq!(recent.visible_documents().count(), 1);
    recent.query = "讲义".into();
    assert_eq!(recent.visible_documents().next().unwrap().title, "中文讲义");
  }
}
