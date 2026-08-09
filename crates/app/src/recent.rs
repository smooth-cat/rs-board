use std::{
  cmp::Ordering,
  collections::{HashMap, HashSet},
  ops::Range,
  path::PathBuf,
  time::{SystemTime, UNIX_EPOCH},
};

use common::{DocumentId, Revision};

use crate::storage::{
  DocumentSkeleton, DocumentSummary, LocalStore, ScanFailure, ScanResult, StorageResult,
};

#[derive(Debug, Clone)]
pub struct LibraryEntry {
  pub document_id: DocumentId,
  pub summary: Option<DocumentSummary>,
  pub metadata_error: Option<String>,
  fallback_modified_at: Option<SystemTime>,
  search_key: String,
  scan_generation: Option<u64>,
  local_epoch: u64,
}

impl LibraryEntry {
  pub fn is_loading(&self) -> bool {
    self.summary.is_none() && self.metadata_error.is_none()
  }
}

#[derive(Debug)]
pub struct RecentDocuments {
  pub query: String,
  entries: HashMap<DocumentId, LibraryEntry>,
  ordered_ids: Vec<DocumentId>,
  filtered_ids: Vec<DocumentId>,
  applied_query_source: String,
  applied_query: String,
  view_dirty: bool,
  pub failures: Vec<ScanFailure>,
  pub highlighted: Option<DocumentId>,
  next_local_epoch: u64,
  tombstones: HashSet<DocumentId>,
  background_results_disabled: bool,
  #[cfg(test)]
  order_rebuild_count: usize,
  #[cfg(test)]
  filter_rebuild_count: usize,
}

impl Default for RecentDocuments {
  fn default() -> Self {
    Self {
      query: String::new(),
      entries: HashMap::new(),
      ordered_ids: Vec::new(),
      filtered_ids: Vec::new(),
      applied_query_source: String::new(),
      applied_query: String::new(),
      view_dirty: false,
      failures: Vec::new(),
      highlighted: None,
      next_local_epoch: 1,
      tombstones: HashSet::new(),
      background_results_disabled: false,
      #[cfg(test)]
      order_rebuild_count: 0,
      #[cfg(test)]
      filter_rebuild_count: 0,
    }
  }
}

impl RecentDocuments {
  pub fn refresh(&mut self, store: &LocalStore) -> StorageResult<()> {
    self.apply_scan(store.scan_documents()?);
    Ok(())
  }

  pub fn apply_scan(&mut self, scan: ScanResult) {
    self.background_results_disabled = false;
    self.entries.clear();
    self.tombstones.clear();
    for summary in scan.documents {
      self.insert_summary(summary, None, 0);
    }
    self.failures = scan.failures;
    self.view_dirty = true;
    self.retain_valid_highlight();
  }

  pub fn begin_scan(
    &mut self,
    generation: u64,
    skeletons: &[DocumentSkeleton],
    failures: Vec<ScanFailure>,
  ) {
    if self.background_results_disabled {
      return;
    }
    let mut present = HashSet::with_capacity(skeletons.len());
    for skeleton in skeletons {
      let document_id = skeleton.document_id;
      present.insert(document_id);
      if self.tombstones.contains(&document_id) {
        continue;
      }
      match self.entries.get_mut(&document_id) {
        Some(entry) if entry.local_epoch == 0 => {
          entry.fallback_modified_at = skeleton.manifest_fingerprint.modified_at;
          entry.scan_generation = Some(generation);
        }
        Some(_) => {}
        None => {
          self.entries.insert(
            document_id,
            LibraryEntry {
              document_id,
              summary: None,
              metadata_error: None,
              fallback_modified_at: skeleton.manifest_fingerprint.modified_at,
              search_key: String::new(),
              scan_generation: Some(generation),
              local_epoch: 0,
            },
          );
        }
      }
    }
    self
      .entries
      .retain(|document_id, entry| entry.local_epoch != 0 || present.contains(document_id));
    self.failures = failures;
    self.view_dirty = true;
    self.retain_valid_highlight();
  }

  pub fn apply_cached_summary(&mut self, generation: u64, summary: DocumentSummary) -> bool {
    self.apply_background_summary(generation, summary)
  }

