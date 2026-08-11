use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::geometry::{
  GeometryError, PointPx, RectPx, SizePx, minimum_geometry_extent, process_stroke_points,
};

pub const FONT_FAMILY: &str = "Noto Sans CJK SC Regular";
pub const PRESET_STROKE_WIDTHS_PX: [f32; 3] = [4.0, 8.0, 12.0];
pub const PRESET_FONT_SIZES_PX: [f32; 6] = [12.0, 16.0, 24.0, 36.0, 48.0, 64.0];
const BOUNDS_EPSILON_PX: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ElementId(pub Uuid);

impl ElementId {
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

impl Default for ElementId {
  fn default() -> Self {
    Self::new()
  }
}

impl fmt::Display for ElementId {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    self.0.fmt(formatter)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColorRgba {
  pub red: u8,
  pub green: u8,
  pub blue: u8,
  pub alpha: u8,
}

impl ColorRgba {
  pub const RED: Self = Self::opaque(0xFF, 0x3B, 0x30);
  pub const YELLOW: Self = Self::opaque(0xFF, 0xD6, 0x0A);
  pub const GREEN: Self = Self::opaque(0x30, 0xD1, 0x58);
  pub const BLUE: Self = Self::opaque(0x0A, 0x84, 0xFF);
  pub const WHITE: Self = Self::opaque(0xFF, 0xFF, 0xFF);
  pub const BLACK: Self = Self::opaque(0x00, 0x00, 0x00);

  pub const fn opaque(red: u8, green: u8, blue: u8) -> Self {
    Self { red, green, blue, alpha: u8::MAX }
  }

  pub fn is_mvp_color(self) -> bool {
    matches!(self, Self::RED | Self::YELLOW | Self::GREEN | Self::BLUE | Self::WHITE | Self::BLACK)
  }

