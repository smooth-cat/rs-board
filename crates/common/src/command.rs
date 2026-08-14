use chrono::Utc;
use thiserror::Error;

use crate::{
  document::{BoardDocument, DocumentError, MAX_ELEMENTS},
  element::{
    Element, ElementError, ElementId, ElementKind, ElementPayload, RectangleLabelAnchor,
    StyleChange,
  },
  geometry::PointPx,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowEndpoint {
  Start,
  End,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DocumentCommand {
  AddElement {
    element: Element,
  },
  UpdateElement {
    element_id: ElementId,
    payload: ElementPayload,
  },
  MoveElement {
    element_id: ElementId,
    delta_px: PointPx,
  },
  DeleteElement {
    element_id: ElementId,
  },
  ChangeElementStyle {
    element_id: ElementId,
    change: StyleChange,
  },
  ResizeRectangle {
    element_id: ElementId,
    start_px: PointPx,
    end_px: PointPx,
  },
  UpdateArrowEndpoint {
    element_id: ElementId,
    endpoint: ArrowEndpoint,
    position_px: PointPx,
  },
  UpdateElementLabel {
    element_id: ElementId,
    text: Option<String>,
  },
  SetRectangleLabelPlacement {
    element_id: ElementId,
    preferred_anchor: RectangleLabelAnchor,
    actual_anchor: RectangleLabelAnchor,
  },
  SetNextSequenceNumber {
    next_sequence_number: u64,
  },
  BringForward {
    element_id: ElementId,
  },
  SendBackward {
    element_id: ElementId,
  },
  BringToFront {
    element_id: ElementId,
  },
  SendToBack {
    element_id: ElementId,
  },
}

impl DocumentCommand {
  pub fn apply(self, document: &mut BoardDocument) -> Result<AppliedCommand, CommandError> {
    CommandBatch::single(self).apply(document)
  }

  pub fn paste_copy(
    source: &Element,
    new_element_id: ElementId,
    mouse_position_px: PointPx,
    document: &BoardDocument,
  ) -> Result<Self, CommandError> {
    let copy = source.placed_copy(new_element_id, mouse_position_px, document.canvas_size_px)?;
    Ok(Self::AddElement { element: copy })
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandBatch {
  commands: Vec<DocumentCommand>,
}

impl CommandBatch {
  pub fn new(commands: Vec<DocumentCommand>) -> Result<Self, CommandError> {
    if commands.is_empty() {
      return Err(CommandError::EmptyBatch);
    }
    Ok(Self { commands })
  }

  pub fn single(command: DocumentCommand) -> Self {
    Self { commands: vec![command] }
  }

  pub fn sequence_marker(document: &BoardDocument, element: Element) -> Result<Self, CommandError> {
    let ElementPayload::SequenceMarker(marker) = &element.payload else {
      return Err(CommandError::WrongElementKind {
        expected: ElementKind::SequenceMarker,
        actual: element.kind(),
      });
    };
    if marker.number != document.next_sequence_number {
      return Err(CommandError::SequenceNumberMismatch {
        expected: document.next_sequence_number,
        actual: marker.number,
      });
    }
    let next_sequence_number =
      document.next_sequence_number.checked_add(1).ok_or(CommandError::SequenceNumberOverflow)?;
    Self::new(vec![
      DocumentCommand::AddElement { element },
      DocumentCommand::SetNextSequenceNumber { next_sequence_number },
    ])
  }

  pub fn commands(&self) -> &[DocumentCommand] {
    &self.commands
  }

  pub fn apply(self, document: &mut BoardDocument) -> Result<AppliedCommand, CommandError> {
    document.validate()?;
    let mut staged = document.clone();
    let mut redo_mutations = Vec::with_capacity(self.commands.len());
    let mut undo_mutations = Vec::with_capacity(self.commands.len());

    for command in self.commands {
      let (redo, undo) = prepare_command(&mut staged, command)?;
      redo_mutations.push(redo);
      undo_mutations.push(undo);
    }
    undo_mutations.reverse();
    staged.validate()?;
    if staged.content_fingerprint() == document.content_fingerprint() {
      return Err(CommandError::NoChange);
    }
    staged.commit_content_change(Utc::now())?;
    let applied = AppliedCommand {
      redo: ReplayMutation::Batch(redo_mutations),
      undo: ReplayMutation::Batch(undo_mutations),
    };
    *document = staged;
    Ok(applied)
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppliedCommand {
  redo: ReplayMutation,
  undo: ReplayMutation,
}

impl AppliedCommand {
  pub fn undo(&self, document: &mut BoardDocument) -> Result<(), CommandError> {
    self.replay(document, &self.undo)
  }

  pub fn redo(&self, document: &mut BoardDocument) -> Result<(), CommandError> {
    self.replay(document, &self.redo)
  }

  pub fn estimated_bytes(&self) -> usize {
    std::mem::size_of::<Self>()
      .saturating_add(self.redo.estimated_bytes())
      .saturating_add(self.undo.estimated_bytes())
  }

  fn replay(
    &self,
    document: &mut BoardDocument,
    mutation: &ReplayMutation,
  ) -> Result<(), CommandError> {
    document.validate()?;
    let mut staged = document.clone();
    mutation.apply(&mut staged)?;
    staged.validate()?;
    if staged.content_fingerprint() == document.content_fingerprint() {
      return Err(CommandError::HistoryInvariant("history replay did not change document content"));
    }
    staged.commit_content_change(Utc::now())?;
    *document = staged;
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq)]
enum ReplayMutation {
  Insert { index: usize, element: Element },
  Remove { index: usize, element_id: ElementId },
  Replace { index: usize, element: Element },
  MoveLayer { from: usize, to: usize, element_id: ElementId },
  SetNextSequenceNumber(u64),
  Batch(Vec<ReplayMutation>),
}

impl ReplayMutation {
  fn apply(&self, document: &mut BoardDocument) -> Result<(), CommandError> {
    match self {
      Self::Insert { index, element } => {
        if *index > document.elements.len() {
          return Err(CommandError::HistoryInvariant("insert index is outside the element list"));
        }
        if document.element(element.element_id).is_some() {
          return Err(CommandError::DuplicateElementId(element.element_id));
        }
        document.elements.insert(*index, element.clone());
        document.normalize_z_order()?;
      }
      Self::Remove { index, element_id } => {
        if document.elements.get(*index).map(|element| element.element_id) != Some(*element_id) {
          return Err(CommandError::HistoryInvariant(
            "element at removal index does not match history",
          ));
        }
        document.elements.remove(*index);
        document.normalize_z_order()?;
      }
      Self::Replace { index, element } => {
        let Some(current) = document.elements.get_mut(*index) else {
          return Err(CommandError::HistoryInvariant(
            "replacement index is outside the element list",
          ));
        };
        if current.element_id != element.element_id {
          return Err(CommandError::HistoryInvariant(
            "replacement element id does not match history",
          ));
        }
        *current = element.clone();
      }
      Self::MoveLayer { from, to, element_id } => {
        if document.elements.get(*from).map(|element| element.element_id) != Some(*element_id)
          || *to >= document.elements.len()
        {
          return Err(CommandError::HistoryInvariant("layer order does not match history"));
        }
        let element = document.elements.remove(*from);
        document.elements.insert(*to, element);
        document.normalize_z_order()?;
      }
      Self::SetNextSequenceNumber(number) => {
        if *number == 0 {
          return Err(CommandError::InvalidNextSequenceNumber);
        }
        document.next_sequence_number = *number;
      }
      Self::Batch(mutations) => {
        for mutation in mutations {
          mutation.apply(document)?;
        }
      }
    }
    Ok(())
  }

  fn estimated_bytes(&self) -> usize {
    let own = std::mem::size_of::<Self>();
    match self {
      Self::Insert { element, .. } | Self::Replace { element, .. } => {
        own.saturating_add(element.estimated_bytes())
      }
      Self::Batch(mutations) => mutations
        .iter()
        .fold(own, |total, mutation| total.saturating_add(mutation.estimated_bytes())),
      Self::Remove { .. } | Self::MoveLayer { .. } | Self::SetNextSequenceNumber(_) => own,
    }
  }
}

fn prepare_command(
  document: &mut BoardDocument,
  command: DocumentCommand,
) -> Result<(ReplayMutation, ReplayMutation), CommandError> {
  match command {
    DocumentCommand::AddElement { element } => prepare_add(document, element),
    DocumentCommand::UpdateElement { element_id, payload } => {
      prepare_replace(document, element_id, |element, canvas| {
        if payload.kind() != element.kind() {
          return Err(CommandError::WrongElementKind {
            expected: element.kind(),
            actual: payload.kind(),
          });
        }
        Element::new(element.element_id, element.z_index, payload, canvas)
          .map_err(CommandError::from)
      })
    }
    DocumentCommand::MoveElement { element_id, delta_px } => {
      prepare_replace(document, element_id, |mut element, canvas| {
        element.move_by(delta_px, canvas)?;
        Ok(element)
      })
    }
    DocumentCommand::DeleteElement { element_id } => prepare_delete(document, element_id),
    DocumentCommand::ChangeElementStyle { element_id, change } => {
      prepare_replace(document, element_id, |mut element, canvas| {
        element.set_style(&change, canvas)?;
        Ok(element)
      })
    }
    DocumentCommand::ResizeRectangle { element_id, start_px, end_px } => {
      prepare_replace(document, element_id, |mut element, canvas| {
        let actual = element.kind();
        let ElementPayload::Rectangle(payload) = &mut element.payload else {
          return Err(CommandError::WrongElementKind { expected: ElementKind::Rectangle, actual });
        };
        payload.start_px = start_px;
        payload.end_px = end_px;
        element.refresh_bounds(canvas)?;
        element.constrain_to_canvas(canvas, true)?;
        element.validate(canvas)?;
        Ok(element)
      })
    }
    DocumentCommand::UpdateArrowEndpoint { element_id, endpoint, position_px } => {
      prepare_replace(document, element_id, |mut element, canvas| {
        let actual = element.kind();
        let ElementPayload::Arrow(payload) = &mut element.payload else {
          return Err(CommandError::WrongElementKind { expected: ElementKind::Arrow, actual });
        };
        match endpoint {
          ArrowEndpoint::Start => payload.start_px = position_px,
          ArrowEndpoint::End => payload.end_px = position_px,
        }
        element.refresh_bounds(canvas)?;
        element.constrain_to_canvas(canvas, true)?;
        element.validate(canvas)?;
        Ok(element)
      })
    }
    DocumentCommand::UpdateElementLabel { element_id, text } => {
      prepare_replace(document, element_id, |mut element, canvas| {
        let actual = element.kind();
        let label = match &mut element.payload {
          ElementPayload::Arrow(payload) => &mut payload.label,
          ElementPayload::Rectangle(payload) => &mut payload.label,
          _ => return Err(CommandError::LabelNotSupported(actual)),
        };
        label.text = text.filter(|text| !text.trim().is_empty());
        element.refresh_bounds(canvas)?;
        element.constrain_to_canvas(canvas, true)?;
        element.validate(canvas)?;
        Ok(element)
      })
    }
    DocumentCommand::SetRectangleLabelPlacement { element_id, preferred_anchor, actual_anchor } => {
      prepare_replace(document, element_id, |mut element, canvas| {
        let actual = element.kind();
        let ElementPayload::Rectangle(payload) = &mut element.payload else {
          return Err(CommandError::WrongElementKind { expected: ElementKind::Rectangle, actual });
        };
        payload.preferred_label_anchor = preferred_anchor;
        payload.label_anchor = actual_anchor;
        element.refresh_bounds(canvas)?;
        element.validate(canvas)?;
        Ok(element)
      })
    }
    DocumentCommand::SetNextSequenceNumber { next_sequence_number } => {
      if next_sequence_number == 0 {
        return Err(CommandError::InvalidNextSequenceNumber);
      }
      if next_sequence_number == document.next_sequence_number {
        return Err(CommandError::NoChange);
      }
      let before = document.next_sequence_number;
      document.next_sequence_number = next_sequence_number;
      Ok((
        ReplayMutation::SetNextSequenceNumber(next_sequence_number),
        ReplayMutation::SetNextSequenceNumber(before),
      ))
    }
    DocumentCommand::BringForward { element_id } => {
      prepare_layer_move(document, element_id, LayerMove::Forward)
    }
    DocumentCommand::SendBackward { element_id } => {
      prepare_layer_move(document, element_id, LayerMove::Backward)
    }
    DocumentCommand::BringToFront { element_id } => {
      prepare_layer_move(document, element_id, LayerMove::Front)
    }
    DocumentCommand::SendToBack { element_id } => {
      prepare_layer_move(document, element_id, LayerMove::Back)
    }
  }
}

fn prepare_add(
  document: &mut BoardDocument,
  element: Element,
) -> Result<(ReplayMutation, ReplayMutation), CommandError> {
  if document.elements.len() >= MAX_ELEMENTS {
    return Err(
      DocumentError::ElementLimitExceeded {
        count: document.elements.len().saturating_add(1),
        limit: MAX_ELEMENTS,
      }
      .into(),
    );
  }
  if document.element(element.element_id).is_some() {
    return Err(CommandError::DuplicateElementId(element.element_id));
  }
  let index = document.elements.len();
  let element = Element::new(
    element.element_id,
    i64::try_from(index).map_err(|_| DocumentError::ZIndexOverflow)?,
    element.payload,
    document.canvas_size_px,
  )?;
  let element_id = element.element_id;
  document.elements.push(element.clone());
  document.validate()?;
  Ok((ReplayMutation::Insert { index, element }, ReplayMutation::Remove { index, element_id }))
}

fn prepare_delete(
  document: &mut BoardDocument,
  element_id: ElementId,
) -> Result<(ReplayMutation, ReplayMutation), CommandError> {
  let index = element_index(document, element_id)?;
  let element = document.elements.remove(index);
  document.normalize_z_order()?;
  Ok((ReplayMutation::Remove { index, element_id }, ReplayMutation::Insert { index, element }))
}

fn prepare_replace(
  document: &mut BoardDocument,
  element_id: ElementId,
  replacement: impl FnOnce(Element, crate::geometry::SizePx) -> Result<Element, CommandError>,
) -> Result<(ReplayMutation, ReplayMutation), CommandError> {
  let index = element_index(document, element_id)?;
  let before = document.elements[index].clone();
  let after = replacement(before.clone(), document.canvas_size_px)?;
  if before == after {
    return Err(CommandError::NoChange);
  }
  if after.element_id != before.element_id || after.z_index != before.z_index {
    return Err(CommandError::HistoryInvariant("element replacement changed identity or layer"));
  }
  document.elements[index] = after.clone();
  document.validate()?;
  Ok((
    ReplayMutation::Replace { index, element: after },
    ReplayMutation::Replace { index, element: before },
  ))
}

#[derive(Debug, Clone, Copy)]
enum LayerMove {
  Forward,
  Backward,
  Front,
  Back,
}

fn prepare_layer_move(
  document: &mut BoardDocument,
  element_id: ElementId,
  movement: LayerMove,
) -> Result<(ReplayMutation, ReplayMutation), CommandError> {
  let from = element_index(document, element_id)?;
  let last = document.elements.len().saturating_sub(1);
  let to = match movement {
    LayerMove::Forward => from.saturating_add(1).min(last),
    LayerMove::Backward => from.saturating_sub(1),
    LayerMove::Front => last,
    LayerMove::Back => 0,
  };
  if from == to {
    return Err(CommandError::NoChange);
  }
  let element = document.elements.remove(from);
  document.elements.insert(to, element);
  document.normalize_z_order()?;
  Ok((
    ReplayMutation::MoveLayer { from, to, element_id },
    ReplayMutation::MoveLayer { from: to, to: from, element_id },
  ))
}

fn element_index(document: &BoardDocument, element_id: ElementId) -> Result<usize, CommandError> {
  document
    .elements
    .iter()
    .position(|element| element.element_id == element_id)
    .ok_or(CommandError::ElementNotFound(element_id))
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum CommandError {
  #[error(transparent)]
  Document(#[from] DocumentError),
  #[error(transparent)]
  Element(#[from] ElementError),
  #[error("element {0} was not found")]
  ElementNotFound(ElementId),
  #[error("element id {0} already exists")]
  DuplicateElementId(ElementId),
  #[error("expected {expected:?}, found {actual:?}")]
  WrongElementKind { expected: ElementKind, actual: ElementKind },
  #[error("{0:?} elements do not support labels")]
  LabelNotSupported(ElementKind),
  #[error("command does not change document content")]
  NoChange,
  #[error("command batch must not be empty")]
  EmptyBatch,
  #[error("next sequence number must be at least one")]
  InvalidNextSequenceNumber,
  #[error("sequence marker number {actual} does not match next number {expected}")]
  SequenceNumberMismatch { expected: u64, actual: u64 },
  #[error("sequence number overflow")]
  SequenceNumberOverflow,
  #[error("command history invariant failed: {0}")]
  HistoryInvariant(&'static str),
}

#[cfg(test)]
mod tests {
  use chrono::{TimeZone, Utc};
  use uuid::Uuid;

  use super::*;
  use crate::{
    document::{CapturedDisplay, DocumentId, GlobalBoundsPx},
    element::{
      ArrowHead, ArrowPayload, ColorRgba, ElementLabel, ElementPayload, RectangleLabelAnchor,
      RectangleLabelEdge, RectangleLabelSide, RectanglePayload, SequenceMarkerPayload, StrokeStyle,
      TextPayload, TextStyle,
    },
    geometry::SizePx,
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

  fn arrow(id: ElementId) -> Element {
    let style = StrokeStyle::default();
    Element::new(
      id,
      0,
      ElementPayload::Arrow(ArrowPayload {
        start_px: PointPx::new(50.0, 50.0),
        end_px: PointPx::new(180.0, 100.0),
        head: ArrowHead::for_stroke_width(style.width_px).unwrap(),
        label: ElementLabel {
          text: None,
          max_width_px: 180.0,
          padding_px: 4.0,
          anchor_offset_px: 4.0,
          text_style: TextStyle::mvp(style.color_rgba.contrasting_text(), 24.0).unwrap(),
        },
        stroke_style: style,
      }),
      SizePx::new(500, 300),
    )
    .unwrap()
  }

  fn marker(id: ElementId, number: u64) -> Element {
    let fill = ColorRgba::RED;
    Element::new(
      id,
      0,
      ElementPayload::SequenceMarker(SequenceMarkerPayload {
        center_px: PointPx::new(100.0, 100.0),
        number,
        radius_px: 18.0,
        pill_width_px: 36.0,
        fill_rgba: fill,
        stroke_style: StrokeStyle::mvp(fill, 4.0).unwrap(),
        text_style: TextStyle::mvp(fill.contrasting_text(), 16.0).unwrap(),
      }),
      SizePx::new(500, 300),
    )
    .unwrap()
  }

  fn rectangle(id: ElementId) -> Element {
    let color = ColorRgba::RED;
    Element::new(
      id,
      0,
      ElementPayload::Rectangle(RectanglePayload {
        start_px: PointPx::new(180.0, 100.0),
        end_px: PointPx::new(320.0, 220.0),
        stroke_style: StrokeStyle::default(),
        fill_rgba: None,
        label: ElementLabel {
          text: Some("标题".to_owned()),
          max_width_px: 180.0,
          padding_px: 4.0,
          anchor_offset_px: 4.0,
          text_style: TextStyle::mvp(color.contrasting_text(), 24.0).unwrap(),
        },
        preferred_label_anchor: RectangleLabelAnchor::new(
          RectangleLabelEdge::Top,
          RectangleLabelSide::Outside,
          0.0,
        ),
        label_anchor: RectangleLabelAnchor::new(
          RectangleLabelEdge::Top,
          RectangleLabelSide::Outside,
          0.0,
        ),
      }),
      SizePx::new(500, 300),
    )
    .unwrap()
  }

  fn text(id: ElementId, value: &str) -> Element {
    Element::new(
      id,
      0,
      ElementPayload::Text(TextPayload {
        anchor_px: PointPx::new(40.0, 180.0),
        text: value.to_owned(),
        box_width_px: 180.0,
        text_style: TextStyle::default(),
      }),
      SizePx::new(500, 300),
    )
    .unwrap()
  }

  fn assert_content_round_trip(mut document: BoardDocument, command: DocumentCommand) {
    let before = document.content_fingerprint();
    let starting_revision = document.revision;
    let applied = command.apply(&mut document).unwrap();
    let after = document.content_fingerprint();
    assert_ne!(after, before);
    assert_eq!(document.revision, starting_revision + 1);
    applied.undo(&mut document).unwrap();
    assert_eq!(document.content_fingerprint(), before);
    assert_eq!(document.revision, starting_revision + 2);
    applied.redo(&mut document).unwrap();
    assert_eq!(document.content_fingerprint(), after);
    assert_eq!(document.revision, starting_revision + 3);
  }

  #[test]
  fn apply_undo_redo_keeps_revision_monotonic() {
    let mut document = document();
    document.preview_revision = Some(0);
    let id = ElementId::new();
    let applied = DocumentCommand::AddElement { element: arrow(id) }.apply(&mut document).unwrap();
    assert_eq!(document.revision, 1);
    assert_eq!(document.preview_revision, None);
    applied.undo(&mut document).unwrap();
    assert_eq!(document.revision, 2);
    assert_eq!(document.preview_revision, None);
    assert!(document.elements.is_empty());
    applied.redo(&mut document).unwrap();
    assert_eq!(document.revision, 3);
    assert_eq!(document.elements[0].element_id, id);
  }

  #[test]
  fn failed_command_is_fully_atomic() {
    let mut document = document();
    let before = document.clone();
    let result = DocumentCommand::MoveElement {
      element_id: ElementId::new(),
      delta_px: PointPx::new(5.0, 5.0),
    }
    .apply(&mut document);
    assert!(matches!(result, Err(CommandError::ElementNotFound(_))));
    assert_eq!(document, before);
  }

  #[test]
  fn sequence_marker_and_counter_are_one_atomic_action() {
    let mut document = document();
    let marker = marker(ElementId::new(), 1);
    let applied =
      CommandBatch::sequence_marker(&document, marker).unwrap().apply(&mut document).unwrap();
    assert_eq!(document.elements.len(), 1);
    assert_eq!(document.next_sequence_number, 2);
    assert_eq!(document.revision, 1);
    applied.undo(&mut document).unwrap();
    assert!(document.elements.is_empty());
    assert_eq!(document.next_sequence_number, 1);
    assert_eq!(document.revision, 2);
    applied.redo(&mut document).unwrap();
    assert_eq!(document.next_sequence_number, 2);
    assert_eq!(document.revision, 3);
  }

  #[test]
  fn deleting_sequence_marker_does_not_renumber_or_rewind_counter() {
    let mut document = document();
    let marker_id = ElementId::new();
    CommandBatch::sequence_marker(&document, marker(marker_id, 1))
      .unwrap()
      .apply(&mut document)
      .unwrap();
    let delete =
      DocumentCommand::DeleteElement { element_id: marker_id }.apply(&mut document).unwrap();
    assert!(document.elements.is_empty());
    assert_eq!(document.next_sequence_number, 2);
    delete.undo(&mut document).unwrap();
    let ElementPayload::SequenceMarker(restored) = &document.elements[0].payload else {
      unreachable!();
    };
    assert_eq!(restored.number, 1);
    assert_eq!(document.next_sequence_number, 2);
  }

  #[test]
  fn pasting_sequence_marker_copies_fixed_number_without_advancing_counter() {
    let mut document = document();
    let marker_id = ElementId::new();
    CommandBatch::sequence_marker(&document, marker(marker_id, 1))
      .unwrap()
      .apply(&mut document)
      .unwrap();
    let paste = DocumentCommand::paste_copy(
      document.element(marker_id).unwrap(),
      ElementId::new(),
      PointPx::new(180.0, 180.0),
      &document,
    )
    .unwrap();
    paste.apply(&mut document).unwrap();
    assert_eq!(document.next_sequence_number, 2);
    let numbers = document
      .elements
      .iter()
      .map(|element| match &element.payload {
        ElementPayload::SequenceMarker(marker) => marker.number,
        _ => unreachable!(),
      })
      .collect::<Vec<_>>();
    assert_eq!(numbers, vec![1, 1]);
  }

  #[test]
  fn layer_commands_reorder_and_restore_exactly() {
    let mut document = document();
    let ids = [ElementId::new(), ElementId::new(), ElementId::new()];
    for id in ids {
      DocumentCommand::AddElement { element: arrow(id) }.apply(&mut document).unwrap();
    }
    let applied =
      DocumentCommand::BringToFront { element_id: ids[0] }.apply(&mut document).unwrap();
    assert_eq!(document.elements[2].element_id, ids[0]);
    applied.undo(&mut document).unwrap();
    assert_eq!(document.elements.iter().map(|element| element.element_id).collect::<Vec<_>>(), ids);
  }

  #[test]
  fn boundary_layer_operation_is_no_change() {
    let mut document = document();
    let id = ElementId::new();
    DocumentCommand::AddElement { element: arrow(id) }.apply(&mut document).unwrap();
    let before = document.clone();
    let result = DocumentCommand::BringForward { element_id: id }.apply(&mut document);
    assert_eq!(result, Err(CommandError::NoChange));
    assert_eq!(document, before);
  }

  #[test]
  fn every_element_mutation_command_round_trips_content_exactly() {
    let arrow_id = ElementId::new();
    let rectangle_id = ElementId::new();
    let text_id = ElementId::new();
    let mut base = document();
    for element in [arrow(arrow_id), rectangle(rectangle_id), text(text_id, "原文")] {
      DocumentCommand::AddElement { element }.apply(&mut base).unwrap();
    }

    assert_content_round_trip(
      base.clone(),
      DocumentCommand::MoveElement { element_id: text_id, delta_px: PointPx::new(23.5, -7.25) },
    );
    assert_content_round_trip(base.clone(), DocumentCommand::DeleteElement { element_id: text_id });
    assert_content_round_trip(
      base.clone(),
      DocumentCommand::ChangeElementStyle {
        element_id: arrow_id,
        change: StyleChange {
          color_rgba: Some(ColorRgba::BLUE),
          width_px: Some(12.0),
          font_size_px: None,
          hardness: None,
        },
      },
    );
    assert_content_round_trip(
      base.clone(),
      DocumentCommand::ResizeRectangle {
        element_id: rectangle_id,
        start_px: PointPx::new(160.0, 90.0),
        end_px: PointPx::new(340.0, 230.0),
      },
    );
    assert_content_round_trip(
      base.clone(),
      DocumentCommand::UpdateArrowEndpoint {
        element_id: arrow_id,
        endpoint: ArrowEndpoint::End,
        position_px: PointPx::new(220.0, 140.0),
      },
    );
    assert_content_round_trip(
      base.clone(),
      DocumentCommand::UpdateElementLabel {
        element_id: rectangle_id,
        text: Some("更新后的标签".to_owned()),
      },
    );
    assert_content_round_trip(
      base.clone(),
      DocumentCommand::SetRectangleLabelPlacement {
        element_id: rectangle_id,
        preferred_anchor: RectangleLabelAnchor::new(
          RectangleLabelEdge::Top,
          RectangleLabelSide::Outside,
          0.0,
        ),
        actual_anchor: RectangleLabelAnchor::new(
          RectangleLabelEdge::Right,
          RectangleLabelSide::Inside,
          0.5,
        ),
      },
    );
    assert_content_round_trip(
      base.clone(),
      DocumentCommand::UpdateElement {
        element_id: text_id,
        payload: ElementPayload::Text(TextPayload {
          anchor_px: PointPx::new(40.0, 180.0),
          text: "新文字\n第二行".to_owned(),
          box_width_px: 180.0,
          text_style: TextStyle::default(),
        }),
      },
    );
    assert_content_round_trip(
      base,
      DocumentCommand::SetNextSequenceNumber { next_sequence_number: 8 },
    );
  }

  #[test]
  fn generic_label_command_updates_arrows_and_rectangles_and_normalizes_blank_text() {
    let arrow_id = ElementId::new();
    let rectangle_id = ElementId::new();
    let mut document = document();
    for element in [arrow(arrow_id), rectangle(rectangle_id)] {
      DocumentCommand::AddElement { element }.apply(&mut document).unwrap();
    }

    DocumentCommand::UpdateElementLabel {
      element_id: arrow_id,
      text: Some("  箭头\n标签  ".to_owned()),
    }
    .apply(&mut document)
    .unwrap();
    let ElementPayload::Arrow(arrow) = &document.element(arrow_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(arrow.label.text.as_deref(), Some("  箭头\n标签  "));

    let applied = DocumentCommand::UpdateElementLabel {
      element_id: rectangle_id,
      text: Some(" \n\t ".to_owned()),
    }
    .apply(&mut document)
    .unwrap();
    let ElementPayload::Rectangle(rectangle) = &document.element(rectangle_id).unwrap().payload
    else {
      unreachable!();
    };
    assert_eq!(rectangle.label.text, None);
    applied.undo(&mut document).unwrap();
    let ElementPayload::Rectangle(rectangle) = &document.element(rectangle_id).unwrap().payload
    else {
      unreachable!();
    };
    assert_eq!(rectangle.label.text.as_deref(), Some("标题"));
  }

  #[test]
  fn rectangle_label_text_and_anchor_can_be_recreated_in_one_revision() {
    let rectangle_id = ElementId::new();
    let mut document = document();
    DocumentCommand::AddElement { element: rectangle(rectangle_id) }.apply(&mut document).unwrap();
    DocumentCommand::UpdateElementLabel { element_id: rectangle_id, text: None }
      .apply(&mut document)
      .unwrap();
    let before_revision = document.revision;
    let anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Left, RectangleLabelSide::Inside, 0.75);
    let batch = CommandBatch::new(vec![
      DocumentCommand::UpdateElementLabel {
        element_id: rectangle_id,
        text: Some("重新展示".to_owned()),
      },
      DocumentCommand::SetRectangleLabelPlacement {
        element_id: rectangle_id,
        preferred_anchor: anchor,
        actual_anchor: anchor,
      },
    ])
    .unwrap();
    let applied = batch.apply(&mut document).unwrap();
    assert_eq!(document.revision, before_revision + 1);
    let ElementPayload::Rectangle(rectangle) = &document.element(rectangle_id).unwrap().payload
    else {
      unreachable!();
    };
    assert_eq!(rectangle.label.text.as_deref(), Some("重新展示"));
    assert_eq!(rectangle.label_anchor, anchor);

    applied.undo(&mut document).unwrap();
    let ElementPayload::Rectangle(rectangle) = &document.element(rectangle_id).unwrap().payload
    else {
      unreachable!();
    };
    assert_eq!(rectangle.label.text, None);
    assert_ne!(rectangle.label_anchor, anchor);
  }

  #[test]
  fn paste_copy_assigns_new_id_centers_clamps_and_places_on_top() {
    let source_id = ElementId::new();
    let mut document = document();
    DocumentCommand::AddElement { element: rectangle(source_id) }.apply(&mut document).unwrap();
    let new_id = ElementId::new();
    let paste = DocumentCommand::paste_copy(
      document.element(source_id).unwrap(),
      new_id,
      PointPx::new(499.0, 299.0),
      &document,
    )
    .unwrap();
    let applied = paste.apply(&mut document).unwrap();
    let pasted = document.highest_element().unwrap();
    assert_eq!(pasted.element_id, new_id);
    assert!(document.canvas_size_px.bounds().contains_rect(pasted.bounds_px));
    applied.undo(&mut document).unwrap();
    assert_eq!(document.elements.len(), 1);
    assert_eq!(document.elements[0].element_id, source_id);
  }

  #[test]
  fn paste_copy_preserves_shared_labels_and_rectangle_anchors() {
    let document = document();

    let mut source_arrow = arrow(ElementId::new());
    let ElementPayload::Arrow(source_payload) = &mut source_arrow.payload else {
      unreachable!();
    };
    source_payload.label.text = Some("箭头标签".to_owned());
    source_arrow.refresh_bounds(document.canvas_size_px).unwrap();
    let arrow_copy = DocumentCommand::paste_copy(
      &source_arrow,
      ElementId::new(),
      PointPx::new(250.0, 150.0),
      &document,
    )
    .unwrap();
    let DocumentCommand::AddElement { element: arrow_copy } = arrow_copy else {
      unreachable!();
    };
    let (ElementPayload::Arrow(source), ElementPayload::Arrow(copy)) =
      (&source_arrow.payload, &arrow_copy.payload)
    else {
      unreachable!();
    };
    assert_eq!(copy.label, source.label);

    let mut source_rectangle = rectangle(ElementId::new());
    let ElementPayload::Rectangle(source_payload) = &mut source_rectangle.payload else {
      unreachable!();
    };
    source_payload.label.text = None;
    source_payload.label_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Right, RectangleLabelSide::Inside, 0.75);
    source_rectangle.refresh_bounds(document.canvas_size_px).unwrap();
    let rectangle_copy = DocumentCommand::paste_copy(
      &source_rectangle,
      ElementId::new(),
      PointPx::new(250.0, 150.0),
      &document,
    )
    .unwrap();
    let DocumentCommand::AddElement { element: rectangle_copy } = rectangle_copy else {
      unreachable!();
    };
    let (ElementPayload::Rectangle(source), ElementPayload::Rectangle(copy)) =
      (&source_rectangle.payload, &rectangle_copy.payload)
    else {
      unreachable!();
    };
    assert_eq!(copy.label, source.label);
    assert_eq!(copy.label_anchor, source.label_anchor);
  }

  #[test]
  fn revision_overflow_rolls_back_the_whole_command() {
    let mut document = document();
    document.revision = u64::MAX;
    let before = document.clone();
    let result =
      DocumentCommand::AddElement { element: arrow(ElementId::new()) }.apply(&mut document);
    assert_eq!(result, Err(CommandError::Document(DocumentError::RevisionOverflow)));
    assert_eq!(document, before);
  }

  #[test]
  fn all_four_layer_commands_have_adjacent_or_absolute_semantics() {
    let ids = [ElementId::new(), ElementId::new(), ElementId::new()];
    let mut base = document();
    for id in ids {
      DocumentCommand::AddElement { element: arrow(id) }.apply(&mut base).unwrap();
    }

    let cases = [
      (DocumentCommand::BringForward { element_id: ids[1] }, vec![ids[0], ids[2], ids[1]]),
      (DocumentCommand::SendBackward { element_id: ids[1] }, vec![ids[1], ids[0], ids[2]]),
      (DocumentCommand::BringToFront { element_id: ids[0] }, vec![ids[1], ids[2], ids[0]]),
      (DocumentCommand::SendToBack { element_id: ids[2] }, vec![ids[2], ids[0], ids[1]]),
    ];
    for (command, expected) in cases {
      let mut document = base.clone();
      let before = document.content_fingerprint();
      let applied = command.apply(&mut document).unwrap();
      assert_eq!(
        document.elements.iter().map(|element| element.element_id).collect::<Vec<_>>(),
        expected
      );
      applied.undo(&mut document).unwrap();
      assert_eq!(document.content_fingerprint(), before);
    }
  }

  #[test]
  fn add_at_element_limit_is_rejected_without_revision_change() {
    let mut document = document();
    let template = arrow(ElementId::new());
    document.elements = (0..MAX_ELEMENTS)
      .map(|index| {
        let mut element = template.clone();
        element.element_id = ElementId::from_uuid(Uuid::from_u128(index as u128 + 1));
        element.z_index = index as i64;
        element
      })
      .collect();
    document.validate().unwrap();
    let before_fingerprint = document.content_fingerprint();
    let before_revision = document.revision;
    let result =
      DocumentCommand::AddElement { element: arrow(ElementId::new()) }.apply(&mut document);
    assert_eq!(
      result,
      Err(CommandError::Document(DocumentError::ElementLimitExceeded {
        count: MAX_ELEMENTS + 1,
        limit: MAX_ELEMENTS,
      }))
    );
    assert_eq!(document.content_fingerprint(), before_fingerprint);
    assert_eq!(document.revision, before_revision);
  }

  #[test]
  fn update_element_cannot_change_element_kind() {
    let id = ElementId::new();
    let mut document = document();
    DocumentCommand::AddElement { element: text(id, "文字") }.apply(&mut document).unwrap();
    let before = document.clone();
    let ElementPayload::Arrow(arrow_payload) = arrow(ElementId::new()).payload else {
      unreachable!();
    };
    let result = DocumentCommand::UpdateElement {
      element_id: id,
      payload: ElementPayload::Arrow(arrow_payload),
    }
    .apply(&mut document);
    assert_eq!(
      result,
      Err(CommandError::WrongElementKind {
        expected: ElementKind::Text,
        actual: ElementKind::Arrow,
      })
    );
    assert_eq!(document, before);
  }
}