  pub fn apply_hydrated_summary(&mut self, generation: u64, summary: DocumentSummary) -> bool {
    self.apply_background_summary(generation, summary)
  }

  pub fn fail_hydration(
    &mut self,
    generation: u64,
    document_id: DocumentId,
    message: String,
  ) -> bool {
    if self.background_results_disabled {
      return false;
    }
    let Some(entry) = self.entries.get_mut(&document_id) else {
      return false;
    };
    if entry.local_epoch != 0 || entry.scan_generation != Some(generation) {
      return false;
    }
    entry.summary = None;
    entry.metadata_error = Some(message);
    entry.search_key.clear();
    self.view_dirty = true;
    true
  }

  pub fn upsert(&mut self, summary: DocumentSummary) {
    let epoch = self.take_local_epoch();
    self.tombstones.remove(&summary.document_id);
    self.insert_summary(summary, None, epoch);
    self.view_dirty = true;
  }

  pub fn remove(&mut self, document_id: DocumentId) -> bool {
    self.take_local_epoch();
    self.tombstones.insert(document_id);
    let removed = self.entries.remove(&document_id).is_some();
    if self.highlighted == Some(document_id) {
      self.highlighted = None;
    }
    if removed {
      self.view_dirty = true;
    }
    removed
  }

  pub fn clear(&mut self) {
    self.entries.clear();
    self.ordered_ids.clear();
    self.filtered_ids.clear();
    self.failures.clear();
    self.highlighted = None;
    self.tombstones.clear();
    self.background_results_disabled = true;
    self.view_dirty = false;
  }

  pub fn mark_preview_ready(
    &mut self,
    document_id: DocumentId,
    revision: Revision,
    preview_path: PathBuf,
  ) -> bool {
    let Some(summary) = self.entries.get_mut(&document_id).and_then(|entry| entry.summary.as_mut())
    else {
      return false;
    };
    if summary.revision != revision {
      return false;
    }
    summary.preview_revision = Some(revision);
    summary.preview_path = Some(preview_path);
    true
  }

  pub fn prepare_view(&mut self) -> bool {
    let normalized_query =
      (self.query != self.applied_query_source).then(|| normalize_query(&self.query));
    let query_changed = normalized_query.as_ref().is_some_and(|query| query != &self.applied_query);
    let rebuild_filter = self.view_dirty || query_changed;
    if self.view_dirty {
      self.rebuild_order();
    }
    if let Some(normalized_query) = normalized_query {
      self.applied_query_source.clone_from(&self.query);
      if query_changed {
        self.applied_query = normalized_query;
      }
    }
    if rebuild_filter {
      self.filtered_ids.clear();
      self.filtered_ids.reserve(self.ordered_ids.len());
      for document_id in &self.ordered_ids {
        let Some(entry) = self.entries.get(document_id) else {
          continue;
        };
        if self.applied_query.is_empty() || entry.search_key.contains(&self.applied_query) {
          self.filtered_ids.push(*document_id);
        }
      }
      #[cfg(test)]
      {
        self.filter_rebuild_count += 1;
      }
      self.view_dirty = false;
    }
    rebuild_filter
  }

  pub fn visible_count(&self) -> usize {
    self.filtered_ids.len()
  }

  pub fn visible_id(&self, index: usize) -> Option<DocumentId> {
    self.filtered_ids.get(index).copied()
  }

  pub fn visible_ids(&self, range: Range<usize>) -> Vec<DocumentId> {
    let start = range.start.min(self.filtered_ids.len());
    let end = range.end.min(self.filtered_ids.len());
    if start >= end {
      return Vec::new();
    }
    self.filtered_ids[start..end].to_vec()
  }

  pub fn entry(&self, document_id: DocumentId) -> Option<&LibraryEntry> {
    self.entries.get(&document_id)
  }

  pub fn summary(&self, document_id: DocumentId) -> Option<&DocumentSummary> {
    self.entries.get(&document_id).and_then(|entry| entry.summary.as_ref())
  }

