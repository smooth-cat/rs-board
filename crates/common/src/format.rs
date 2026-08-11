use std::path::{Component, Path};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use thiserror::Error;

use crate::{
  document::{
    BackgroundMetadata, BoardDocument, CURRENT_SCHEMA_VERSION, DocumentError, DocumentId,
    DocumentSnapshot, Revision,
  },
  element::{
    ArrowHead, ArrowPayload, Element, ElementError, ElementId, ElementLabel, ElementPayload,
    RectangleLabelAnchor, RectangleLabelEdge, RectangleLabelSide, RectanglePayload,
    SequenceMarkerPayload, StrokePayload, StrokeStyle, TextPayload, TextStyle, layout_text,
  },
  geometry::{PointPx, RectPx, SizePx},
};

const LEGACY_SCHEMA_VERSION: u32 = 2;
const LEGACY_BOUNDS_EPSILON_PX: f32 = 0.5;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentManifestSummary {
  pub document_id: DocumentId,
  pub title: String,
  pub revision: Revision,
  pub updated_at: DateTime<Utc>,
  pub preview_file: String,
  pub preview_revision: Option<Revision>,
}

pub fn encode_document(document: &BoardDocument) -> Result<Vec<u8>, FormatError> {
  document.validate()?;
  serde_json::to_vec_pretty(document).map_err(|error| FormatError::Json(error.to_string()))
}

pub fn encode_snapshot(snapshot: &DocumentSnapshot) -> Result<Vec<u8>, FormatError> {
  snapshot.validate()?;
  serde_json::to_vec_pretty(snapshot).map_err(|error| FormatError::Json(error.to_string()))
}

pub fn decode_document(bytes: &[u8]) -> Result<BoardDocument, FormatError> {
  let schema = decode_schema(bytes)?;
  match schema {
    CURRENT_SCHEMA_VERSION => {
      let document: BoardDocument =
        serde_json::from_slice(bytes).map_err(|error| FormatError::Json(error.to_string()))?;
      document.validate()?;
      Ok(document)
    }
    LEGACY_SCHEMA_VERSION => decode_v2_document(bytes),
    _ => Err(FormatError::UnsupportedSchema { found: schema, supported: CURRENT_SCHEMA_VERSION }),
  }
}

pub fn decode_snapshot(bytes: &[u8]) -> Result<DocumentSnapshot, FormatError> {
  decode_document(bytes).map(|document| DocumentSnapshot::from(&document))
}

pub fn decode_document_summary(bytes: &[u8]) -> Result<DocumentManifestSummary, FormatError> {
  #[derive(Deserialize)]
  struct SummaryEnvelope {
    schema_version: u32,
    document_id: DocumentId,
    title: String,
    preview_file: String,
    preview_revision: Option<Revision>,
    revision: Revision,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
  }

  let envelope: SummaryEnvelope =
    serde_json::from_slice(bytes).map_err(|error| FormatError::Json(error.to_string()))?;
  if !matches!(envelope.schema_version, LEGACY_SCHEMA_VERSION | CURRENT_SCHEMA_VERSION) {
    return Err(FormatError::UnsupportedSchema {
      found: envelope.schema_version,
      supported: CURRENT_SCHEMA_VERSION,
    });
  }
  if envelope.title.trim().is_empty() {
    return Err(DocumentError::EmptyTitle.into());
  }
  validate_resource_name(&envelope.preview_file).map_err(|source| {
    FormatError::InvalidDocument(DocumentError::InvalidResourceName {
      field: "preview_file",
      source,
    })
  })?;
  if envelope.preview_revision.is_some_and(|preview| preview > envelope.revision) {
    return Err(DocumentError::PreviewRevisionAhead.into());
  }
  if envelope.updated_at < envelope.created_at {
    return Err(DocumentError::UpdatedBeforeCreated.into());
  }

  Ok(DocumentManifestSummary {
    document_id: envelope.document_id,
    title: envelope.title,
    revision: envelope.revision,
    updated_at: envelope.updated_at,
    preview_file: envelope.preview_file,
    preview_revision: envelope.preview_revision,
  })
}

pub fn validate_resource_name(name: &str) -> Result<(), ResourceNameError> {
  if name.is_empty() {
    return Err(ResourceNameError::Empty);
  }
  if name.contains('\0') {
    return Err(ResourceNameError::NulByte);
  }
  if name.contains('/') || name.contains('\\') {
    return Err(ResourceNameError::PathSeparator);
  }
  if name.as_bytes().get(1) == Some(&b':')
    && name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
  {
    return Err(ResourceNameError::Absolute);
  }
  if name == "." || name == ".." {
    return Err(ResourceNameError::Traversal);
  }
  let path = Path::new(name);
  if path.is_absolute() {
    return Err(ResourceNameError::Absolute);
  }
  let mut components = path.components();
  match (components.next(), components.next()) {
    (Some(Component::Normal(component)), None) if component == name => Ok(()),
    (Some(Component::ParentDir | Component::CurDir), _) => Err(ResourceNameError::Traversal),
    _ => Err(ResourceNameError::NotPlainFileName),
  }
}

