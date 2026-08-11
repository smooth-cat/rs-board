use std::collections::VecDeque;

use thiserror::Error;

use crate::{
  command::{CommandBatch, CommandError, DocumentCommand},
  document::{BoardDocument, Revision},
};

pub const MAX_HISTORY_ENTRIES: usize = 500;
pub const MAX_HISTORY_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryLimits {
  pub max_entries: usize,
  pub max_bytes: usize,
}

impl Default for HistoryLimits {
  fn default() -> Self {
    Self { max_entries: MAX_HISTORY_ENTRIES, max_bytes: MAX_HISTORY_BYTES }
  }
}

#[derive(Debug, Clone)]
struct HistoryEntry {
  sequence: u64,
  estimated_bytes: usize,
  applied: crate::command::AppliedCommand,
}

#[derive(Debug, Clone)]
pub struct CommandHistory {
  undo: VecDeque<HistoryEntry>,
  redo: VecDeque<HistoryEntry>,
  estimated_bytes: usize,
  next_sequence: u64,
  limits: HistoryLimits,
}

impl Default for CommandHistory {
  fn default() -> Self {
    Self::new()
  }
}

impl CommandHistory {
  pub fn new() -> Self {
    Self::with_limits(HistoryLimits::default())
  }

  pub fn with_limits(limits: HistoryLimits) -> Self {
    Self {
      undo: VecDeque::new(),
      redo: VecDeque::new(),
      estimated_bytes: 0,
      next_sequence: 0,
      limits,
    }
  }

  pub fn execute(
    &mut self,
    document: &mut BoardDocument,
    command: DocumentCommand,
  ) -> Result<Revision, HistoryError> {
    self.execute_batch(document, CommandBatch::single(command))
  }

  pub fn execute_batch(
    &mut self,
    document: &mut BoardDocument,
    batch: CommandBatch,
  ) -> Result<Revision, HistoryError> {
    let next_sequence = self.next_sequence.checked_add(1).ok_or(HistoryError::SequenceOverflow)?;
    let applied = batch.apply(document)?;
    self.clear_redo();
    let estimated_bytes = applied.estimated_bytes();
    self.estimated_bytes = self.estimated_bytes.saturating_add(estimated_bytes);
    self.undo.push_back(HistoryEntry { sequence: self.next_sequence, estimated_bytes, applied });
    self.next_sequence = next_sequence;
    self.trim_to_limits();
    Ok(document.revision)
  }

  pub fn undo(&mut self, document: &mut BoardDocument) -> Result<bool, HistoryError> {
    let Some(entry) = self.undo.back() else {
      return Ok(false);
    };
    entry.applied.undo(document)?;
    let entry = self.undo.pop_back().expect("the history entry checked above is still present");
    self.redo.push_back(entry);
    Ok(true)
  }

  pub fn redo(&mut self, document: &mut BoardDocument) -> Result<bool, HistoryError> {
    let Some(entry) = self.redo.back() else {
      return Ok(false);
    };
    entry.applied.redo(document)?;
    let entry = self.redo.pop_back().expect("the history entry checked above is still present");
    self.undo.push_back(entry);
    Ok(true)
  }

  pub fn can_undo(&self) -> bool {
    !self.undo.is_empty()
  }

  pub fn can_redo(&self) -> bool {
    !self.redo.is_empty()
  }

  pub fn undo_len(&self) -> usize {
    self.undo.len()
  }

  pub fn redo_len(&self) -> usize {
    self.redo.len()
  }

  pub fn len(&self) -> usize {
    self.undo.len() + self.redo.len()
  }

  pub fn is_empty(&self) -> bool {
    self.undo.is_empty() && self.redo.is_empty()
  }

  pub fn estimated_bytes(&self) -> usize {
    self.estimated_bytes
  }

  pub fn limits(&self) -> HistoryLimits {
    self.limits
  }

  pub fn set_limits(&mut self, limits: HistoryLimits) {
    self.limits = limits;
    self.trim_to_limits();
  }

  pub fn clear(&mut self) {
    self.undo.clear();
    self.redo.clear();
    self.estimated_bytes = 0;
  }

