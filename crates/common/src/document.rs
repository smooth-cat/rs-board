use std::{collections::HashSet, fmt};

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{
  element::{Element, ElementError, ElementId},
  format::{ResourceNameError, validate_resource_name},
  geometry::{GeometryError, SizePx},
};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;
pub const MAX_ELEMENTS: usize = 10_000;
pub const MAX_STROKE_POINTS: usize = 1_000_000;

pub type Revision = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocumentId(pub Uuid);

impl DocumentId {
  pub fn new() -> Self {
    Self(Uuid::new_v4())
  }

  pub const fn from_uuid(uuid: Uuid) -> Self {
    Self(uuid)
  }

  pub const fn as_uuid(self) -> Uuid {
    self.0
  }
}

impl Default for DocumentId {
  fn default() -> Self {
    Self::new()
  }
}

impl fmt::Display for DocumentId {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    self.0.fmt(formatter)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalBoundsPx {
  pub x_px: i32,
  pub y_px: i32,
  pub width_px: u32,
  pub height_px: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDisplay {
  pub global_bounds_px: GlobalBoundsPx,
  pub scale_factor: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundKind {
  CapturedScreen,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundMetadata {
  pub kind: BackgroundKind,
  pub file: String,
  pub pixel_size: SizePx,
  pub captured_display: CapturedDisplay,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardDocument {
  pub schema_version: u32,
  pub document_id: DocumentId,
  pub title: String,
  pub canvas_size_px: SizePx,
  pub background: BackgroundMetadata,
  pub preview_file: String,
  pub preview_revision: Option<Revision>,
  pub elements: Vec<Element>,
  pub next_sequence_number: u64,
  pub revision: Revision,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
}

impl BoardDocument {
  pub fn new_capture<Tz: TimeZone>(
    document_id: DocumentId,
    canvas_size_px: SizePx,
    captured_display: CapturedDisplay,
    captured_at: DateTime<Tz>,
  ) -> Result<Self, DocumentError>
  where
    Tz::Offset: fmt::Display,
  {
    let timestamp = captured_at.with_timezone(&Utc);
    let title = format!("截图 {}", captured_at.format("%Y-%m-%d %H:%M:%S"));
    let document = Self {
      schema_version: CURRENT_SCHEMA_VERSION,
      document_id,
      title,
      canvas_size_px,
      background: BackgroundMetadata {
        kind: BackgroundKind::CapturedScreen,
        file: format!("{document_id}.png"),
        pixel_size: canvas_size_px,
        captured_display,
      },
      preview_file: format!("{document_id}.preview.png"),
      preview_revision: None,
      elements: Vec::new(),
      next_sequence_number: 1,
      revision: 0,
      created_at: timestamp,
      updated_at: timestamp,
    };
    document.validate()?;
    Ok(document)
  }

  pub fn validate(&self) -> Result<(), DocumentError> {
    if self.schema_version != CURRENT_SCHEMA_VERSION {
      return Err(DocumentError::UnsupportedSchema {
        found: self.schema_version,
        supported: CURRENT_SCHEMA_VERSION,
      });
    }
    if self.title.trim().is_empty() {
      return Err(DocumentError::EmptyTitle);
    }
    self.canvas_size_px.validate()?;
    if self.background.pixel_size != self.canvas_size_px {
      return Err(DocumentError::BackgroundSizeMismatch);
    }
    if !self.background.captured_display.scale_factor.is_finite()
      || self.background.captured_display.scale_factor <= 0.0
    {
      return Err(DocumentError::InvalidScaleFactor);
    }
    let display = &self.background.captured_display;
    let expected_width_px = display.global_bounds_px.width_px as f32 * display.scale_factor;
    let expected_height_px = display.global_bounds_px.height_px as f32 * display.scale_factor;
    // Platform display bounds are logical coordinates; capture pixels are physical coordinates.
    if (expected_width_px - self.canvas_size_px.width_px as f32).abs() > 1.0
      || (expected_height_px - self.canvas_size_px.height_px as f32).abs() > 1.0
    {
      return Err(DocumentError::CapturedDisplaySizeMismatch);
    }
    validate_resource_name(&self.background.file)
      .map_err(|source| DocumentError::InvalidResourceName { field: "background.file", source })?;
    validate_resource_name(&self.preview_file)
      .map_err(|source| DocumentError::InvalidResourceName { field: "preview_file", source })?;
    if self.next_sequence_number == 0 {
      return Err(DocumentError::InvalidNextSequenceNumber);
    }
    if self.preview_revision.is_some_and(|preview| preview > self.revision) {
      return Err(DocumentError::PreviewRevisionAhead);
    }
    if self.updated_at < self.created_at {
      return Err(DocumentError::UpdatedBeforeCreated);
    }
    if self.elements.len() > MAX_ELEMENTS {
      return Err(DocumentError::ElementLimitExceeded {
        count: self.elements.len(),
        limit: MAX_ELEMENTS,
      });
    }

    let mut ids = HashSet::with_capacity(self.elements.len());
    let mut point_count = 0usize;
    for (index, element) in self.elements.iter().enumerate() {
      if !ids.insert(element.element_id) {
        return Err(DocumentError::DuplicateElementId(element.element_id));
      }
      if element.z_index != index as i64 {
        return Err(DocumentError::InvalidZOrder { index, z_index: element.z_index });
      }
      element.validate(self.canvas_size_px)?;
      point_count = point_count
        .checked_add(element.persistent_point_count())
        .ok_or(DocumentError::StrokePointCountOverflow)?;
      if point_count > MAX_STROKE_POINTS {
        return Err(DocumentError::StrokePointLimitExceeded {
          count: point_count,
          limit: MAX_STROKE_POINTS,
        });
      }
    }
    Ok(())
  }

  pub fn snapshot(&self, expected_revision: Revision) -> Result<DocumentSnapshot, DocumentError> {
    if self.revision != expected_revision {
      return Err(DocumentError::RevisionMismatch {
        expected: expected_revision,
        actual: self.revision,
      });
    }
    self.validate()?;
    Ok(DocumentSnapshot::from(self))
  }

  pub fn content_fingerprint(&self) -> ContentFingerprint {
    #[derive(Serialize)]
    struct FingerprintContent<'a> {
      schema_version: u32,
      document_id: DocumentId,
      title: &'a str,
      canvas_size_px: SizePx,
      background: &'a BackgroundMetadata,
      preview_file: &'a str,
      elements: &'a [Element],
      next_sequence_number: u64,
    }

    let content = FingerprintContent {
      schema_version: self.schema_version,
      document_id: self.document_id,
      title: &self.title,
      canvas_size_px: self.canvas_size_px,
      background: &self.background,
      preview_file: &self.preview_file,
      elements: &self.elements,
      next_sequence_number: self.next_sequence_number,
    };
    let bytes =
      serde_json::to_vec(&content).expect("serializing in-memory document content cannot fail");
    ContentFingerprint {
      first: fnv1a(&bytes, 0xcbf29ce484222325),
      second: fnv1a(&bytes, 0x84222325cbf29ce4),
    }
  }

  pub fn dirty_baseline(&self) -> DirtyBaseline {
    DirtyBaseline(self.content_fingerprint())
  }

  pub fn is_dirty_against(&self, baseline: DirtyBaseline) -> bool {
    self.content_fingerprint() != baseline.0
  }

  pub fn element(&self, element_id: ElementId) -> Option<&Element> {
    self.elements.iter().find(|element| element.element_id == element_id)
  }

  pub fn highest_element(&self) -> Option<&Element> {
    self.elements.last()
  }

  pub fn persistent_stroke_point_count(&self) -> usize {
    self.elements.iter().map(Element::persistent_point_count).fold(0usize, usize::saturating_add)
  }

  pub(crate) fn commit_content_change(
    &mut self,
    changed_at: DateTime<Utc>,
  ) -> Result<(), DocumentError> {
    self.revision = self.revision.checked_add(1).ok_or(DocumentError::RevisionOverflow)?;
    self.updated_at = changed_at.max(self.updated_at);
    self.preview_revision = None;
    Ok(())
  }

  pub(crate) fn normalize_z_order(&mut self) -> Result<(), DocumentError> {
    for (index, element) in self.elements.iter_mut().enumerate() {
      element.z_index = i64::try_from(index).map_err(|_| DocumentError::ZIndexOverflow)?;
    }
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentSnapshot {
  pub schema_version: u32,
  pub document_id: DocumentId,
  pub title: String,
  pub canvas_size_px: SizePx,
  pub background: BackgroundMetadata,
  pub preview_file: String,
  pub preview_revision: Option<Revision>,
  pub elements: Vec<Element>,
  pub next_sequence_number: u64,
  pub revision: Revision,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
}

impl DocumentSnapshot {
  pub fn validate(&self) -> Result<(), DocumentError> {
    self.to_document().validate()
  }

  pub fn to_document(&self) -> BoardDocument {
    BoardDocument {
      schema_version: self.schema_version,
      document_id: self.document_id,
      title: self.title.clone(),
      canvas_size_px: self.canvas_size_px,
      background: self.background.clone(),
      preview_file: self.preview_file.clone(),
      preview_revision: self.preview_revision,
      elements: self.elements.clone(),
      next_sequence_number: self.next_sequence_number,
      revision: self.revision,
      created_at: self.created_at,
      updated_at: self.updated_at,
    }
  }

  pub fn into_document(self) -> BoardDocument {
    BoardDocument {
      schema_version: self.schema_version,
      document_id: self.document_id,
      title: self.title,
      canvas_size_px: self.canvas_size_px,
      background: self.background,
      preview_file: self.preview_file,
      preview_revision: self.preview_revision,
      elements: self.elements,
      next_sequence_number: self.next_sequence_number,
      revision: self.revision,
      created_at: self.created_at,
      updated_at: self.updated_at,
    }
  }
}

impl From<&BoardDocument> for DocumentSnapshot {
  fn from(document: &BoardDocument) -> Self {
    Self {
      schema_version: document.schema_version,
      document_id: document.document_id,
      title: document.title.clone(),
      canvas_size_px: document.canvas_size_px,
      background: document.background.clone(),
      preview_file: document.preview_file.clone(),
      preview_revision: document.preview_revision,
      elements: document.elements.clone(),
      next_sequence_number: document.next_sequence_number,
      revision: document.revision,
      created_at: document.created_at,
      updated_at: document.updated_at,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentFingerprint {
  pub first: u64,
  pub second: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirtyBaseline(ContentFingerprint);

impl DirtyBaseline {
  pub const fn fingerprint(self) -> ContentFingerprint {
    self.0
  }
}

fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
  bytes.iter().fold(seed, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3))
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum DocumentError {
  #[error("unsupported schema version {found}; this build supports {supported}")]
  UnsupportedSchema { found: u32, supported: u32 },
  #[error("document title must not be blank")]
  EmptyTitle,
  #[error(transparent)]
  Geometry(#[from] GeometryError),
  #[error(transparent)]
  Element(#[from] ElementError),
  #[error("background dimensions do not match the canvas")]
  BackgroundSizeMismatch,
  #[error("captured display dimensions do not match the canvas")]
  CapturedDisplaySizeMismatch,
  #[error("captured display scale factor must be finite and positive")]
  InvalidScaleFactor,
  #[error("invalid {field}: {source}")]
  InvalidResourceName { field: &'static str, source: ResourceNameError },
  #[error("next sequence number must be at least one")]
  InvalidNextSequenceNumber,
  #[error("preview revision cannot be ahead of the document revision")]
  PreviewRevisionAhead,
  #[error("updated_at cannot be before created_at")]
  UpdatedBeforeCreated,
  #[error("element limit exceeded: {count} > {limit}")]
  ElementLimitExceeded { count: usize, limit: usize },
  #[error("stroke point limit exceeded: {count} > {limit}")]
  StrokePointLimitExceeded { count: usize, limit: usize },
  #[error("stroke point count overflow")]
  StrokePointCountOverflow,
  #[error("duplicate element id {0}")]
  DuplicateElementId(ElementId),
  #[error("element at position {index} has z-index {z_index}; z-indices must be contiguous")]
  InvalidZOrder { index: usize, z_index: i64 },
  #[error("requested revision {expected}, current revision is {actual}")]
  RevisionMismatch { expected: Revision, actual: Revision },
  #[error("document revision overflow")]
  RevisionOverflow,
  #[error("z-index overflow")]
  ZIndexOverflow,
}

#[cfg(test)]
mod tests {
  use chrono::FixedOffset;

  use super::*;
  use crate::{
    element::{ArrowHead, ArrowPayload, ElementPayload, StrokePayload, StrokePoint, StrokeStyle},
    geometry::PointPx,
  };

  fn document() -> BoardDocument {
    let captured_at =
      FixedOffset::east_opt(8 * 60 * 60).unwrap().with_ymd_and_hms(2026, 8, 6, 14, 30, 45).unwrap();
    BoardDocument::new_capture(
      DocumentId::from_uuid(Uuid::nil()),
      SizePx::new(1920, 1080),
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx { x_px: -960, y_px: 0, width_px: 960, height_px: 540 },
        scale_factor: 2.0,
      },
      captured_at,
    )
    .unwrap()
  }

  #[test]
  fn new_capture_uses_capture_local_time_and_stable_names() {
    let document = document();
    assert_eq!(document.title, "截图 2026-08-06 14:30:45");
    assert_eq!(document.revision, 0);
    assert_eq!(document.next_sequence_number, 1);
    assert_eq!(document.preview_revision, None);
    assert_eq!(document.background.file, "00000000-0000-0000-0000-000000000000.png");
  }

  #[test]
  fn snapshot_requires_current_revision_and_is_deeply_isolated() {
    let mut document = document();
    let snapshot = document.snapshot(0).unwrap();
    document.title = "changed".to_owned();
    assert_ne!(snapshot.title, document.title);
    assert_eq!(
      document.snapshot(1),
      Err(DocumentError::RevisionMismatch { expected: 1, actual: 0 })
    );
  }

  #[test]
  fn dirty_fingerprint_ignores_revision_and_preview_metadata() {
    let mut document = document();
    let baseline = document.dirty_baseline();
    document.revision = 42;
    document.preview_revision = Some(41);
    document.updated_at += chrono::Duration::hours(1);
    assert!(!document.is_dirty_against(baseline));
    document.title.push('!');
    assert!(document.is_dirty_against(baseline));
  }

  #[test]
  fn retina_logical_bounds_map_to_physical_canvas_pixels() {
    let document = document();
    assert_eq!(document.canvas_size_px, SizePx::new(1920, 1080));
    assert_eq!(document.background.captured_display.global_bounds_px.width_px, 960);
    assert!(document.validate().is_ok());
  }

  fn arrow_element(element_id: ElementId, z_index: i64) -> Element {
    let style = StrokeStyle::default();
    Element::new(
      element_id,
      z_index,
      ElementPayload::Arrow(ArrowPayload {
        start_px: PointPx::new(100.0, 100.0),
        end_px: PointPx::new(300.0, 200.0),
        head: ArrowHead::for_stroke_width(style.width_px).unwrap(),
        stroke_style: style,
      }),
      SizePx::new(1920, 1080),
    )
    .unwrap()
  }

  #[test]
  fn element_limit_accepts_ten_thousand_and_rejects_one_more() {
    let mut document = document();
    document.elements = (0..MAX_ELEMENTS)
      .map(|index| {
        arrow_element(ElementId::from_uuid(Uuid::from_u128(index as u128 + 1)), index as i64)
      })
      .collect();
    assert!(document.validate().is_ok());
    document.elements.push(arrow_element(
      ElementId::from_uuid(Uuid::from_u128(MAX_ELEMENTS as u128 + 1)),
      MAX_ELEMENTS as i64,
    ));
    assert_eq!(
      document.validate(),
      Err(DocumentError::ElementLimitExceeded { count: 10_001, limit: 10_000 })
    );
  }

  #[test]
  fn stroke_point_limit_accepts_one_million_and_rejects_one_more() {
    let mut document = document();
    let points = (0..MAX_STROKE_POINTS)
      .map(|index| {
        StrokePoint::new(if index % 2 == 0 {
          PointPx::new(100.0, 100.0)
        } else {
          PointPx::new(120.0, 100.0)
        })
      })
      .collect();
    let stroke = Element::new(
      ElementId::from_uuid(Uuid::from_u128(1)),
      0,
      ElementPayload::Stroke(StrokePayload { points, stroke_style: StrokeStyle::default() }),
      document.canvas_size_px,
    )
    .unwrap();
    document.elements.push(stroke);
    assert!(document.validate().is_ok());
    let ElementPayload::Stroke(stroke) = &mut document.elements[0].payload else {
      unreachable!();
    };
    stroke.points.push(StrokePoint::new(PointPx::new(100.0, 100.0)));
    assert_eq!(
      document.validate(),
      Err(DocumentError::StrokePointLimitExceeded { count: 1_000_001, limit: 1_000_000 })
    );
  }

  #[test]
  fn duplicate_ids_and_non_contiguous_z_order_are_rejected() {
    let mut document = document();
    let id = ElementId::from_uuid(Uuid::from_u128(1));
    document.elements = vec![arrow_element(id, 0), arrow_element(id, 1)];
    assert_eq!(document.validate(), Err(DocumentError::DuplicateElementId(id)));
    document.elements[1].element_id = ElementId::from_uuid(Uuid::from_u128(2));
    document.elements[1].z_index = 3;
    assert_eq!(document.validate(), Err(DocumentError::InvalidZOrder { index: 1, z_index: 3 }));
  }
}