  pub fn summaries(&self) -> impl Iterator<Item = &DocumentSummary> {
    self.entries.values().filter_map(|entry| entry.summary.as_ref())
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  pub fn highlight(&mut self, document_id: DocumentId) {
    if self.entries.contains_key(&document_id) {
      self.highlighted = Some(document_id);
    }
  }

  fn apply_background_summary(&mut self, generation: u64, summary: DocumentSummary) -> bool {
    if self.background_results_disabled {
      return false;
    }
    let document_id = summary.document_id;
    if self.tombstones.contains(&document_id) {
      return false;
    }
    let Some(entry) = self.entries.get(&document_id) else {
      return false;
    };
    if entry.local_epoch != 0 || entry.scan_generation != Some(generation) {
      return false;
    }
    self.insert_summary(summary, Some(generation), 0);
    self.view_dirty = true;
    true
  }

  fn insert_summary(
    &mut self,
    summary: DocumentSummary,
    scan_generation: Option<u64>,
    local_epoch: u64,
  ) {
    let document_id = summary.document_id;
    let fallback_modified_at = summary.manifest_fingerprint.modified_at;
    let search_key = normalize_query(&summary.title);
    self.entries.insert(
      document_id,
      LibraryEntry {
        document_id,
        summary: Some(summary),
        metadata_error: None,
        fallback_modified_at,
        search_key,
        scan_generation,
        local_epoch,
      },
    );
  }

  fn rebuild_order(&mut self) {
    let entries = &self.entries;
    self.ordered_ids.clear();
    self.ordered_ids.extend(entries.keys().copied());
    self.ordered_ids.sort_by(|left_id, right_id| {
      let left = &entries[left_id];
      let right = &entries[right_id];
      compare_entries(left, right)
    });
    #[cfg(test)]
    {
      self.order_rebuild_count += 1;
    }
  }

  fn take_local_epoch(&mut self) -> u64 {
    let epoch = self.next_local_epoch;
    self.next_local_epoch = self.next_local_epoch.saturating_add(1);
    epoch
  }

  fn retain_valid_highlight(&mut self) {
    if self.highlighted.is_some_and(|id| !self.entries.contains_key(&id)) {
      self.highlighted = None;
    }
  }
}

fn normalize_query(value: &str) -> String {
  value.trim().to_lowercase()
}

fn compare_entries(left: &LibraryEntry, right: &LibraryEntry) -> Ordering {
  entry_sort_time(right)
    .cmp(&entry_sort_time(left))
    .then_with(|| left.document_id.as_uuid().cmp(&right.document_id.as_uuid()))
}

fn entry_sort_time(entry: &LibraryEntry) -> i128 {
  if let Some(summary) = &entry.summary {
    return summary
      .updated_at
      .timestamp_nanos_opt()
      .map(i128::from)
      .unwrap_or_else(|| i128::from(summary.updated_at.timestamp()) * 1_000_000_000);
  }
  entry
    .fallback_modified_at
    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
    .map(|duration| i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX))
    .unwrap_or(i128::MIN)
}

#[cfg(test)]
mod tests {
  use chrono::{TimeZone, Utc};

  use super::*;
  use crate::storage::ManifestFingerprint;

  fn summary(title: &str, day: u32, revision: Revision) -> DocumentSummary {
    DocumentSummary {
      document_id: DocumentId::new(),
      title: title.to_owned(),
      revision,
      updated_at: Utc.with_ymd_and_hms(2026, 8, day, 0, 0, 0).unwrap(),
      preview_revision: None,
      preview_path: None,
      manifest_fingerprint: ManifestFingerprint::default(),
    }
  }

  fn summary_at(title: &str, timestamp: i64, revision: Revision) -> DocumentSummary {
    let mut summary = summary(title, 1, revision);
    summary.updated_at = Utc.timestamp_opt(timestamp, 0).single().unwrap();
    summary
  }

  fn skeleton(document_id: DocumentId) -> DocumentSkeleton {
    DocumentSkeleton { document_id, manifest_fingerprint: ManifestFingerprint::default() }
  }

  fn prepared_recent(count: usize) -> RecentDocuments {
    let mut recent = RecentDocuments::default();
    for index in 0..count {
      recent.upsert(summary_at(&format!("讲义 {index}"), index as i64, 0));
    }
    recent.prepare_view();
    recent
  }

