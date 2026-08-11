use std::path::{Component, Path};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use thiserror::Error;

use crate::document::{
  BoardDocument, CURRENT_SCHEMA_VERSION, DocumentError, DocumentId, DocumentSnapshot, Revision,
};

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
  if schema != CURRENT_SCHEMA_VERSION {
    return Err(FormatError::UnsupportedSchema {
      found: schema,
      supported: CURRENT_SCHEMA_VERSION,
    });
  }
  let document: BoardDocument =
    serde_json::from_slice(bytes).map_err(|error| FormatError::Json(error.to_string()))?;
  document.validate()?;
  Ok(document)
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
  if envelope.schema_version != CURRENT_SCHEMA_VERSION {
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
      ArrowHead, ArrowPayload, ColorRgba, Element, ElementId, ElementPayload,
      LabelPlacementPreference, RectangleLabel, RectanglePayload, SequenceMarkerPayload,
      StrokePayload, StrokeStyle, TextPayload, TextStyle,
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
        label: RectangleLabel {
          text: "章节标题".to_owned(),
          placement_preference: LabelPlacementPreference::Above,
          max_width_px: 480.0,
          padding_px: 8.0,
          anchor_offset_px: 8.0,
          text_style: TextStyle::mvp(rectangle_color.contrasting_text(), 36.0).unwrap(),
        },
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

  #[test]
  fn document_json_round_trip_is_stable() {
    let document = document();
    let encoded = encode_document(&document).unwrap();
    let decoded = decode_document(&encoded).unwrap();
    assert_eq!(decoded, document);
    let json = String::from_utf8(encoded).unwrap();
    assert!(json.contains("\"schema_version\": 2"));
    assert!(json.contains("\"document_id\": \"00000000-0000-0000-0000-000000000000\""));
    assert!(!json.contains("history"));
    assert!(!json.contains("selection"));
  }

  #[test]
  fn summary_decode_reads_only_list_metadata() {
    let document = document_with_all_elements();
    let mut value = serde_json::to_value(&document).unwrap();
    value["elements"] = serde_json::json!([{"unsupported_future_element": true}]);
    let summary = decode_document_summary(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(summary.document_id, document.document_id);
    assert_eq!(summary.title, document.title);
    assert_eq!(summary.revision, document.revision);
    assert_eq!(summary.updated_at, document.updated_at);
    assert_eq!(summary.preview_file, document.preview_file);
    assert_eq!(summary.preview_revision, document.preview_revision);
    assert!(decode_document(&serde_json::to_vec(&value).unwrap()).is_err());
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