pub fn validate_managed_resource_names(document: &BoardDocument) -> Result<(), FormatError> {
  document.validate()?;
  let expected_background = format!("{}.png", document.document_id);
  let expected_preview = format!("{}.preview.png", document.document_id);
  if document.background.file != expected_background {
    return Err(FormatError::ManagedResourceNameMismatch {
      field: "background.file",
      expected: expected_background,
      actual: document.background.file.clone(),
    });
  }
  if document.preview_file != expected_preview {
    return Err(FormatError::ManagedResourceNameMismatch {
      field: "preview_file",
      expected: expected_preview,
      actual: document.preview_file.clone(),
    });
  }
  Ok(())
}

fn decode_schema(bytes: &[u8]) -> Result<u32, FormatError> {
  #[derive(Deserialize)]
  struct SchemaEnvelope {
    schema_version: u32,
  }

  serde_json::from_slice::<SchemaEnvelope>(bytes).map(|envelope| envelope.schema_version).map_err(
    |error| {
      if error.to_string().contains("schema_version") {
        FormatError::MissingOrInvalidSchema
      } else {
        FormatError::Json(error.to_string())
      }
    },
  )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2BoardDocument {
  schema_version: u32,
  document_id: DocumentId,
  title: String,
  canvas_size_px: SizePx,
  background: BackgroundMetadata,
  preview_file: String,
  preview_revision: Option<Revision>,
  elements: Vec<V2Element>,
  next_sequence_number: u64,
  revision: Revision,
  created_at: DateTime<Utc>,
  updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2Element {
  element_id: ElementId,
  z_index: i64,
  bounds_px: RectPx,
  #[serde(flatten)]
  payload: V2ElementPayload,
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
enum V2ElementPayload {
  Stroke(StrokePayload),
  Arrow(V2ArrowPayload),
  Rectangle(V2RectanglePayload),
  Text(TextPayload),
  SequenceMarker(SequenceMarkerPayload),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2ArrowPayload {
  start_px: PointPx,
  end_px: PointPx,
  stroke_style: StrokeStyle,
  head: ArrowHead,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2RectanglePayload {
  start_px: PointPx,
  end_px: PointPx,
  stroke_style: StrokeStyle,
  fill_rgba: Option<crate::element::ColorRgba>,
  label: V2RectangleLabel,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2RectangleLabel {
  text: String,
  placement_preference: V2LabelPlacementPreference,
  max_width_px: f32,
  padding_px: f32,
  anchor_offset_px: f32,
  text_style: TextStyle,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum V2LabelPlacementPreference {
  Above,
}

#[derive(Clone, Copy)]
enum V2DerivedLabelPlacement {
  Above,
  Below,
}

#[derive(Clone, Copy)]
struct V2RectangleLabelLayout {
  bounds_px: RectPx,
  placement: V2DerivedLabelPlacement,
}

fn decode_v2_document(bytes: &[u8]) -> Result<BoardDocument, FormatError> {
  let legacy: V2BoardDocument =
    serde_json::from_slice(bytes).map_err(|error| FormatError::Json(error.to_string()))?;
  if legacy.schema_version != LEGACY_SCHEMA_VERSION {
    return Err(FormatError::UnsupportedSchema {
      found: legacy.schema_version,
      supported: CURRENT_SCHEMA_VERSION,
    });
  }

  let elements = legacy
    .elements
    .into_iter()
    .map(|element| migrate_v2_element(element, legacy.canvas_size_px))
    .collect::<Result<Vec<_>, _>>()?;
  let document = BoardDocument {
    schema_version: CURRENT_SCHEMA_VERSION,
    document_id: legacy.document_id,
    title: legacy.title,
    canvas_size_px: legacy.canvas_size_px,
    background: legacy.background,
    preview_file: legacy.preview_file,
    preview_revision: legacy.preview_revision,
    elements,
    next_sequence_number: legacy.next_sequence_number,
    revision: legacy.revision,
    created_at: legacy.created_at,
    updated_at: legacy.updated_at,
  };
  document.validate()?;
  Ok(document)
}

fn migrate_v2_element(legacy: V2Element, canvas_size_px: SizePx) -> Result<Element, FormatError> {
  legacy.bounds_px.validate().map_err(ElementError::from).map_err(element_format_error)?;

  let (payload, expected_v2_bounds) = match legacy.payload {
    V2ElementPayload::Stroke(payload) => {
      let payload = ElementPayload::Stroke(payload);
      let expected = refreshed_bounds_for_v2_payload(
        legacy.element_id,
        legacy.z_index,
        payload.clone(),
        canvas_size_px,
      )?;
      (payload, expected)
    }
    V2ElementPayload::Arrow(payload) => {
      let payload = ElementPayload::Arrow(ArrowPayload {
        start_px: payload.start_px,
        end_px: payload.end_px,
        label: default_migrated_arrow_label(&payload.stroke_style)?,
        stroke_style: payload.stroke_style,
        head: payload.head,
      });
      let expected = refreshed_bounds_for_v2_payload(
        legacy.element_id,
        legacy.z_index,
        payload.clone(),
        canvas_size_px,
      )?;
      (payload, expected)
    }
    V2ElementPayload::Rectangle(payload) => {
      let legacy_layout = v2_rectangle_label_layout(&payload, canvas_size_px)?;
      let body = RectPx::from_points(payload.start_px, payload.end_px);
      let expected =
        body.expanded(payload.stroke_style.width_px / 2.0).union(legacy_layout.bounds_px);
      let position = if body.width().is_finite() && body.width() > 0.0 {
        ((legacy_layout.bounds_px.min.x_px - body.min.x_px - payload.label.anchor_offset_px)
          / body.width())
        .clamp(0.0, 1.0)
      } else {
        0.0
      };
      let edge = match legacy_layout.placement {
        V2DerivedLabelPlacement::Above => RectangleLabelEdge::Top,
        V2DerivedLabelPlacement::Below => RectangleLabelEdge::Bottom,
      };
      let label = ElementLabel {
        text: Some(payload.label.text),
        max_width_px: payload.label.max_width_px,
        padding_px: payload.label.padding_px,
        anchor_offset_px: payload.label.anchor_offset_px,
        text_style: payload.label.text_style,
      };
      (
        ElementPayload::Rectangle(RectanglePayload {
          start_px: payload.start_px,
          end_px: payload.end_px,
          stroke_style: payload.stroke_style,
          fill_rgba: payload.fill_rgba,
          label,
          label_anchor: RectangleLabelAnchor::new(edge, RectangleLabelSide::Outside, position),
        }),
        expected,
      )
    }
    V2ElementPayload::Text(payload) => {
      let payload = ElementPayload::Text(payload);
      let expected = refreshed_bounds_for_v2_payload(
        legacy.element_id,
        legacy.z_index,
        payload.clone(),
        canvas_size_px,
      )?;
      (payload, expected)
    }
    V2ElementPayload::SequenceMarker(payload) => {
      let payload = ElementPayload::SequenceMarker(payload);
      let expected = refreshed_bounds_for_v2_payload(
        legacy.element_id,
        legacy.z_index,
        payload.clone(),
        canvas_size_px,
      )?;
      (payload, expected)
    }
  };

  if !v2_bounds_approximately_equal(legacy.bounds_px, expected_v2_bounds) {
    return Err(element_format_error(ElementError::StaleBounds));
  }

  let mut migrated = Element {
    element_id: legacy.element_id,
    z_index: legacy.z_index,
    bounds_px: legacy.bounds_px,
    payload,
  };
  migrated.refresh_bounds(canvas_size_px).map_err(element_format_error)?;
  migrated.validate(canvas_size_px).map_err(element_format_error)?;
  Ok(migrated)
}

fn refreshed_bounds_for_v2_payload(
  element_id: ElementId,
  z_index: i64,
  payload: ElementPayload,
  canvas_size_px: SizePx,
) -> Result<RectPx, FormatError> {
  let mut element = Element {
    element_id,
    z_index,
    bounds_px: RectPx::from_min_max(PointPx::ZERO, PointPx::ZERO),
    payload,
  };
  element.refresh_bounds(canvas_size_px).map_err(element_format_error)?;
  Ok(element.bounds_px)
}

fn default_migrated_arrow_label(stroke_style: &StrokeStyle) -> Result<ElementLabel, FormatError> {
  Ok(ElementLabel {
    text: None,
    max_width_px: 420.0,
    padding_px: 8.0,
    anchor_offset_px: 8.0,
    text_style: TextStyle::mvp(stroke_style.color_rgba.contrasting_text(), 24.0)
      .map_err(element_format_error)?,
  })
}

fn v2_rectangle_label_layout(
  rectangle: &V2RectanglePayload,
  canvas_size_px: SizePx,
) -> Result<V2RectangleLabelLayout, FormatError> {
  canvas_size_px.validate().map_err(ElementError::from).map_err(element_format_error)?;
  let _ = rectangle.label.placement_preference;
  let canvas = canvas_size_px.bounds();
  let body = RectPx::from_points(rectangle.start_px, rectangle.end_px);
  let maximum_width_px = rectangle
    .label
    .max_width_px
    .min(body.width() * 1.5)
    .min(canvas.width())
    .max(rectangle.label.padding_px * 2.0 + 1.0);
  let text_wrap_width_px = (maximum_width_px - rectangle.label.padding_px * 2.0).max(1.0);
  let text_layout =
    layout_text(&rectangle.label.text, &rectangle.label.text_style, text_wrap_width_px)
      .map_err(element_format_error)?;
  let width_px =
    (text_layout.width_px + rectangle.label.padding_px * 2.0).min(maximum_width_px).max(1.0);
  let height_px = text_layout.height_px + rectangle.label.padding_px * 2.0;
  let x_px = (body.min.x_px + rectangle.label.anchor_offset_px)
    .clamp(0.0, (canvas.width() - width_px).max(0.0));
  let margin_px = rectangle.label.anchor_offset_px;
  let (placement, candidate_y_px) = if body.min.y_px - height_px - margin_px >= 0.0 {
    (V2DerivedLabelPlacement::Above, body.min.y_px - height_px - margin_px)
  } else {
    (V2DerivedLabelPlacement::Below, body.max.y_px + margin_px)
  };
  let y_px = candidate_y_px.clamp(0.0, (canvas.height() - height_px).max(0.0));
  Ok(V2RectangleLabelLayout {
    bounds_px: RectPx::from_min_max(
      PointPx::new(x_px, y_px),
      PointPx::new(x_px + width_px, y_px + height_px),
    ),
    placement,
  })
}

fn v2_bounds_approximately_equal(left: RectPx, right: RectPx) -> bool {
  (left.min.x_px - right.min.x_px).abs() <= LEGACY_BOUNDS_EPSILON_PX
    && (left.min.y_px - right.min.y_px).abs() <= LEGACY_BOUNDS_EPSILON_PX
    && (left.max.x_px - right.max.x_px).abs() <= LEGACY_BOUNDS_EPSILON_PX
    && (left.max.y_px - right.max.y_px).abs() <= LEGACY_BOUNDS_EPSILON_PX
}

fn element_format_error(error: ElementError) -> FormatError {
  FormatError::InvalidDocument(DocumentError::Element(error))
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ResourceNameError {
  #[error("resource name must not be empty")]
  Empty,
  #[error("resource name must not contain NUL")]
  NulByte,
  #[error("resource name must not contain path separators")]
  PathSeparator,
  #[error("resource name must not contain parent or current directory components")]
  Traversal,
  #[error("resource name must not be absolute")]
  Absolute,
  #[error("resource name must be one ordinary file name")]
  NotPlainFileName,
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum FormatError {
  #[error("invalid JSON: {0}")]
  Json(String),
  #[error("manifest schema_version is missing or invalid")]
  MissingOrInvalidSchema,
  #[error("unsupported schema version {found}; this build supports {supported}")]
  UnsupportedSchema { found: u32, supported: u32 },
  #[error(transparent)]
  InvalidDocument(#[from] DocumentError),
  #[error("managed {field} must be {expected}, found {actual}")]
  ManagedResourceNameMismatch { field: &'static str, expected: String, actual: String },
}

#[cfg(test)]
mod tests {
  use chrono::{TimeZone, Utc};
  use uuid::Uuid;

  use super::*;
  use crate::{
    command::{CommandBatch, DocumentCommand},
    document::{CapturedDisplay, DocumentId, GlobalBoundsPx},
    element::{
      ArrowHead, ArrowPayload, ColorRgba, Element, ElementId, ElementLabel, ElementPayload,
      RectangleLabelAnchor, RectangleLabelEdge, RectangleLabelSide, RectanglePayload,
      SequenceMarkerPayload, StrokePayload, StrokePoint, StrokeStyle, TextPayload, TextStyle,
    },
    geometry::{PointPx, SizePx},
  };

  fn document() -> BoardDocument {
    BoardDocument::new_capture(
      DocumentId::from_uuid(Uuid::nil()),
      SizePx::new(1280, 720),
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx { x_px: -640, y_px: 0, width_px: 640, height_px: 360 },
        scale_factor: 2.0,
      },
      Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).unwrap(),
    )
    .unwrap()
  }

  fn document_with_all_elements() -> BoardDocument {
    let mut document = document();
    let canvas = document.canvas_size_px;

    let stroke_style = StrokeStyle::mvp(ColorRgba::GREEN, 4.0).unwrap();
    let stroke = Element::new(
      ElementId::from_uuid(Uuid::from_u128(1)),
      0,
      ElementPayload::Stroke(
        StrokePayload::from_raw_points(
          &[PointPx::new(40.0, 620.0), PointPx::new(100.0, 650.0), PointPx::new(180.0, 610.0)],
          stroke_style,
        )
        .unwrap(),
      ),
      canvas,
    )
    .unwrap();

    let arrow_style = StrokeStyle::mvp(ColorRgba::BLUE, 8.0).unwrap();
    let arrow = Element::new(
      ElementId::from_uuid(Uuid::from_u128(2)),
      0,
      ElementPayload::Arrow(ArrowPayload {
        start_px: PointPx::new(80.0, 80.0),
        end_px: PointPx::new(320.0, 180.0),
        head: ArrowHead::for_stroke_width(arrow_style.width_px).unwrap(),
        label: ElementLabel {
          text: None,
          max_width_px: 420.0,
          padding_px: 8.0,
          anchor_offset_px: 8.0,
          text_style: TextStyle::mvp(arrow_style.color_rgba.contrasting_text(), 24.0).unwrap(),
        },
        stroke_style: arrow_style,
      }),
      canvas,
    )
    .unwrap();

    let rectangle_color = ColorRgba::YELLOW;
    let rectangle = Element::new(
      ElementId::from_uuid(Uuid::from_u128(3)),
      0,
      ElementPayload::Rectangle(RectanglePayload {
        start_px: PointPx::new(400.0, 180.0),
        end_px: PointPx::new(800.0, 450.0),
        stroke_style: StrokeStyle::mvp(rectangle_color, 12.0).unwrap(),
        fill_rgba: None,
        label: ElementLabel {
          text: Some("章节标题".to_owned()),
          max_width_px: 480.0,
          padding_px: 8.0,
          anchor_offset_px: 8.0,
          text_style: TextStyle::mvp(rectangle_color.contrasting_text(), 36.0).unwrap(),
        },
        label_anchor: RectangleLabelAnchor::new(
          RectangleLabelEdge::Top,
          RectangleLabelSide::Outside,
          0.0,
        ),
      }),
      canvas,
    )
    .unwrap();

    let text = Element::new(
      ElementId::from_uuid(Uuid::from_u128(4)),
      0,
      ElementPayload::Text(TextPayload {
        anchor_px: PointPx::new(820.0, 220.0),
        text: "第一行\n第二行".to_owned(),
        box_width_px: 300.0,
        text_style: TextStyle::mvp(ColorRgba::WHITE, 24.0).unwrap(),
      }),
      canvas,
    )
    .unwrap();

    let marker_color = ColorRgba::RED;
    let marker = Element::new(
      ElementId::from_uuid(Uuid::from_u128(5)),
      0,
      ElementPayload::SequenceMarker(SequenceMarkerPayload {
        center_px: PointPx::new(1100.0, 600.0),
        number: 1,
        radius_px: 18.0,
        pill_width_px: 36.0,
        fill_rgba: marker_color,
        stroke_style: StrokeStyle::mvp(marker_color, 4.0).unwrap(),
        text_style: TextStyle::mvp(marker_color.contrasting_text(), 16.0).unwrap(),
      }),
      canvas,
    )
    .unwrap();

    for element in [stroke, arrow, rectangle, text] {
      DocumentCommand::AddElement { element }.apply(&mut document).unwrap();
    }
    CommandBatch::sequence_marker(&document, marker).unwrap().apply(&mut document).unwrap();
    document
  }

  fn v2_value(document: &BoardDocument) -> serde_json::Value {
    let mut value = serde_json::to_value(document).unwrap();
    value["schema_version"] = LEGACY_SCHEMA_VERSION.into();

    for (json_element, element) in
      value["elements"].as_array_mut().unwrap().iter_mut().zip(&document.elements)
    {
      let json_payload = json_element["payload"].as_object_mut().unwrap();
      match &element.payload {
        ElementPayload::Arrow(_) => {
          json_payload.remove("label");
        }
        ElementPayload::Rectangle(rectangle) => {
          json_payload.remove("label_anchor");
          json_payload["label"]
            .as_object_mut()
            .unwrap()
            .insert("placement_preference".to_owned(), serde_json::json!("above"));

          let legacy = V2RectanglePayload {
            start_px: rectangle.start_px,
            end_px: rectangle.end_px,
            stroke_style: rectangle.stroke_style.clone(),
            fill_rgba: rectangle.fill_rgba,
            label: V2RectangleLabel {
              text: rectangle.label.text.clone().unwrap(),
              placement_preference: V2LabelPlacementPreference::Above,
              max_width_px: rectangle.label.max_width_px,
              padding_px: rectangle.label.padding_px,
              anchor_offset_px: rectangle.label.anchor_offset_px,
              text_style: rectangle.label.text_style.clone(),
            },
          };
          let label_layout = v2_rectangle_label_layout(&legacy, document.canvas_size_px).unwrap();
          let bounds = RectPx::from_points(rectangle.start_px, rectangle.end_px)
            .expanded(rectangle.stroke_style.width_px / 2.0)
            .union(label_layout.bounds_px);
          json_element["bounds_px"] = serde_json::to_value(bounds).unwrap();
        }
        _ => {}
      }
    }
    value
  }

  #[test]
  fn document_json_round_trip_is_stable() {
    let document = document();
    let encoded = encode_document(&document).unwrap();
    let decoded = decode_document(&encoded).unwrap();
    assert_eq!(decoded, document);
    let json = String::from_utf8(encoded).unwrap();
    assert!(json.contains("\"schema_version\": 3"));
    assert!(json.contains("\"document_id\": \"00000000-0000-0000-0000-000000000000\""));
    assert!(!json.contains("history"));
    assert!(!json.contains("selection"));
  }

  #[test]
  fn summary_decode_reads_only_list_metadata() {
    let document = document_with_all_elements();
    let mut value = serde_json::to_value(&document).unwrap();
    value["elements"] = serde_json::json!([{"unsupported_future_element": true}]);
    for schema_version in [LEGACY_SCHEMA_VERSION, CURRENT_SCHEMA_VERSION] {
      value["schema_version"] = schema_version.into();
      let summary = decode_document_summary(&serde_json::to_vec(&value).unwrap()).unwrap();
      assert_eq!(summary.document_id, document.document_id);
      assert_eq!(summary.title, document.title);
      assert_eq!(summary.revision, document.revision);
      assert_eq!(summary.updated_at, document.updated_at);
      assert_eq!(summary.preview_file, document.preview_file);
      assert_eq!(summary.preview_revision, document.preview_revision);
    }
    assert!(decode_document(&serde_json::to_vec(&value).unwrap()).is_err());

    for schema_version in [1, CURRENT_SCHEMA_VERSION + 1] {
      value["schema_version"] = schema_version.into();
      assert_eq!(
        decode_document_summary(&serde_json::to_vec(&value).unwrap()),
        Err(FormatError::UnsupportedSchema {
          found: schema_version,
          supported: CURRENT_SCHEMA_VERSION,
        })
      );
    }
  }

  #[test]
  fn v2_document_migrates_labels_anchors_and_bounds_without_metadata_churn() {
    let document = document_with_all_elements();
    let value = v2_value(&document);
    let legacy_rectangle: V2RectanglePayload =
      serde_json::from_value(value["elements"][2]["payload"].clone()).unwrap();
    let legacy_label_layout =
      v2_rectangle_label_layout(&legacy_rectangle, document.canvas_size_px).unwrap();
    let bytes = serde_json::to_vec(&value).unwrap();
    let migrated = decode_document(&bytes).unwrap();

    assert_eq!(migrated.schema_version, CURRENT_SCHEMA_VERSION);
    assert_eq!(migrated.document_id, document.document_id);
    assert_eq!(migrated.title, document.title);
    assert_eq!(migrated.preview_file, document.preview_file);
    assert_eq!(migrated.preview_revision, document.preview_revision);
    assert_eq!(migrated.revision, document.revision);
    assert_eq!(migrated.created_at, document.created_at);
    assert_eq!(migrated.updated_at, document.updated_at);

    let arrow = migrated
      .elements
      .iter()
      .find_map(|element| match &element.payload {
        ElementPayload::Arrow(payload) => Some(payload),
        _ => None,
      })
      .unwrap();
    assert_eq!(arrow.label.text, None);
    assert_eq!(arrow.label.max_width_px, 420.0);
    assert_eq!(arrow.label.padding_px, 8.0);
    assert_eq!(arrow.label.anchor_offset_px, 8.0);
    assert_eq!(arrow.label.text_style.font_size_px, 24.0);
    assert_eq!(arrow.label.text_style.color_rgba, arrow.stroke_style.color_rgba.contrasting_text());

    let rectangle_element = migrated
      .elements
      .iter()
      .find(|element| matches!(element.payload, ElementPayload::Rectangle(_)))
      .unwrap();
    let ElementPayload::Rectangle(rectangle) = &rectangle_element.payload else {
      unreachable!();
    };
    assert_eq!(rectangle.label.text.as_deref(), Some("章节标题"));
    assert_eq!(rectangle.label.max_width_px, 480.0);
    assert_eq!(rectangle.label.padding_px, 8.0);
    assert_eq!(rectangle.label.anchor_offset_px, 8.0);
    assert_eq!(rectangle.label_anchor.edge, RectangleLabelEdge::Top);
    assert_eq!(rectangle.label_anchor.side, RectangleLabelSide::Outside);
    assert_eq!(rectangle.label_anchor.position, 0.0);
    rectangle_element.validate(migrated.canvas_size_px).unwrap();
    let migrated_label_layout =
      crate::element::rectangle_label_layout(rectangle, migrated.canvas_size_px).unwrap().unwrap();
    assert!(v2_bounds_approximately_equal(
      migrated_label_layout.bounds_px,
      legacy_label_layout.bounds_px,
    ));

    let snapshot = decode_snapshot(&bytes).unwrap();
    assert_eq!(snapshot.schema_version, CURRENT_SCHEMA_VERSION);
    assert_eq!(snapshot.revision, document.revision);
    assert_eq!(snapshot.updated_at, document.updated_at);
  }

  #[test]
  fn v2_rectangle_near_the_top_migrates_to_bottom_outside() {
    let mut document = document_with_all_elements();
    let rectangle_element = document
      .elements
      .iter_mut()
      .find(|element| matches!(element.payload, ElementPayload::Rectangle(_)))
      .unwrap();
    let ElementPayload::Rectangle(rectangle) = &mut rectangle_element.payload else {
      unreachable!();
    };
    rectangle.start_px.y_px = 10.0;
    rectangle.end_px.y_px = 280.0;
    rectangle_element.refresh_bounds(document.canvas_size_px).unwrap();
    document.validate().unwrap();

    let migrated = decode_document(&serde_json::to_vec(&v2_value(&document)).unwrap()).unwrap();
    let rectangle = migrated
      .elements
      .iter()
      .find_map(|element| match &element.payload {
        ElementPayload::Rectangle(payload) => Some(payload),
        _ => None,
      })
      .unwrap();
    assert_eq!(rectangle.label_anchor.edge, RectangleLabelEdge::Bottom);
    assert_eq!(rectangle.label_anchor.side, RectangleLabelSide::Outside);
  }

  #[test]
  fn v2_rectangle_canvas_correction_keeps_the_legacy_label_position() {
    let mut document = document_with_all_elements();
    let rectangle_element = document
      .elements
      .iter_mut()
      .find(|element| matches!(element.payload, ElementPayload::Rectangle(_)))
      .unwrap();
    let ElementPayload::Rectangle(rectangle) = &mut rectangle_element.payload else {
      unreachable!();
    };
    rectangle.start_px.x_px = 1_160.0;
    rectangle.end_px.x_px = 1_270.0;
    rectangle_element.refresh_bounds(document.canvas_size_px).unwrap();
    document.validate().unwrap();

    let value = v2_value(&document);
    let legacy_rectangle: V2RectanglePayload =
      serde_json::from_value(value["elements"][2]["payload"].clone()).unwrap();
    let legacy_layout =
      v2_rectangle_label_layout(&legacy_rectangle, document.canvas_size_px).unwrap();
    let migrated = decode_document(&serde_json::to_vec(&value).unwrap()).unwrap();
    let rectangle = migrated
      .elements
      .iter()
      .find_map(|element| match &element.payload {
        ElementPayload::Rectangle(payload) => Some(payload),
        _ => None,
      })
      .unwrap();
    let migrated_layout =
      crate::element::rectangle_label_layout(rectangle, migrated.canvas_size_px).unwrap().unwrap();
    assert!(v2_bounds_approximately_equal(migrated_layout.bounds_px, legacy_layout.bounds_px,));
  }

  #[test]
  fn v2_narrow_rectangle_keeps_the_legacy_label_inset() {
    let mut document = document_with_all_elements();
    let rectangle_element = document
      .elements
      .iter_mut()
      .find(|element| matches!(element.payload, ElementPayload::Rectangle(_)))
      .unwrap();
    let ElementPayload::Rectangle(rectangle) = &mut rectangle_element.payload else {
      unreachable!();
    };
    rectangle.end_px.x_px = rectangle.start_px.x_px + 6.0;
    rectangle.stroke_style = StrokeStyle::mvp(ColorRgba::YELLOW, 4.0).unwrap();
    rectangle_element.refresh_bounds(document.canvas_size_px).unwrap();
    document.validate().unwrap();

    let value = v2_value(&document);
    let legacy_rectangle: V2RectanglePayload =
      serde_json::from_value(value["elements"][2]["payload"].clone()).unwrap();
    let legacy_layout =
      v2_rectangle_label_layout(&legacy_rectangle, document.canvas_size_px).unwrap();
    let migrated = decode_document(&serde_json::to_vec(&value).unwrap()).unwrap();
    let rectangle = migrated
      .elements
      .iter()
      .find_map(|element| match &element.payload {
        ElementPayload::Rectangle(payload) => Some(payload),
        _ => None,
      })
      .unwrap();
    let migrated_layout =
      crate::element::rectangle_label_layout(rectangle, migrated.canvas_size_px).unwrap().unwrap();
    assert_eq!(rectangle.label_anchor.position, 0.0);
    assert!(v2_bounds_approximately_equal(migrated_layout.bounds_px, legacy_layout.bounds_px,));
  }

  #[test]
  fn v2_decode_remains_strict_and_rejects_stale_legacy_bounds() {
    let document = document_with_all_elements();
    let mut value = v2_value(&document);
    value.as_object_mut().unwrap().insert("runtime_selection".to_owned(), true.into());
    assert!(matches!(
      decode_document(&serde_json::to_vec(&value).unwrap()),
      Err(FormatError::Json(_))
    ));

    let mut value = v2_value(&document);
    value["elements"][1]["payload"]
      .as_object_mut()
      .unwrap()
      .insert("future_arrow_field".to_owned(), true.into());
    assert!(matches!(
      decode_document(&serde_json::to_vec(&value).unwrap()),
      Err(FormatError::Json(_))
    ));

    let mut value = v2_value(&document);
    value["elements"][2]["bounds_px"]["min"]["x_px"] = 395.0.into();
    assert_eq!(
      decode_document(&serde_json::to_vec(&value).unwrap()),
      Err(FormatError::InvalidDocument(DocumentError::Element(ElementError::StaleBounds)))
    );
  }

  #[test]
  fn unknown_higher_schema_is_rejected_before_domain_decode() {
    let bytes = br#"{"schema_version":999,"arbitrary":"future"}"#;
    assert_eq!(
      decode_document(bytes),
      Err(FormatError::UnsupportedSchema { found: 999, supported: CURRENT_SCHEMA_VERSION })
    );
  }

  #[test]
  fn schema_one_documents_are_rejected_after_brush_hardness_is_added() {
    let mut value = serde_json::to_value(document()).unwrap();
    value["schema_version"] = 1.into();
    assert_eq!(
      decode_document(&serde_json::to_vec(&value).unwrap()),
      Err(FormatError::UnsupportedSchema { found: 1, supported: CURRENT_SCHEMA_VERSION })
    );
  }

  #[test]
  fn resource_name_validation_blocks_path_attacks() {
    for invalid in [
      "",
      ".",
      "..",
      "../image.png",
      "folder/image.png",
      "folder\\image.png",
      "C:\\image.png",
      "C:image.png",
      "/tmp/image.png",
      "image\0.png",
    ] {
      assert!(validate_resource_name(invalid).is_err(), "accepted {invalid:?}");
    }
    assert!(validate_resource_name("标题-2.png").is_ok());
  }

  #[test]
  fn exported_title_resource_names_are_valid_but_not_managed_names() {
    let mut document = document();
    document.background.file = "课程-2.png".to_owned();
    document.preview_file = "课程-2.preview.png".to_owned();
    assert!(document.validate().is_ok());
    assert!(matches!(
      validate_managed_resource_names(&document),
      Err(FormatError::ManagedResourceNameMismatch { .. })
    ));
  }

  #[test]
  fn all_element_payloads_round_trip_with_tagged_shape() {
    let document = document_with_all_elements();
    let encoded = encode_document(&document).unwrap();
    let json = String::from_utf8(encoded.clone()).unwrap();
    for kind in ["stroke", "arrow", "rectangle", "text", "sequence_marker"] {
      assert!(json.contains(&format!("\"kind\": \"{kind}\"")));
    }
    assert!(json.contains("\"hardness\": 1.0"));
    assert!(json.contains("第一行\\n第二行"));
    assert_eq!(decode_document(&encoded).unwrap(), document);
  }

  #[test]
  fn pressure_values_round_trip_without_changing_schema_version() {
    let mut document = document_with_all_elements();
    let ElementPayload::Stroke(stroke) = &mut document.elements[0].payload else {
      unreachable!();
    };
    stroke.points[0] = StrokePoint::with_pressure(stroke.points[0].point(), 0.25).unwrap();
    stroke.points[1] = StrokePoint::with_pressure(stroke.points[1].point(), 0.75).unwrap();

    let encoded = encode_document(&document).unwrap();
    let json = String::from_utf8(encoded.clone()).unwrap();
    assert!(json.contains("\"schema_version\": 3"));
    assert!(json.contains("\"pressure\": 0.25"));
    assert!(json.contains("\"pressure\": 0.75"));
    assert_eq!(decode_document(&encoded).unwrap(), document);
  }

  #[test]
  fn unknown_fields_and_stale_element_bounds_are_rejected() {
    let document = document_with_all_elements();
    let mut value = serde_json::to_value(&document).unwrap();
    value.as_object_mut().unwrap().insert("runtime_selection".into(), true.into());
    assert!(matches!(
      decode_document(&serde_json::to_vec(&value).unwrap()),
      Err(FormatError::Json(_))
    ));

    let mut value = serde_json::to_value(&document).unwrap();
    value["elements"][0].as_object_mut().unwrap().insert("runtime_hover".into(), true.into());
    assert!(matches!(
      decode_document(&serde_json::to_vec(&value).unwrap()),
      Err(FormatError::Json(_))
    ));

    let mut value = serde_json::to_value(&document).unwrap();
    value["elements"][0]["bounds_px"]["min"]["x_px"] = 999.0.into();
    assert!(matches!(
      decode_document(&serde_json::to_vec(&value).unwrap()),
      Err(FormatError::InvalidDocument(DocumentError::Element(_)))
    ));
  }
}