  #[test]
  fn search_is_cached_between_data_or_query_changes() {
    let mut recent = RecentDocuments::default();
    recent.upsert(summary("API Review", 1, 0));
    recent.upsert(summary("中文讲义", 2, 0));
    recent.query = "api".into();
    recent.prepare_view();
    assert_eq!(recent.visible_count(), 1);
    let first = recent.visible_id(0).unwrap();

    recent.prepare_view();
    assert_eq!(recent.visible_id(0), Some(first));
    recent.query = "讲义".into();
    recent.prepare_view();
    assert_eq!(recent.visible_count(), 1);
    assert_eq!(recent.summary(recent.visible_id(0).unwrap()).unwrap().title, "中文讲义");
  }

  #[test]
  fn visible_ranges_clamp_empty_single_and_odd_two_column_rows() {
    let mut empty = RecentDocuments::default();
    empty.prepare_view();
    assert_eq!(empty.visible_count(), 0);
    assert_eq!(empty.visible_id(0), None);
    assert!(empty.visible_ids(0..2).is_empty());
    assert!(empty.visible_ids(Range { start: 1, end: 0 }).is_empty());

    let single = prepared_recent(1);
    let only = single.visible_id(0).unwrap();
    assert_eq!(single.visible_ids(0..2), vec![only]);
    assert_eq!(single.visible_id(1), None);
    assert!(single.visible_ids(1..2).is_empty());

    let odd = prepared_recent(5);
    let all = odd.visible_ids(0..usize::MAX);
    assert_eq!(all.len(), 5);
    assert_eq!(odd.visible_ids(4..6), vec![all[4]]);
    assert_eq!(odd.visible_id(4), Some(all[4]));
    assert_eq!(odd.visible_id(5), None);
    assert!(odd.visible_ids(6..8).is_empty());
    assert!(odd.visible_ids(Range { start: 4, end: 3 }).is_empty());
  }

  #[test]
  fn ten_thousand_entries_rebuild_only_after_query_or_data_changes() {
    let mut recent = RecentDocuments::default();
    let mut target_id = None;
    for index in 0..10_000 {
      let document = summary_at(&format!("Lecture {index:05}"), index as i64, 0);
      if index == 9_999 {
        target_id = Some(document.document_id);
      }
      recent.upsert(document);
    }
    let target_id = target_id.unwrap();

    recent.prepare_view();
    assert_eq!(recent.visible_count(), 10_000);
    assert!(recent.visible_id(9_999).is_some());
    assert_eq!(recent.visible_id(10_000), None);
    assert_eq!(recent.visible_ids(9_998..10_002).len(), 2);
    assert!(recent.visible_ids(10_000..10_001).is_empty());
    assert!(recent.visible_ids(Range { start: 10_001, end: 9_999 }).is_empty());
    assert_eq!(recent.order_rebuild_count, 1);
    assert_eq!(recent.filter_rebuild_count, 1);

    for _ in 0..32 {
      recent.prepare_view();
    }
    assert_eq!(recent.order_rebuild_count, 1);
    assert_eq!(recent.filter_rebuild_count, 1);

    recent.query = "lecture 09999".into();
    recent.prepare_view();
    assert_eq!(recent.visible_count(), 1);
    assert_eq!(recent.visible_id(0), Some(target_id));
    assert_eq!(recent.order_rebuild_count, 1);
    assert_eq!(recent.filter_rebuild_count, 2);

    recent.query = "  LECTURE 09999  ".into();
    recent.prepare_view();
    assert_eq!(recent.visible_id(0), Some(target_id));
    assert_eq!(recent.order_rebuild_count, 1);
    assert_eq!(recent.filter_rebuild_count, 2);

    let mut updated = recent.summary(target_id).unwrap().clone();
    updated.updated_at = Utc.timestamp_opt(20_000, 0).single().unwrap();
    recent.upsert(updated);
    recent.prepare_view();
    assert_eq!(recent.visible_id(0), Some(target_id));
    assert_eq!(recent.order_rebuild_count, 2);
    assert_eq!(recent.filter_rebuild_count, 3);
  }