  fn clear_redo(&mut self) {
    for entry in self.redo.drain(..) {
      self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.estimated_bytes);
    }
  }

  fn trim_to_limits(&mut self) {
    while self.len() > self.limits.max_entries || self.estimated_bytes > self.limits.max_bytes {
      let oldest_undo = self.undo.iter().map(|entry| entry.sequence).min();
      let oldest_redo = self.redo.iter().map(|entry| entry.sequence).min();
      let remove_from_undo = match (oldest_undo, oldest_redo) {
        (Some(undo), Some(redo)) => undo <= redo,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => break,
      };
      let removed = if remove_from_undo {
        remove_oldest(&mut self.undo)
      } else {
        remove_oldest(&mut self.redo)
      };
      if let Some(entry) = removed {
        self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.estimated_bytes);
      }
    }
  }
}

fn remove_oldest(entries: &mut VecDeque<HistoryEntry>) -> Option<HistoryEntry> {
  let position = entries
    .iter()
    .enumerate()
    .min_by_key(|(_, entry)| entry.sequence)
    .map(|(position, _)| position)?;
  entries.remove(position)
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum HistoryError {
  #[error(transparent)]
  Command(#[from] CommandError),
  #[error("history action sequence overflow")]
  SequenceOverflow,
}

#[cfg(test)]
mod tests {
  use chrono::{TimeZone, Utc};
  use uuid::Uuid;

  use super::*;
  use crate::{
    command::DocumentCommand,
    document::{CapturedDisplay, DocumentId, GlobalBoundsPx},
    element::{
      ArrowHead, ArrowPayload, Element, ElementId, ElementLabel, ElementPayload, StrokeStyle,
      TextStyle,
    },
    geometry::{PointPx, SizePx},
  };

  fn document() -> BoardDocument {
    BoardDocument::new_capture(
      DocumentId::from_uuid(Uuid::nil()),
      SizePx::new(500, 300),
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx { x_px: 0, y_px: 0, width_px: 500, height_px: 300 },
        scale_factor: 1.0,
      },
      Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).unwrap(),
    )
    .unwrap()
  }

  fn arrow(id: ElementId, offset: f32) -> Element {
    let style = StrokeStyle::default();
    Element::new(
      id,
      0,
      ElementPayload::Arrow(ArrowPayload {
        start_px: PointPx::new(50.0 + offset, 50.0),
        end_px: PointPx::new(150.0 + offset, 100.0),
        head: ArrowHead::for_stroke_width(style.width_px).unwrap(),
        label: ElementLabel {
          text: None,
          max_width_px: 420.0,
          padding_px: 8.0,
          anchor_offset_px: 8.0,
          text_style: TextStyle::mvp(style.color_rgba.contrasting_text(), 24.0).unwrap(),
        },
        stroke_style: style,
      }),
      SizePx::new(500, 300),
    )
    .unwrap()
  }

  #[test]
  fn new_successful_command_clears_redo() {
    let mut document = document();
    let mut history = CommandHistory::new();
    let first = ElementId::new();
    history
      .execute(&mut document, DocumentCommand::AddElement { element: arrow(first, 0.0) })
      .unwrap();
    history.undo(&mut document).unwrap();
    assert!(history.can_redo());
    history
      .execute(
        &mut document,
        DocumentCommand::AddElement { element: arrow(ElementId::new(), 10.0) },
      )
      .unwrap();
    assert!(!history.can_redo());
  }

  #[test]
  fn failed_command_preserves_redo_and_document() {
    let mut document = document();
    let mut history = CommandHistory::new();
    let id = ElementId::new();
    history
      .execute(&mut document, DocumentCommand::AddElement { element: arrow(id, 0.0) })
      .unwrap();
    history.undo(&mut document).unwrap();
    let before = document.clone();
    let result = history.execute(
      &mut document,
      DocumentCommand::MoveElement {
        element_id: ElementId::new(),
        delta_px: PointPx::new(1.0, 1.0),
      },
    );
    assert!(result.is_err());
    assert_eq!(document, before);
    assert!(history.can_redo());
  }

  #[test]
  fn entry_limit_evicts_oldest_record() {
    let mut document = document();
    let mut history =
      CommandHistory::with_limits(HistoryLimits { max_entries: 2, max_bytes: usize::MAX });
    for offset in [0.0, 10.0, 20.0] {
      history
        .execute(
          &mut document,
          DocumentCommand::AddElement { element: arrow(ElementId::new(), offset) },
        )
        .unwrap();
    }
    assert_eq!(history.undo_len(), 2);
    history.undo(&mut document).unwrap();
    history.undo(&mut document).unwrap();
    assert!(!history.undo(&mut document).unwrap());
    assert_eq!(document.elements.len(), 1);
  }

  #[test]
  fn oversized_single_entry_is_not_retained() {
    let mut document = document();
    let mut history = CommandHistory::with_limits(HistoryLimits { max_entries: 500, max_bytes: 1 });
    history
      .execute(&mut document, DocumentCommand::AddElement { element: arrow(ElementId::new(), 0.0) })
      .unwrap();
    assert_eq!(document.elements.len(), 1);
    assert!(history.is_empty());
    assert_eq!(history.estimated_bytes(), 0);
  }

  #[test]
  fn reducing_limits_evicts_globally_oldest_across_stacks() {
    let mut document = document();
    let mut history = CommandHistory::new();
    for offset in [0.0, 10.0, 20.0] {
      history
        .execute(
          &mut document,
          DocumentCommand::AddElement { element: arrow(ElementId::new(), offset) },
        )
        .unwrap();
    }
    history.undo(&mut document).unwrap();
    history.set_limits(HistoryLimits { max_entries: 2, max_bytes: usize::MAX });
    assert_eq!(history.len(), 2);
    assert_eq!(history.undo_len(), 1);
    assert_eq!(history.redo_len(), 1);
  }

  #[test]
  fn default_history_keeps_exactly_five_hundred_records() {
    let mut document = document();
    let mut history = CommandHistory::new();
    for next_sequence_number in 2..=502 {
      history
        .execute(&mut document, DocumentCommand::SetNextSequenceNumber { next_sequence_number })
        .unwrap();
    }
    assert_eq!(history.undo_len(), MAX_HISTORY_ENTRIES);
    for _ in 0..MAX_HISTORY_ENTRIES {
      assert!(history.undo(&mut document).unwrap());
    }
    assert!(!history.undo(&mut document).unwrap());
    // The oldest action was evicted, so undo stops at the value written by that action.
    assert_eq!(document.next_sequence_number, 2);
  }

  #[test]
  fn undoing_to_opened_content_clears_dirty_without_rewinding_revision() {
    let mut document = document();
    let baseline = document.dirty_baseline();
    let mut history = CommandHistory::new();
    history
      .execute(&mut document, DocumentCommand::AddElement { element: arrow(ElementId::new(), 0.0) })
      .unwrap();
    assert!(document.is_dirty_against(baseline));
    history.undo(&mut document).unwrap();
    assert!(!document.is_dirty_against(baseline));
    assert_eq!(document.revision, 2);
  }

  #[test]
  fn byte_limit_is_inclusive_and_bytes_do_not_double_count_redo() {
    let mut document = document();
    let mut history = CommandHistory::new();
    history
      .execute(&mut document, DocumentCommand::AddElement { element: arrow(ElementId::new(), 0.0) })
      .unwrap();
    let one_entry_bytes = history.estimated_bytes();
    assert!(one_entry_bytes > 1);
    history.set_limits(HistoryLimits { max_entries: 500, max_bytes: one_entry_bytes });
    assert_eq!(history.len(), 1);
    history.undo(&mut document).unwrap();
    assert_eq!(history.estimated_bytes(), one_entry_bytes);
    history.redo(&mut document).unwrap();
    assert_eq!(history.estimated_bytes(), one_entry_bytes);
    history.set_limits(HistoryLimits { max_entries: 500, max_bytes: one_entry_bytes - 1 });
    assert!(history.is_empty());
    assert_eq!(history.estimated_bytes(), 0);
  }
}
