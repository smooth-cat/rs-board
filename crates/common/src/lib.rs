pub mod command;
pub mod document;
pub mod element;
pub mod format;
pub mod geometry;
pub mod history;
pub mod rectangle_label_reflow;

pub use command::{AppliedCommand, ArrowEndpoint, CommandBatch, CommandError, DocumentCommand};
pub use document::{
  BackgroundKind, BackgroundMetadata, BoardDocument, CURRENT_SCHEMA_VERSION, CapturedDisplay,
  ContentFingerprint, DirtyBaseline, DocumentError, DocumentId, DocumentSnapshot, GlobalBoundsPx,
  MAX_ELEMENTS, MAX_STROKE_POINTS, Revision,
};
pub use element::{
  ArrowHead, ArrowLabelLayout, ArrowPayload, ColorRgba, Element, ElementError, ElementId,
  ElementKind, ElementLabel, ElementPayload, FONT_FAMILY, LineCap, LineJoin,
  PRESET_BRUSH_HARDNESSES, PRESET_FONT_SIZES_PX, PRESET_STROKE_WIDTHS_PX, RectangleLabelAnchor,
  RectangleLabelEdge, RectangleLabelLayout, RectangleLabelSide, RectanglePayload,
  SequenceMarkerPayload, StrokePayload, StrokePoint, StrokeStyle, StyleChange, TextAlign,
  TextLayout, TextPayload, TextStyle, arrow_label_available_width, arrow_label_layout,
  arrow_minimum_length_for_label, choose_rectangle_label_anchor, layout_text,
  rectangle_label_layout, rectangle_label_layout_at_anchor, snap_rectangle_label_layout,
  wrap_arrow_label_text_lines, wrap_text_lines,
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
pub use rectangle_label_reflow::{
  RectangleLabelScene, RectangleLabelSceneItem, RectangleLabelSolution,
  solve_rectangle_label_reflow,
};