  #[test]
  fn local_upsert_is_not_overwritten_by_late_scan_or_metadata() {
    let original = summary("磁盘旧标题", 1, 1);
    let document_id = original.document_id;
    let mut recent = RecentDocuments::default();
    recent.begin_scan(7, &[skeleton(document_id)], Vec::new());

    let mut renamed = original.clone();
    renamed.title = "刚重命名".into();
    recent.upsert(renamed);

    recent.begin_scan(6, &[skeleton(document_id)], Vec::new());
    assert!(!recent.apply_cached_summary(6, original.clone()));
    assert!(!recent.apply_hydrated_summary(7, original));
    assert_eq!(recent.summary(document_id).unwrap().title, "刚重命名");
  }

  #[test]
  fn local_delete_is_not_resurrected_by_late_scan_or_metadata() {
    let document = summary("待删除", 1, 1);
    let document_id = document.document_id;
    let mut recent = RecentDocuments::default();
    recent.begin_scan(4, &[skeleton(document_id)], Vec::new());
    recent.remove(document_id);

    recent.begin_scan(3, &[skeleton(document_id)], Vec::new());
    assert!(!recent.apply_cached_summary(3, document.clone()));
    assert!(!recent.apply_hydrated_summary(4, document));
    assert!(recent.entry(document_id).is_none());
  }

  #[test]
  fn clear_rejects_a_late_bootstrap_and_its_metadata() {
    let document = summary("清理前讲义", 1, 1);
    let document_id = document.document_id;
    let mut recent = RecentDocuments::default();

    recent.clear();
    recent.begin_scan(1, &[skeleton(document_id)], Vec::new());

    assert!(recent.is_empty());
    assert!(!recent.apply_cached_summary(1, document.clone()));
    assert!(!recent.apply_hydrated_summary(1, document));
    assert!(!recent.fail_hydration(1, document_id, "late failure".into()));
  }

  #[test]
  fn metadata_must_match_the_entry_scan_generation() {
    let document = summary("当前扫描", 1, 1);
    let document_id = document.document_id;
    let mut recent = RecentDocuments::default();
    recent.begin_scan(9, &[skeleton(document_id)], Vec::new());

    assert!(!recent.apply_cached_summary(8, document.clone()));
    assert!(recent.summary(document_id).is_none());
    assert!(recent.apply_cached_summary(9, document));
    assert_eq!(recent.summary(document_id).unwrap().title, "当前扫描");
  }

  #[test]
  fn failed_hydration_discards_a_stale_cached_summary() {
    let document = summary("缓存旧标题", 1, 1);
    let document_id = document.document_id;
    let mut recent = RecentDocuments::default();
    recent.begin_scan(5, &[skeleton(document_id)], Vec::new());
    assert!(recent.apply_cached_summary(5, document));

    assert!(recent.fail_hydration(5, document_id, "manifest 已损坏".into()));

    let entry = recent.entry(document_id).unwrap();
    assert!(entry.summary.is_none());
    assert_eq!(entry.metadata_error.as_deref(), Some("manifest 已损坏"));
  }

  #[test]
  fn stale_preview_does_not_replace_current_revision() {
    let document = summary("讲义", 1, 3);
    let document_id = document.document_id;
    let mut recent = RecentDocuments::default();
    recent.upsert(document);

    assert!(!recent.mark_preview_ready(document_id, 2, PathBuf::from("old.png")));
    assert!(recent.mark_preview_ready(document_id, 3, PathBuf::from("current.png")));
    assert_eq!(recent.summary(document_id).unwrap().preview_revision, Some(3));
  }

  #[test]
  fn skeletons_remain_visible_without_matching_a_search_query() {
    let document_id = DocumentId::new();
    let mut recent = RecentDocuments::default();
    recent.begin_scan(1, &[skeleton(document_id)], Vec::new());
    recent.prepare_view();
    assert_eq!(recent.visible_count(), 1);

    recent.query = "unknown".into();
    recent.prepare_view();
    assert_eq!(recent.visible_count(), 0);
  }
}
