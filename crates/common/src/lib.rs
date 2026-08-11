pub mod command;
pub mod document;
pub mod element;
pub mod format;
pub mod geometry;
pub mod history;

pub use command::{AppliedCommand, ArrowEndpoint, CommandBatch, CommandError, DocumentCommand};
pub use document::{
  BackgroundKind, BackgroundMetadata, BoardDocument, CURRENT_SCHEMA_VERSION, CapturedDisplay,
  ContentFingerprint, DirtyBaseline, DocumentError, DocumentId, DocumentSnapshot, GlobalBoundsPx,
  MAX_ELEMENTS, MAX_STROKE_POINTS, Revision,
};
pub use element::{
  ArrowHead, ArrowPayload, ColorRgba, DerivedLabelPlacement, Element, ElementError, ElementId,
  ElementKind, ElementPayload, FONT_FAMILY, LabelPlacementPreference, LineCap, LineJoin,
  PRESET_BRUSH_HARDNESSES, PRESET_FONT_SIZES_PX, PRESET_STROKE_WIDTHS_PX, RectangleLabel,
  RectangleLabelLayout, RectanglePayload, SequenceMarkerPayload, StrokePayload, StrokePoint,
  StrokeStyle, StyleChange, TextAlign, TextLayout, TextPayload, TextStyle, layout_text,
  rectangle_label_layout,
};
pub use format::{
  FormatError, ResourceNameError, decode_document, decode_snapshot, encode_document,
  encode_snapshot, validate_managed_resource_names, validate_resource_name,
};
pub use geometry::{
  GeometryError, MAX_CANVAS_DIMENSION_PX, PointPx, RectPx, SizePx, minimum_geometry_extent,
  process_stroke_points,
};
pub use history::{
  CommandHistory, HistoryError, HistoryLimits, MAX_HISTORY_BYTES, MAX_HISTORY_ENTRIES,
};