  pub fn contrasting_text(self) -> Self {
    let luminance = (0.2126 * f32::from(self.red)
      + 0.7152 * f32::from(self.green)
      + 0.0722 * f32::from(self.blue))
      / 255.0;
    if luminance > 0.58 { Self::BLACK } else { Self::WHITE }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineCap {
  Round,
  Square,
  Butt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineJoin {
  Round,
  Miter,
  Bevel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrokeStyle {
  pub color_rgba: ColorRgba,
  pub width_px: f32,
  pub opacity: f32,
  pub line_cap: LineCap,
  pub line_join: LineJoin,
}

impl StrokeStyle {
  pub fn mvp(color_rgba: ColorRgba, width_px: f32) -> Result<Self, ElementError> {
    let style = Self {
      color_rgba,
      width_px,
      opacity: 1.0,
      line_cap: LineCap::Round,
      line_join: LineJoin::Round,
    };
    style.validate()?;
    Ok(style)
  }

  pub fn validate(&self) -> Result<(), ElementError> {
    validate_color(self.color_rgba)?;
    if !is_preset(self.width_px, &PRESET_STROKE_WIDTHS_PX) {
      return Err(ElementError::InvalidStrokeWidth(self.width_px));
    }
    if self.opacity != 1.0 {
      return Err(ElementError::OpacityMustBeOpaque);
    }
    Ok(())
  }
}

impl Default for StrokeStyle {
  fn default() -> Self {
    Self::mvp(ColorRgba::RED, 8.0).expect("default stroke style is valid")
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
  Left,
  Center,
  Right,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextStyle {
  pub color_rgba: ColorRgba,
  pub font_family: String,
  pub font_size_px: f32,
  pub line_height_px: f32,
  pub align: TextAlign,
}

impl TextStyle {
  pub fn mvp(color_rgba: ColorRgba, font_size_px: f32) -> Result<Self, ElementError> {
    let style = Self {
      color_rgba,
      font_family: FONT_FAMILY.to_owned(),
      font_size_px,
      line_height_px: font_size_px * 1.2,
      align: TextAlign::Left,
    };
    style.validate()?;
    Ok(style)
  }

  pub fn validate(&self) -> Result<(), ElementError> {
    validate_color(self.color_rgba)?;
    if self.font_family != FONT_FAMILY {
      return Err(ElementError::InvalidFontFamily);
    }
    if !is_preset(self.font_size_px, &PRESET_FONT_SIZES_PX) {
      return Err(ElementError::InvalidFontSize(self.font_size_px));
    }
    if !self.line_height_px.is_finite() || self.line_height_px < self.font_size_px {
      return Err(ElementError::InvalidLineHeight);
    }
    Ok(())
  }
}

impl Default for TextStyle {
  fn default() -> Self {
    Self::mvp(ColorRgba::RED, 24.0).expect("default text style is valid")
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrokePoint {
  pub x_px: f32,
  pub y_px: f32,
  pub pressure: f32,
}

impl StrokePoint {
  pub fn new(point: PointPx) -> Self {
    Self { x_px: point.x_px, y_px: point.y_px, pressure: 1.0 }
  }

  pub fn point(&self) -> PointPx {
    PointPx::new(self.x_px, self.y_px)
  }

  fn validate(&self) -> Result<(), ElementError> {
    if !self.point().is_finite() {
      return Err(ElementError::Geometry(GeometryError::NonFiniteCoordinate));
    }
    if self.pressure != 1.0 {
      return Err(ElementError::PressureMustBeOne);
    }
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrokePayload {
  pub points: Vec<StrokePoint>,
  pub stroke_style: StrokeStyle,
}

impl StrokePayload {
  pub fn from_raw_points(
    points: &[PointPx],
    stroke_style: StrokeStyle,
  ) -> Result<Self, ElementError> {
    stroke_style.validate()?;
    let points = process_stroke_points(points, stroke_style.width_px)?
      .into_iter()
      .map(StrokePoint::new)
      .collect();
    let payload = Self { points, stroke_style };
    payload.validate()?;
    Ok(payload)
  }

  fn validate(&self) -> Result<(), ElementError> {
    self.stroke_style.validate()?;
    if self.points.is_empty() {
      return Err(ElementError::Geometry(GeometryError::TooFewPoints));
    }
    for point in &self.points {
      point.validate()?;
    }
    if self.points.len() == 1 {
      return Ok(());
    }
    let path_length: f32 =
      self.points.windows(2).map(|points| points[0].point().distance_to(points[1].point())).sum();
    if path_length < minimum_geometry_extent(self.stroke_style.width_px)? {
      return Err(ElementError::GeometryBelowMinimum);
    }
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArrowHead {
  pub length_px: f32,
  pub width_px: f32,
  pub min_body_length_px: f32,
}

impl ArrowHead {
  pub fn for_stroke_width(width_px: f32) -> Result<Self, ElementError> {
    if !width_px.is_finite() || width_px <= 0.0 {
      return Err(ElementError::InvalidArrowHead);
    }
    Ok(Self {
      length_px: (width_px * 5.0).clamp(20.0, 60.0),
      width_px: (width_px * 4.0).clamp(16.0, 48.0),
      min_body_length_px: (width_px * 3.0).max(12.0),
    })
  }

  fn validate(&self) -> Result<(), ElementError> {
    if !self.length_px.is_finite()
      || !self.width_px.is_finite()
      || !self.min_body_length_px.is_finite()
      || self.length_px <= 0.0
      || self.width_px <= 0.0
      || self.min_body_length_px <= 0.0
    {
      return Err(ElementError::InvalidArrowHead);
    }
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArrowPayload {
  pub start_px: PointPx,
  pub end_px: PointPx,
  pub stroke_style: StrokeStyle,
  pub head: ArrowHead,
}

impl ArrowPayload {
  fn validate_for_bounds(&self) -> Result<(), ElementError> {
    self.stroke_style.validate()?;
    self.head.validate()?;
    if !self.start_px.is_finite() || !self.end_px.is_finite() {
      return Err(ElementError::Geometry(GeometryError::NonFiniteCoordinate));
    }
    Ok(())
  }

  fn validate(&self) -> Result<(), ElementError> {
    self.validate_for_bounds()?;
    if self.start_px.distance_to(self.end_px) < self.head.min_body_length_px {
      return Err(ElementError::GeometryBelowMinimum);
    }
    Ok(())
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelPlacementPreference {
  Above,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RectangleLabel {
  pub text: String,
  pub placement_preference: LabelPlacementPreference,
  pub max_width_px: f32,
  pub padding_px: f32,
  pub anchor_offset_px: f32,
  pub text_style: TextStyle,
}

impl RectangleLabel {
  fn validate(&self) -> Result<(), ElementError> {
    if self.text.trim().is_empty() {
      return Err(ElementError::EmptyText);
    }
    if !self.max_width_px.is_finite()
      || !self.padding_px.is_finite()
      || !self.anchor_offset_px.is_finite()
      || self.max_width_px <= 0.0
      || self.padding_px < 0.0
      || self.anchor_offset_px < 0.0
    {
      return Err(ElementError::InvalidLabelLayout);
    }
    self.text_style.validate()
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RectanglePayload {
  pub start_px: PointPx,
  pub end_px: PointPx,
  pub stroke_style: StrokeStyle,
  pub fill_rgba: Option<ColorRgba>,
  pub label: RectangleLabel,
}

impl RectanglePayload {
  fn validate_for_layout(&self) -> Result<(), ElementError> {
    self.stroke_style.validate()?;
    self.label.validate()?;
    if self.label.text_style.color_rgba != self.stroke_style.color_rgba.contrasting_text() {
      return Err(ElementError::InvalidContrastColor);
    }
    if self.fill_rgba.is_some() {
      return Err(ElementError::RectangleFillNotSupported);
    }
    if !self.start_px.is_finite() || !self.end_px.is_finite() {
      return Err(ElementError::Geometry(GeometryError::NonFiniteCoordinate));
    }
    Ok(())
  }

  fn validate(&self) -> Result<(), ElementError> {
    self.validate_for_layout()?;
    let body = RectPx::from_points(self.start_px, self.end_px);
    let minimum = minimum_geometry_extent(self.stroke_style.width_px)?;
    if body.width() < minimum || body.height() < minimum {
      return Err(ElementError::GeometryBelowMinimum);
    }
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextPayload {
  pub anchor_px: PointPx,
  pub text: String,
  pub box_width_px: f32,
  pub text_style: TextStyle,
}

impl TextPayload {
  fn validate(&self) -> Result<(), ElementError> {
    if !self.anchor_px.is_finite() {
      return Err(ElementError::Geometry(GeometryError::NonFiniteCoordinate));
    }
    if self.text.trim().is_empty() {
      return Err(ElementError::EmptyText);
    }
    if !self.box_width_px.is_finite() || self.box_width_px <= 0.0 {
      return Err(ElementError::InvalidTextBoxWidth);
    }
    self.text_style.validate()
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequenceMarkerPayload {
  pub center_px: PointPx,
  pub number: u64,
  pub radius_px: f32,
  pub pill_width_px: f32,
  pub fill_rgba: ColorRgba,
  pub stroke_style: StrokeStyle,
  pub text_style: TextStyle,
}

impl SequenceMarkerPayload {
  fn validate(&self) -> Result<(), ElementError> {
    if !self.center_px.is_finite() {
      return Err(ElementError::Geometry(GeometryError::NonFiniteCoordinate));
    }
    if self.number == 0 {
      return Err(ElementError::InvalidSequenceNumber);
    }
    if !self.radius_px.is_finite()
      || !self.pill_width_px.is_finite()
      || self.radius_px <= 0.0
      || self.pill_width_px < self.radius_px * 2.0
    {
      return Err(ElementError::InvalidSequenceGeometry);
    }
    validate_color(self.fill_rgba)?;
    self.stroke_style.validate()?;
    self.text_style.validate()?;
    if self.text_style.color_rgba != self.fill_rgba.contrasting_text() {
      return Err(ElementError::InvalidContrastColor);
    }
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum ElementPayload {
  Stroke(StrokePayload),
  Arrow(ArrowPayload),
  Rectangle(RectanglePayload),
  Text(TextPayload),
  SequenceMarker(SequenceMarkerPayload),
}

impl ElementPayload {
  pub fn kind(&self) -> ElementKind {
    match self {
      Self::Stroke(_) => ElementKind::Stroke,
      Self::Arrow(_) => ElementKind::Arrow,
      Self::Rectangle(_) => ElementKind::Rectangle,
      Self::Text(_) => ElementKind::Text,
      Self::SequenceMarker(_) => ElementKind::SequenceMarker,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementKind {
  Stroke,
  Arrow,
  Rectangle,
  Text,
  SequenceMarker,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Element {
  pub element_id: ElementId,
  pub z_index: i64,
  pub bounds_px: RectPx,
  #[serde(flatten)]
  pub payload: ElementPayload,
}

impl Element {
  pub fn new(
    element_id: ElementId,
    z_index: i64,
    payload: ElementPayload,
    canvas_size_px: SizePx,
  ) -> Result<Self, ElementError> {
    let mut element = Self {
      element_id,
      z_index,
      bounds_px: RectPx::from_min_max(PointPx::ZERO, PointPx::ZERO),
      payload,
    };
    element.validate_payload()?;
    element.refresh_bounds(canvas_size_px)?;
    if element.requires_full_canvas_containment() {
      element.constrain_to_canvas(canvas_size_px, true)?;
    }
    element.validate(canvas_size_px)?;
    Ok(element)
  }

  pub fn kind(&self) -> ElementKind {
    self.payload.kind()
  }

  pub fn persistent_point_count(&self) -> usize {
    match &self.payload {
      ElementPayload::Stroke(payload) => payload.points.len(),
      _ => 0,
    }
  }

  pub fn validate(&self, canvas_size_px: SizePx) -> Result<(), ElementError> {
    canvas_size_px.validate()?;
    if self.z_index < 0 {
      return Err(ElementError::NegativeZIndex);
    }
    self.bounds_px.validate()?;
    self.validate_payload()?;
    if let ElementPayload::Text(payload) = &self.payload
      && payload.box_width_px > canvas_size_px.width_px as f32
    {
      return Err(ElementError::TextBoxWiderThanCanvas);
    }
    let expected = self.derived_bounds(canvas_size_px)?;
    if !rect_approximately_equal(self.bounds_px, expected) {
      return Err(ElementError::StaleBounds);
    }
    if self.requires_full_canvas_containment()
      && !canvas_size_px.bounds().contains_rect(self.bounds_px)
    {
      return Err(ElementError::OutsideCanvas);
    }
    if matches!(self.payload, ElementPayload::Text(_))
      && !has_positive_intersection(canvas_size_px.bounds(), self.bounds_px)
    {
      return Err(ElementError::OutsideCanvas);
    }
    Ok(())
  }

  pub fn refresh_bounds(&mut self, canvas_size_px: SizePx) -> Result<(), ElementError> {
    self.bounds_px = self.derived_bounds(canvas_size_px)?;
    Ok(())
  }

  pub fn move_by(&mut self, delta: PointPx, canvas_size_px: SizePx) -> Result<(), ElementError> {
    if !delta.is_finite() {
      return Err(ElementError::Geometry(GeometryError::NonFiniteCoordinate));
    }
    let mut staged = self.clone();
    staged.translate_unchecked(delta);
    staged.refresh_bounds(canvas_size_px)?;
    if staged.requires_full_canvas_containment() {
      staged.constrain_to_canvas_in_place(canvas_size_px, true)?;
    }
    staged.validate(canvas_size_px)?;
    *self = staged;
    Ok(())
  }

  pub fn placed_copy(
    &self,
    element_id: ElementId,
    center_px: PointPx,
    canvas_size_px: SizePx,
  ) -> Result<Self, ElementError> {
    if !center_px.is_finite() {
      return Err(ElementError::Geometry(GeometryError::NonFiniteCoordinate));
    }
    let mut copy = self.clone();
    copy.element_id = element_id;
    copy.move_by(center_px - copy.bounds_px.center(), canvas_size_px)?;
    copy.constrain_to_canvas(canvas_size_px, true)?;
    copy.validate(canvas_size_px)?;
    Ok(copy)
  }

  pub fn set_style(
    &mut self,
    change: &StyleChange,
    canvas_size_px: SizePx,
  ) -> Result<(), ElementError> {
    change.validate()?;
    let mut staged = self.clone();
    match &mut staged.payload {
      ElementPayload::Stroke(payload) => {
        apply_stroke_change(&mut payload.stroke_style, change);
      }
      ElementPayload::Arrow(payload) => {
        apply_stroke_change(&mut payload.stroke_style, change);
        if change.width_px.is_some() {
          payload.head = ArrowHead::for_stroke_width(payload.stroke_style.width_px)?;
        }
      }
      ElementPayload::Rectangle(payload) => {
        apply_stroke_change(&mut payload.stroke_style, change);
        if let Some(font_size_px) = change.font_size_px {
          payload.label.text_style.font_size_px = font_size_px;
          payload.label.text_style.line_height_px = font_size_px * 1.2;
        }
        payload.label.text_style.color_rgba = payload.stroke_style.color_rgba.contrasting_text();
      }
      ElementPayload::Text(payload) => {
        if let Some(color_rgba) = change.color_rgba {
          payload.text_style.color_rgba = color_rgba;
        }
        if let Some(font_size_px) = change.font_size_px {
          payload.text_style.font_size_px = font_size_px;
          payload.text_style.line_height_px = font_size_px * 1.2;
        }
      }
      ElementPayload::SequenceMarker(payload) => {
        if let Some(color_rgba) = change.color_rgba {
          payload.fill_rgba = color_rgba;
          payload.stroke_style.color_rgba = color_rgba;
          payload.text_style.color_rgba = color_rgba.contrasting_text();
        }
        apply_stroke_change(&mut payload.stroke_style, change);
        if let Some(font_size_px) = change.font_size_px {
          payload.text_style.font_size_px = font_size_px;
          payload.text_style.line_height_px = font_size_px * 1.2;
        }
      }
    }
    staged.validate_payload()?;
    staged.refresh_bounds(canvas_size_px)?;
    if staged.requires_full_canvas_containment() {
      staged.constrain_to_canvas_in_place(canvas_size_px, true)?;
    }
    staged.validate(canvas_size_px)?;
    *self = staged;
    Ok(())
  }

  pub fn constrain_to_canvas(
    &mut self,
    canvas_size_px: SizePx,
    force: bool,
  ) -> Result<(), ElementError> {
    let mut staged = self.clone();
    staged.constrain_to_canvas_in_place(canvas_size_px, force)?;
    staged.validate(canvas_size_px)?;
    *self = staged;
    Ok(())
  }

  fn constrain_to_canvas_in_place(
    &mut self,
    canvas_size_px: SizePx,
    force: bool,
  ) -> Result<(), ElementError> {
    if !force && !self.requires_full_canvas_containment() {
      return Ok(());
    }
    for _ in 0..2 {
      let correction = self.bounds_px.translation_to_fit(canvas_size_px)?;
      if correction == PointPx::ZERO {
        break;
      }
      self.translate_unchecked(correction);
      self.refresh_bounds(canvas_size_px)?;
    }
    if !canvas_size_px.bounds().contains_rect(self.bounds_px) {
      return Err(ElementError::OutsideCanvas);
    }
    Ok(())
  }

  pub fn estimated_bytes(&self) -> usize {
    let payload_bytes = match &self.payload {
      ElementPayload::Stroke(payload) => {
        payload.points.len().saturating_mul(std::mem::size_of::<StrokePoint>())
      }
      ElementPayload::Rectangle(payload) => {
        payload.label.text.len().saturating_add(payload.label.text_style.font_family.len())
      }
      ElementPayload::Text(payload) => {
        payload.text.len().saturating_add(payload.text_style.font_family.len())
      }
      ElementPayload::SequenceMarker(payload) => payload.text_style.font_family.len(),
      ElementPayload::Arrow(_) => 0,
    };
    std::mem::size_of::<Self>().saturating_add(payload_bytes)
  }

  fn validate_payload(&self) -> Result<(), ElementError> {
    match &self.payload {
      ElementPayload::Stroke(payload) => payload.validate(),
      ElementPayload::Arrow(payload) => payload.validate(),
      ElementPayload::Rectangle(payload) => payload.validate(),
      ElementPayload::Text(payload) => payload.validate(),
      ElementPayload::SequenceMarker(payload) => payload.validate(),
    }
  }

  fn requires_full_canvas_containment(&self) -> bool {
    !matches!(self.payload, ElementPayload::Text(_))
  }

  fn derived_bounds(&self, canvas_size_px: SizePx) -> Result<RectPx, ElementError> {
    let bounds = match &self.payload {
      ElementPayload::Stroke(payload) => {
        let points: Vec<_> = payload.points.iter().map(StrokePoint::point).collect();
        bounds_for_points(&points)?.expanded(payload.stroke_style.width_px / 2.0)
      }
      ElementPayload::Arrow(payload) => arrow_bounds(payload)?,
      ElementPayload::Rectangle(payload) => {
        let body = RectPx::from_points(payload.start_px, payload.end_px)
          .expanded(payload.stroke_style.width_px / 2.0);
        body.union(rectangle_label_layout(payload, canvas_size_px)?.bounds_px)
      }
      ElementPayload::Text(payload) => {
        let layout = layout_text(&payload.text, &payload.text_style, payload.box_width_px)?;
        RectPx::from_min_max(
          payload.anchor_px,
          PointPx::new(
            payload.anchor_px.x_px + layout.width_px,
            payload.anchor_px.y_px + layout.height_px,
          ),
        )
      }
      ElementPayload::SequenceMarker(payload) => RectPx::from_center_size(
        payload.center_px,
        payload.pill_width_px + payload.stroke_style.width_px,
        payload.radius_px * 2.0 + payload.stroke_style.width_px,
      ),
    };
    bounds.validate()?;
    Ok(bounds)
  }

  fn translate_unchecked(&mut self, delta: PointPx) {
    match &mut self.payload {
      ElementPayload::Stroke(payload) => {
        for point in &mut payload.points {
          point.x_px += delta.x_px;
          point.y_px += delta.y_px;
        }
      }
      ElementPayload::Arrow(payload) => {
        payload.start_px = payload.start_px + delta;
        payload.end_px = payload.end_px + delta;
      }
      ElementPayload::Rectangle(payload) => {
        payload.start_px = payload.start_px + delta;
        payload.end_px = payload.end_px + delta;
      }
      ElementPayload::Text(payload) => payload.anchor_px = payload.anchor_px + delta,
      ElementPayload::SequenceMarker(payload) => {
        payload.center_px = payload.center_px + delta;
      }
    }
    self.bounds_px = self.bounds_px.translated(delta);
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedLabelPlacement {
  Above,
  Below,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectangleLabelLayout {
  pub bounds_px: RectPx,
  pub placement: DerivedLabelPlacement,
  pub text_layout: TextLayout,
  pub text_wrap_width_px: f32,
}

pub fn rectangle_label_layout(
  rectangle: &RectanglePayload,
  canvas_size_px: SizePx,
) -> Result<RectangleLabelLayout, ElementError> {
  canvas_size_px.validate()?;
  rectangle.validate_for_layout()?;
  let canvas = canvas_size_px.bounds();
  let body = RectPx::from_points(rectangle.start_px, rectangle.end_px);
  let maximum_width = rectangle
    .label
    .max_width_px
    .min(body.width() * 1.5)
    .min(canvas.width())
    .max(rectangle.label.padding_px * 2.0 + 1.0);
  let text_wrap_width_px = (maximum_width - rectangle.label.padding_px * 2.0).max(1.0);
  let text_layout =
    layout_text(&rectangle.label.text, &rectangle.label.text_style, text_wrap_width_px)?;
  let width_px =
    (text_layout.width_px + rectangle.label.padding_px * 2.0).min(maximum_width).max(1.0);
  let height_px = text_layout.height_px + rectangle.label.padding_px * 2.0;
  let x_px = (body.min.x_px + rectangle.label.anchor_offset_px)
    .clamp(0.0, (canvas.width() - width_px).max(0.0));
  let margin = rectangle.label.anchor_offset_px;
  let (placement, candidate_y) = if body.min.y_px - height_px - margin >= 0.0 {
    (DerivedLabelPlacement::Above, body.min.y_px - height_px - margin)
  } else {
    (DerivedLabelPlacement::Below, body.max.y_px + margin)
  };
  let y_px = candidate_y.clamp(0.0, (canvas.height() - height_px).max(0.0));
  Ok(RectangleLabelLayout {
    bounds_px: RectPx::from_min_max(
      PointPx::new(x_px, y_px),
      PointPx::new(x_px + width_px, y_px + height_px),
    ),
    placement,
    text_layout,
    text_wrap_width_px,
  })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextLayout {
  pub width_px: f32,
  pub height_px: f32,
  pub line_count: usize,
}

pub fn layout_text(
  text: &str,
  style: &TextStyle,
  maximum_width_px: f32,
) -> Result<TextLayout, ElementError> {
  style.validate()?;
  if !maximum_width_px.is_finite() || maximum_width_px <= 0.0 {
    return Err(ElementError::InvalidTextBoxWidth);
  }

  let mut line_width = 0.0f32;
  let mut maximum_line_width = 0.0f32;
  let mut line_count = 1usize;
  for character in text.chars() {
    if character == '\n' {
      maximum_line_width = maximum_line_width.max(line_width);
      line_width = 0.0;
      line_count += 1;
      continue;
    }
    let character_width = character_width(character, style.font_size_px);
    if line_width > 0.0 && line_width + character_width > maximum_width_px {
      maximum_line_width = maximum_line_width.max(line_width);
      line_width = character_width.min(maximum_width_px);
      line_count += 1;
    } else {
      line_width = (line_width + character_width).min(maximum_width_px);
    }
  }
  maximum_line_width = maximum_line_width.max(line_width);
  Ok(TextLayout {
    width_px: maximum_line_width.max(1.0),
    height_px: style.line_height_px * line_count as f32,
    line_count,
  })
}

fn character_width(character: char, font_size_px: f32) -> f32 {
  if character == '\u{200b}' {
    0.0
  } else if character.is_ascii_whitespace() {
    font_size_px * 0.33
  } else if character.is_ascii() {
    font_size_px * 0.6
  } else {
    font_size_px
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StyleChange {
  pub color_rgba: Option<ColorRgba>,
  pub width_px: Option<f32>,
  pub font_size_px: Option<f32>,
}

impl StyleChange {
  pub fn validate(&self) -> Result<(), ElementError> {
    if self.color_rgba.is_none() && self.width_px.is_none() && self.font_size_px.is_none() {
      return Err(ElementError::EmptyStyleChange);
    }
    if let Some(color_rgba) = self.color_rgba {
      validate_color(color_rgba)?;
    }
    if let Some(width_px) = self.width_px
      && !is_preset(width_px, &PRESET_STROKE_WIDTHS_PX)
    {
      return Err(ElementError::InvalidStrokeWidth(width_px));
    }
    if let Some(font_size_px) = self.font_size_px
      && !is_preset(font_size_px, &PRESET_FONT_SIZES_PX)
    {
      return Err(ElementError::InvalidFontSize(font_size_px));
    }
    Ok(())
  }
}

fn apply_stroke_change(style: &mut StrokeStyle, change: &StyleChange) {
  if let Some(color_rgba) = change.color_rgba {
    style.color_rgba = color_rgba;
  }
  if let Some(width_px) = change.width_px {
    style.width_px = width_px;
  }
}

fn arrow_bounds(payload: &ArrowPayload) -> Result<RectPx, ElementError> {
  payload.validate_for_bounds()?;
  let x = payload.end_px.x_px - payload.start_px.x_px;
  let y = payload.end_px.y_px - payload.start_px.y_px;
  let length = x.hypot(y).max(f32::EPSILON);
  let unit = PointPx::new(x / length, y / length);
  let perpendicular = PointPx::new(-unit.y_px, unit.x_px);
  let base = PointPx::new(
    payload.end_px.x_px - unit.x_px * payload.head.length_px,
    payload.end_px.y_px - unit.y_px * payload.head.length_px,
  );
  let half_head_width = payload.head.width_px / 2.0;
  let corner_a = PointPx::new(
    base.x_px + perpendicular.x_px * half_head_width,
    base.y_px + perpendicular.y_px * half_head_width,
  );
  let corner_b = PointPx::new(
    base.x_px - perpendicular.x_px * half_head_width,
    base.y_px - perpendicular.y_px * half_head_width,
  );
  Ok(
    bounds_for_points(&[payload.start_px, payload.end_px, corner_a, corner_b])?
      .expanded(payload.stroke_style.width_px / 2.0),
  )
}

fn bounds_for_points(points: &[PointPx]) -> Result<RectPx, ElementError> {
  let Some(first) = points.first().copied() else {
    return Err(ElementError::Geometry(GeometryError::TooFewPoints));
  };
  if points.iter().any(|point| !point.is_finite()) {
    return Err(ElementError::Geometry(GeometryError::NonFiniteCoordinate));
  }
  let mut minimum = first;
  let mut maximum = first;
  for point in &points[1..] {
    minimum.x_px = minimum.x_px.min(point.x_px);
    minimum.y_px = minimum.y_px.min(point.y_px);
    maximum.x_px = maximum.x_px.max(point.x_px);
    maximum.y_px = maximum.y_px.max(point.y_px);
  }
  Ok(RectPx::from_min_max(minimum, maximum))
}

fn validate_color(color_rgba: ColorRgba) -> Result<(), ElementError> {
  if color_rgba.alpha != u8::MAX {
    return Err(ElementError::OpacityMustBeOpaque);
  }
  if !color_rgba.is_mvp_color() {
    return Err(ElementError::UnsupportedColor);
  }
  Ok(())
}

fn is_preset(value: f32, presets: &[f32]) -> bool {
  value.is_finite() && presets.contains(&value)
}

fn rect_approximately_equal(left: RectPx, right: RectPx) -> bool {
  (left.min.x_px - right.min.x_px).abs() <= BOUNDS_EPSILON_PX
    && (left.min.y_px - right.min.y_px).abs() <= BOUNDS_EPSILON_PX
    && (left.max.x_px - right.max.x_px).abs() <= BOUNDS_EPSILON_PX
    && (left.max.y_px - right.max.y_px).abs() <= BOUNDS_EPSILON_PX
}

fn has_positive_intersection(left: RectPx, right: RectPx) -> bool {
  left.min.x_px < right.max.x_px
    && left.max.x_px > right.min.x_px
    && left.min.y_px < right.max.y_px
    && left.max.y_px > right.min.y_px
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum ElementError {
  #[error(transparent)]
  Geometry(#[from] GeometryError),
  #[error("z-index must be non-negative")]
  NegativeZIndex,
  #[error("element bounds do not match its payload")]
  StaleBounds,
  #[error("element must remain fully inside the canvas")]
  OutsideCanvas,
  #[error("geometry is below the line-width-derived minimum")]
  GeometryBelowMinimum,
  #[error("unsupported MVP color")]
  UnsupportedColor,
  #[error("opacity must be 1.0")]
  OpacityMustBeOpaque,
  #[error("stroke pressure must be 1.0")]
  PressureMustBeOne,
  #[error("unsupported stroke width {0}")]
  InvalidStrokeWidth(f32),
  #[error("unsupported font size {0}")]
  InvalidFontSize(f32),
  #[error("font family must be the bundled Noto Sans CJK SC Regular")]
  InvalidFontFamily,
  #[error("line height must be finite and at least the font size")]
  InvalidLineHeight,
  #[error("text must not be blank")]
  EmptyText,
  #[error("text box width must be finite and positive")]
  InvalidTextBoxWidth,
  #[error("text box width must not exceed the canvas width")]
  TextBoxWiderThanCanvas,
  #[error("rectangle labels require positive finite layout dimensions")]
  InvalidLabelLayout,
  #[error("rectangle fill is not supported in the MVP")]
  RectangleFillNotSupported,
  #[error("arrow head dimensions must be finite and positive")]
  InvalidArrowHead,
  #[error("sequence number must be at least one")]
  InvalidSequenceNumber,
  #[error("sequence marker geometry is invalid")]
  InvalidSequenceGeometry,
  #[error("automatic foreground contrast color is invalid")]
  InvalidContrastColor,
  #[error("style change does not contain any values")]
  EmptyStyleChange,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_single_point_stroke_is_a_valid_dot() {
    let canvas = SizePx::new(200, 120);
    let point = PointPx::new(80.0, 60.0);
    let payload = StrokePayload::from_raw_points(&[point], StrokeStyle::default()).unwrap();
    assert_eq!(payload.points.iter().map(StrokePoint::point).collect::<Vec<_>>(), vec![point]);

    let element =
      Element::new(ElementId::new(), 0, ElementPayload::Stroke(payload), canvas).unwrap();
    assert_eq!(element.bounds_px.center(), point);
    assert_eq!(element.bounds_px.width(), 8.0);
    assert_eq!(element.bounds_px.height(), 8.0);
  }

  fn text_style(color: ColorRgba) -> TextStyle {
    TextStyle::mvp(color, 24.0).unwrap()
  }

  fn rectangle(start: PointPx, end: PointPx) -> RectanglePayload {
    RectanglePayload {
      start_px: start,
      end_px: end,
      stroke_style: StrokeStyle::default(),
      fill_rgba: None,
      label: RectangleLabel {
        text: "标题".to_owned(),
        placement_preference: LabelPlacementPreference::Above,
        max_width_px: 240.0,
        padding_px: 4.0,
        anchor_offset_px: 4.0,
        text_style: text_style(ColorRgba::RED.contrasting_text()),
      },
    }
  }

  #[test]
  fn rectangle_label_moves_below_at_top_edge() {
    let rectangle = rectangle(PointPx::new(20.0, 5.0), PointPx::new(120.0, 80.0));
    let layout = rectangle_label_layout(&rectangle, SizePx::new(200, 120)).unwrap();
    assert_eq!(layout.placement, DerivedLabelPlacement::Below);
    assert!(SizePx::new(200, 120).bounds().contains_rect(layout.bounds_px));
  }

  #[test]
  fn rectangle_label_exposes_pre_shrink_text_wrap_width() {
    let rectangle = rectangle(PointPx::new(20.0, 40.0), PointPx::new(120.0, 100.0));
    let layout = rectangle_label_layout(&rectangle, SizePx::new(200, 140)).unwrap();

    // The body-derived outer cap is 150 px; the text wraps inside its 4 px padding.
    assert_eq!(layout.text_wrap_width_px, 142.0);
    assert!(
      layout.text_wrap_width_px > layout.bounds_px.width() - rectangle.label.padding_px * 2.0
    );
  }

  #[test]
  fn long_cjk_text_wraps_deterministically() {
    let style = text_style(ColorRgba::BLACK);
    let layout = layout_text("这是一个很长的中文标签", &style, 72.0).unwrap();
    assert!(layout.line_count > 1);
    assert!(layout.width_px <= 72.0);
  }

  #[test]
  fn placed_copy_gets_new_id_and_stays_inside_canvas() {
    let canvas = SizePx::new(200, 120);
    let original = Element::new(
      ElementId::new(),
      0,
      ElementPayload::Rectangle(rectangle(PointPx::new(30.0, 40.0), PointPx::new(100.0, 100.0))),
      canvas,
    )
    .unwrap();
    let new_id = ElementId::new();
    let copy = original.placed_copy(new_id, PointPx::new(199.0, 119.0), canvas).unwrap();
    assert_eq!(copy.element_id, new_id);
    assert!(canvas.bounds().contains_rect(copy.bounds_px));
  }

  #[test]
  fn stale_persisted_bounds_are_rejected() {
    let canvas = SizePx::new(200, 120);
    let mut element = Element::new(
      ElementId::new(),
      0,
      ElementPayload::Rectangle(rectangle(PointPx::new(30.0, 40.0), PointPx::new(100.0, 100.0))),
      canvas,
    )
    .unwrap();
    element.bounds_px.min.x_px += 10.0;
    assert_eq!(element.validate(canvas), Err(ElementError::StaleBounds));
  }

  #[test]
  fn arrow_bounds_include_rotated_head() {
    let canvas = SizePx::new(300, 300);
    let style = StrokeStyle::default();
    let arrow = Element::new(
      ElementId::new(),
      0,
      ElementPayload::Arrow(ArrowPayload {
        start_px: PointPx::new(40.0, 40.0),
        end_px: PointPx::new(200.0, 200.0),
        head: ArrowHead::for_stroke_width(style.width_px).unwrap(),
        stroke_style: style,
      }),
      canvas,
    )
    .unwrap();
    assert!(arrow.bounds_px.width() > 160.0);
    assert!(arrow.bounds_px.height() > 160.0);
  }

  #[test]
  fn arrow_head_uses_reference_proportions_for_every_stroke_width() {
    for (width_px, expected_length, expected_width) in
      [(4.0, 20.0, 16.0), (8.0, 40.0, 32.0), (12.0, 60.0, 48.0)]
    {
      let head = ArrowHead::for_stroke_width(width_px).unwrap();
      assert_eq!(head.length_px, expected_length);
      assert_eq!(head.width_px, expected_width);
    }
  }

  #[test]
  fn undersized_rectangles_have_finite_transient_layout_but_fail_validation() {
    let canvas = SizePx::new(300, 200);
    for end_px in [PointPx::new(41.0, 61.0), PointPx::new(40.0, 60.0)] {
      let payload = rectangle(PointPx::new(40.0, 60.0), end_px);
      let layout = rectangle_label_layout(&payload, canvas).unwrap();
      assert_eq!(layout.bounds_px.validate(), Ok(()));
      assert!(layout.text_wrap_width_px.is_finite());
      assert!(layout.text_layout.width_px.is_finite());
      assert!(layout.text_layout.height_px.is_finite());

      let mut transient = Element {
        element_id: ElementId::new(),
        z_index: 0,
        bounds_px: RectPx::from_min_max(PointPx::ZERO, PointPx::ZERO),
        payload: ElementPayload::Rectangle(payload.clone()),
      };
      transient.refresh_bounds(canvas).unwrap();
      assert_eq!(transient.bounds_px.validate(), Ok(()));
      assert_eq!(transient.validate(canvas), Err(ElementError::GeometryBelowMinimum));
      assert_eq!(
        Element::new(ElementId::new(), 0, ElementPayload::Rectangle(payload), canvas),
        Err(ElementError::GeometryBelowMinimum)
      );
    }
  }

  #[test]
  fn undersized_arrows_have_finite_transient_bounds_but_fail_validation() {
    let canvas = SizePx::new(300, 200);
    let style = StrokeStyle::default();
    let head = ArrowHead::for_stroke_width(style.width_px).unwrap();
    for end_px in [PointPx::new(41.0, 60.0), PointPx::new(40.0, 60.0)] {
      let payload = ArrowPayload {
        start_px: PointPx::new(40.0, 60.0),
        end_px,
        stroke_style: style.clone(),
        head: head.clone(),
      };
      let mut transient = Element {
        element_id: ElementId::new(),
        z_index: 0,
        bounds_px: RectPx::from_min_max(PointPx::ZERO, PointPx::ZERO),
        payload: ElementPayload::Arrow(payload.clone()),
      };
      transient.refresh_bounds(canvas).unwrap();
      assert_eq!(transient.bounds_px.validate(), Ok(()));
      assert_eq!(transient.validate(canvas), Err(ElementError::GeometryBelowMinimum));
      assert_eq!(
        Element::new(ElementId::new(), 0, ElementPayload::Arrow(payload), canvas),
        Err(ElementError::GeometryBelowMinimum)
      );
    }
  }

  #[test]
  fn text_may_be_partially_but_not_fully_outside_canvas() {
    let canvas = SizePx::new(200, 120);
    let partial = Element::new(
      ElementId::new(),
      0,
      ElementPayload::Text(TextPayload {
        anchor_px: PointPx::new(190.0, 100.0),
        text: "部分越界".to_owned(),
        box_width_px: 100.0,
        text_style: TextStyle::default(),
      }),
      canvas,
    );
    assert!(partial.is_ok());

    let outside = Element::new(
      ElementId::new(),
      0,
      ElementPayload::Text(TextPayload {
        anchor_px: PointPx::new(201.0, 121.0),
        text: "完全越界".to_owned(),
        box_width_px: 100.0,
        text_style: TextStyle::default(),
      }),
      canvas,
    );
    assert_eq!(outside, Err(ElementError::OutsideCanvas));
  }

  #[test]
  fn failed_public_element_mutations_are_transactional() {
    let canvas = SizePx::new(200, 120);
    let mut text = Element::new(
      ElementId::new(),
      0,
      ElementPayload::Text(TextPayload {
        anchor_px: PointPx::new(20.0, 20.0),
        text: "文字".to_owned(),
        box_width_px: 100.0,
        text_style: TextStyle::default(),
      }),
      canvas,
    )
    .unwrap();
    let before = text.clone();
    assert_eq!(text.move_by(PointPx::new(500.0, 500.0), canvas), Err(ElementError::OutsideCanvas));
    assert_eq!(text, before);

    let style = StrokeStyle::mvp(ColorRgba::RED, 4.0).unwrap();
    let mut arrow = Element::new(
      ElementId::new(),
      0,
      ElementPayload::Arrow(ArrowPayload {
        start_px: PointPx::new(50.0, 50.0),
        end_px: PointPx::new(80.0, 50.0),
        head: ArrowHead::for_stroke_width(style.width_px).unwrap(),
        stroke_style: style,
      }),
      canvas,
    )
    .unwrap();
    let before = arrow.clone();
    let result = arrow.set_style(
      &StyleChange { color_rgba: None, width_px: Some(12.0), font_size_px: None },
      canvas,
    );
    assert_eq!(result, Err(ElementError::GeometryBelowMinimum));
    assert_eq!(arrow, before);
  }
}
