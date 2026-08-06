use std::sync::OnceLock;

use ab_glyph::{FontArc, PxScale};
use common::{
  ArrowPayload, BoardDocument, ColorRgba, DocumentSnapshot, Element, ElementPayload, PointPx,
  RectanglePayload, SizePx, TextAlign, TextStyle, rectangle_label_layout,
};
use eframe::egui::{self, Align2, Color32, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind};
use image::{Rgba, RgbaImage, imageops::FilterType};
use imageproc::drawing::{draw_text_mut, text_size};

use crate::editor::CanvasTransform;

const BUNDLED_CJK_FONT: &[u8] = include_bytes!("../assets/NotoSansSC-Regular.otf");

/// Read-only document surface shared by live documents and frozen snapshots.
pub trait RenderDocument {
  fn canvas_size_px(&self) -> SizePx;
  fn elements(&self) -> &[Element];
}

impl RenderDocument for BoardDocument {
  fn canvas_size_px(&self) -> SizePx {
    self.canvas_size_px
  }

  fn elements(&self) -> &[Element] {
    &self.elements
  }
}

impl RenderDocument for DocumentSnapshot {
  fn canvas_size_px(&self) -> SizePx {
    self.canvas_size_px
  }

  fn elements(&self) -> &[Element] {
    &self.elements
  }
}

/// Renders only persistent document content. Editor chrome and selection state are never included.
pub fn render_document_to_image(
  document: &(impl RenderDocument + ?Sized),
  background_rgba: &RgbaImage,
) -> RgbaImage {
  let size = document.canvas_size_px();
  let mut output = if background_rgba.dimensions() == (size.width_px, size.height_px) {
    background_rgba.clone()
  } else {
    image::imageops::resize(background_rgba, size.width_px, size.height_px, FilterType::Triangle)
  };

  for element in document.elements() {
    raster_element(&mut output, element, size);
  }
  output
}

/// Paints persistent document content into an egui canvas using document-pixel geometry.
pub fn paint_document(
  painter: &Painter,
  transform: &CanvasTransform,
  document: &(impl RenderDocument + ?Sized),
) {
  let painter = painter.with_clip_rect(transform.canvas_rect());
  for element in document.elements() {
    paint_element(&painter, transform, element, 1.0);
  }
}

pub(crate) fn paint_element(
  painter: &Painter,
  transform: &CanvasTransform,
  element: &Element,
  opacity: f32,
) {
  match &element.payload {
    ElementPayload::Stroke(payload) => {
      let color = egui_color(payload.stroke_style.color_rgba, opacity);
      let stroke = Stroke::new(payload.stroke_style.width_px * transform.scale(), color);
      for points in payload.points.windows(2) {
        painter.line_segment(
          [
            transform.document_to_egui(points[0].point()),
            transform.document_to_egui(points[1].point()),
          ],
          stroke,
        );
      }
    }
    ElementPayload::Arrow(payload) => paint_arrow(painter, transform, payload, opacity),
    ElementPayload::Rectangle(payload) => paint_rectangle(painter, transform, payload, opacity),
    ElementPayload::Text(payload) => {
      paint_text(
        painter,
        transform.document_to_egui(payload.anchor_px),
        &payload.text,
        &payload.text_style,
        payload.box_width_px * transform.scale(),
        transform.scale(),
        opacity,
      );
    }
    ElementPayload::SequenceMarker(payload) => {
      let center = transform.document_to_egui(payload.center_px);
      let size = egui::vec2(
        payload.pill_width_px * transform.scale(),
        payload.radius_px * 2.0 * transform.scale(),
      );
      let rect = Rect::from_center_size(center, size);
      let radius = (payload.radius_px * transform.scale()).clamp(0.0, 255.0) as u8;
      painter.rect_filled(
        rect,
        egui::CornerRadius::same(radius),
        egui_color(payload.fill_rgba, opacity),
      );
      painter.rect_stroke(
        rect,
        egui::CornerRadius::same(radius),
        Stroke::new(
          payload.stroke_style.width_px * transform.scale(),
          egui_color(payload.stroke_style.color_rgba, opacity),
        ),
        StrokeKind::Middle,
      );
      painter.text(
        center,
        Align2::CENTER_CENTER,
        payload.number,
        FontId::proportional(payload.text_style.font_size_px * transform.scale()),
        egui_color(payload.text_style.color_rgba, opacity),
      );
    }
  }
}

pub(crate) fn paint_raw_polyline(
  painter: &Painter,
  transform: &CanvasTransform,
  points: &[PointPx],
  color: ColorRgba,
  width_px: f32,
) {
  let stroke = Stroke::new(width_px * transform.scale(), egui_color(color, 0.72));
  for points in points.windows(2) {
    painter.line_segment(
      [transform.document_to_egui(points[0]), transform.document_to_egui(points[1])],
      stroke,
    );
  }
}

fn paint_arrow(
  painter: &Painter,
  transform: &CanvasTransform,
  payload: &ArrowPayload,
  opacity: f32,
) {
  let color = egui_color(payload.stroke_style.color_rgba, opacity);
  let stroke = Stroke::new(payload.stroke_style.width_px * transform.scale(), color);
  painter.line_segment(
    [transform.document_to_egui(payload.start_px), transform.document_to_egui(payload.end_px)],
    stroke,
  );
  let [tip, left, right] = arrow_head_points(payload);
  painter.add(Shape::convex_polygon(
    vec![
      transform.document_to_egui(tip),
      transform.document_to_egui(left),
      transform.document_to_egui(right),
    ],
    color,
    Stroke::NONE,
  ));
}

fn paint_rectangle(
  painter: &Painter,
  transform: &CanvasTransform,
  payload: &RectanglePayload,
  opacity: f32,
) {
  let body =
    transform.document_rect_to_egui(common::RectPx::from_points(payload.start_px, payload.end_px));
  painter.rect_stroke(
    body,
    egui::CornerRadius::ZERO,
    Stroke::new(
      payload.stroke_style.width_px * transform.scale(),
      egui_color(payload.stroke_style.color_rgba, opacity),
    ),
    StrokeKind::Middle,
  );

  if let Ok(layout) = rectangle_label_layout(payload, transform.document_size()) {
    let label_rect = transform.document_rect_to_egui(layout.bounds_px);
    let radius = (5.0 * transform.scale()).clamp(0.0, 255.0) as u8;
    painter.rect_filled(
      label_rect,
      egui::CornerRadius::same(radius),
      egui_color(payload.stroke_style.color_rgba, opacity),
    );
    let padding = payload.label.padding_px * transform.scale();
    paint_text(
      painter,
      label_rect.min + egui::vec2(padding, padding),
      &payload.label.text,
      &payload.label.text_style,
      (label_rect.width() - padding * 2.0).max(1.0),
      transform.scale(),
      opacity,
    );
  }
}

fn paint_text(
  painter: &Painter,
  origin: Pos2,
  text: &str,
  style: &TextStyle,
  wrap_width: f32,
  scale: f32,
  opacity: f32,
) {
  let color = egui_color(style.color_rgba, opacity);
  let galley = painter.layout(
    text.to_owned(),
    FontId::proportional(style.font_size_px * scale),
    color,
    wrap_width.max(1.0),
  );
  let x = match style.align {
    TextAlign::Left => origin.x,
    TextAlign::Center => origin.x + (wrap_width - galley.size().x) / 2.0,
    TextAlign::Right => origin.x + wrap_width - galley.size().x,
  };
  painter.galley(egui::pos2(x, origin.y), galley, color);
}

fn raster_element(image: &mut RgbaImage, element: &Element, canvas_size: SizePx) {
  match &element.payload {
    ElementPayload::Stroke(payload) => {
      for points in payload.points.windows(2) {
        draw_thick_segment(
          image,
          points[0].point(),
          points[1].point(),
          payload.stroke_style.width_px,
          rgba(payload.stroke_style.color_rgba),
        );
      }
    }
    ElementPayload::Arrow(payload) => {
      let color = rgba(payload.stroke_style.color_rgba);
      draw_thick_segment(
        image,
        payload.start_px,
        payload.end_px,
        payload.stroke_style.width_px,
        color,
      );
      let [tip, left, right] = arrow_head_points(payload);
      fill_triangle(image, tip, left, right, color);
    }
    ElementPayload::Rectangle(payload) => {
      let color = rgba(payload.stroke_style.color_rgba);
      let body = common::RectPx::from_points(payload.start_px, payload.end_px);
      draw_thick_segment(
        image,
        body.min,
        PointPx::new(body.max.x_px, body.min.y_px),
        payload.stroke_style.width_px,
        color,
      );
      draw_thick_segment(
        image,
        PointPx::new(body.max.x_px, body.min.y_px),
        body.max,
        payload.stroke_style.width_px,
        color,
      );
      draw_thick_segment(
        image,
        body.max,
        PointPx::new(body.min.x_px, body.max.y_px),
        payload.stroke_style.width_px,
        color,
      );
      draw_thick_segment(
        image,
        PointPx::new(body.min.x_px, body.max.y_px),
        body.min,
        payload.stroke_style.width_px,
        color,
      );

      if let Ok(layout) = rectangle_label_layout(payload, canvas_size) {
        fill_rounded_rect(image, layout.bounds_px, 5.0, color);
        draw_wrapped_text(
          image,
          PointPx::new(
            layout.bounds_px.min.x_px + payload.label.padding_px,
            layout.bounds_px.min.y_px + payload.label.padding_px,
          ),
          &payload.label.text,
          &payload.label.text_style,
          (layout.bounds_px.width() - payload.label.padding_px * 2.0).max(1.0),
        );
      }
    }
    ElementPayload::Text(payload) => draw_wrapped_text(
      image,
      payload.anchor_px,
      &payload.text,
      &payload.text_style,
      payload.box_width_px,
    ),
    ElementPayload::SequenceMarker(payload) => {
      let bounds = common::RectPx::from_center_size(
        payload.center_px,
        payload.pill_width_px,
        payload.radius_px * 2.0,
      );
      fill_rounded_rect(image, bounds, payload.radius_px, rgba(payload.fill_rgba));
      let stroke_half = payload.stroke_style.width_px / 2.0;
      if stroke_half > 0.0 {
        stroke_rounded_rect(
          image,
          bounds,
          payload.radius_px,
          payload.stroke_style.width_px,
          rgba(payload.stroke_style.color_rgba),
        );
      }
      draw_centered_text(
        image,
        payload.center_px,
        &payload.number.to_string(),
        &payload.text_style,
      );
    }
  }
}

fn arrow_head_points(payload: &ArrowPayload) -> [PointPx; 3] {
  let x = payload.end_px.x_px - payload.start_px.x_px;
  let y = payload.end_px.y_px - payload.start_px.y_px;
  let length = x.hypot(y).max(f32::EPSILON);
  let unit = PointPx::new(x / length, y / length);
  let perpendicular = PointPx::new(-unit.y_px, unit.x_px);
  let base = PointPx::new(
    payload.end_px.x_px - unit.x_px * payload.head.length_px,
    payload.end_px.y_px - unit.y_px * payload.head.length_px,
  );
  let half_width = payload.head.width_px / 2.0;
  [
    payload.end_px,
    PointPx::new(
      base.x_px + perpendicular.x_px * half_width,
      base.y_px + perpendicular.y_px * half_width,
    ),
    PointPx::new(
      base.x_px - perpendicular.x_px * half_width,
      base.y_px - perpendicular.y_px * half_width,
    ),
  ]
}

fn draw_thick_segment(
  image: &mut RgbaImage,
  start: PointPx,
  end: PointPx,
  width: f32,
  color: Rgba<u8>,
) {
  let radius = width / 2.0;
  let min_x = (start.x_px.min(end.x_px) - radius - 1.0).floor() as i32;
  let max_x = (start.x_px.max(end.x_px) + radius + 1.0).ceil() as i32;
  let min_y = (start.y_px.min(end.y_px) - radius - 1.0).floor() as i32;
  let max_y = (start.y_px.max(end.y_px) + radius + 1.0).ceil() as i32;
  for y in min_y.max(0)..=max_y.min(image.height() as i32 - 1) {
    for x in min_x.max(0)..=max_x.min(image.width() as i32 - 1) {
      let point = PointPx::new(x as f32 + 0.5, y as f32 + 0.5);
      let distance = distance_to_segment(point, start, end);
      let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
      if coverage > 0.0 {
        blend_pixel(image, x as u32, y as u32, color, coverage);
      }
    }
  }
}

fn fill_triangle(
  image: &mut RgbaImage,
  first: PointPx,
  second: PointPx,
  third: PointPx,
  color: Rgba<u8>,
) {
  let min_x = first.x_px.min(second.x_px).min(third.x_px).floor() as i32;
  let max_x = first.x_px.max(second.x_px).max(third.x_px).ceil() as i32;
  let min_y = first.y_px.min(second.y_px).min(third.y_px).floor() as i32;
  let max_y = first.y_px.max(second.y_px).max(third.y_px).ceil() as i32;
  let area = edge(first, second, third);
  if area.abs() <= f32::EPSILON {
    return;
  }
  for y in min_y.max(0)..=max_y.min(image.height() as i32 - 1) {
    for x in min_x.max(0)..=max_x.min(image.width() as i32 - 1) {
      let point = PointPx::new(x as f32 + 0.5, y as f32 + 0.5);
      let a = edge(first, second, point);
      let b = edge(second, third, point);
      let c = edge(third, first, point);
      if (a >= 0.0 && b >= 0.0 && c >= 0.0) || (a <= 0.0 && b <= 0.0 && c <= 0.0) {
        blend_pixel(image, x as u32, y as u32, color, 1.0);
      }
    }
  }
}

fn fill_rounded_rect(image: &mut RgbaImage, rect: common::RectPx, radius: f32, color: Rgba<u8>) {
  let radius = radius.max(0.0).min(rect.width() / 2.0).min(rect.height() / 2.0);
  let min_x = rect.min.x_px.floor() as i32;
  let max_x = rect.max.x_px.ceil() as i32;
  let min_y = rect.min.y_px.floor() as i32;
  let max_y = rect.max.y_px.ceil() as i32;
  for y in min_y.max(0)..=max_y.min(image.height() as i32 - 1) {
    for x in min_x.max(0)..=max_x.min(image.width() as i32 - 1) {
      let point = PointPx::new(x as f32 + 0.5, y as f32 + 0.5);
      if rounded_rect_signed_distance(point, rect, radius) <= 0.0 {
        blend_pixel(image, x as u32, y as u32, color, 1.0);
      }
    }
  }
}

fn stroke_rounded_rect(
  image: &mut RgbaImage,
  rect: common::RectPx,
  radius: f32,
  width: f32,
  color: Rgba<u8>,
) {
  let half = width / 2.0;
  let expanded = rect.expanded(half);
  let min_x = expanded.min.x_px.floor() as i32;
  let max_x = expanded.max.x_px.ceil() as i32;
  let min_y = expanded.min.y_px.floor() as i32;
  let max_y = expanded.max.y_px.ceil() as i32;
  for y in min_y.max(0)..=max_y.min(image.height() as i32 - 1) {
    for x in min_x.max(0)..=max_x.min(image.width() as i32 - 1) {
      let point = PointPx::new(x as f32 + 0.5, y as f32 + 0.5);
      let distance = rounded_rect_signed_distance(point, rect, radius).abs();
      let coverage = (half + 0.5 - distance).clamp(0.0, 1.0);
      if coverage > 0.0 {
        blend_pixel(image, x as u32, y as u32, color, coverage);
      }
    }
  }
}

fn rounded_rect_signed_distance(point: PointPx, rect: common::RectPx, radius: f32) -> f32 {
  let center = rect.center();
  let half_width = rect.width() / 2.0 - radius;
  let half_height = rect.height() / 2.0 - radius;
  let dx = (point.x_px - center.x_px).abs() - half_width;
  let dy = (point.y_px - center.y_px).abs() - half_height;
  let outside = dx.max(0.0).hypot(dy.max(0.0));
  outside + dx.max(dy).min(0.0) - radius
}

fn draw_wrapped_text(
  image: &mut RgbaImage,
  origin: PointPx,
  text: &str,
  style: &TextStyle,
  maximum_width: f32,
) {
  let Some(font) = cjk_font() else {
    return;
  };
  let lines = wrap_text(text, style.font_size_px, maximum_width);
  for (index, line) in lines.iter().enumerate() {
    let (width, _) = text_size(PxScale::from(style.font_size_px), font, line);
    let x = match style.align {
      TextAlign::Left => origin.x_px,
      TextAlign::Center => origin.x_px + (maximum_width - width as f32) / 2.0,
      TextAlign::Right => origin.x_px + maximum_width - width as f32,
    };
    draw_text_mut(
      image,
      rgba(style.color_rgba),
      x.round() as i32,
      (origin.y_px + index as f32 * style.line_height_px).round() as i32,
      PxScale::from(style.font_size_px),
      font,
      line,
    );
  }
}

fn draw_centered_text(image: &mut RgbaImage, center: PointPx, text: &str, style: &TextStyle) {
  let Some(font) = cjk_font() else {
    return;
  };
  let scale = PxScale::from(style.font_size_px);
  let (width, height) = text_size(scale, font, text);
  draw_text_mut(
    image,
    rgba(style.color_rgba),
    (center.x_px - width as f32 / 2.0).round() as i32,
    (center.y_px - height as f32 / 2.0).round() as i32,
    scale,
    font,
    text,
  );
}

fn wrap_text(text: &str, font_size: f32, maximum_width: f32) -> Vec<String> {
  let mut lines = Vec::new();
  let mut line = String::new();
  let mut width = 0.0;
  for character in text.chars() {
    if character == '\n' {
      lines.push(std::mem::take(&mut line));
      width = 0.0;
      continue;
    }
    let character_width = if character.is_ascii_whitespace() {
      font_size * 0.33
    } else if character.is_ascii() {
      font_size * 0.6
    } else {
      font_size
    };
    if !line.is_empty() && width + character_width > maximum_width {
      lines.push(std::mem::take(&mut line));
      width = 0.0;
    }
    line.push(character);
    width += character_width;
  }
  lines.push(line);
  lines
}

fn cjk_font() -> Option<&'static FontArc> {
  static FONT: OnceLock<Option<FontArc>> = OnceLock::new();
  FONT.get_or_init(|| FontArc::try_from_slice(BUNDLED_CJK_FONT).ok()).as_ref()
}

fn distance_to_segment(point: PointPx, start: PointPx, end: PointPx) -> f32 {
  let dx = end.x_px - start.x_px;
  let dy = end.y_px - start.y_px;
  let length_squared = dx * dx + dy * dy;
  if length_squared <= f32::EPSILON {
    return point.distance_to(start);
  }
  let t = (((point.x_px - start.x_px) * dx + (point.y_px - start.y_px) * dy) / length_squared)
    .clamp(0.0, 1.0);
  point.distance_to(PointPx::new(start.x_px + t * dx, start.y_px + t * dy))
}

fn edge(first: PointPx, second: PointPx, point: PointPx) -> f32 {
  (point.x_px - first.x_px) * (second.y_px - first.y_px)
    - (point.y_px - first.y_px) * (second.x_px - first.x_px)
}

fn blend_pixel(image: &mut RgbaImage, x: u32, y: u32, color: Rgba<u8>, coverage: f32) {
  let destination = image.get_pixel_mut(x, y);
  let source_alpha = f32::from(color[3]) / 255.0 * coverage;
  let destination_alpha = f32::from(destination[3]) / 255.0;
  let output_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
  if output_alpha <= f32::EPSILON {
    *destination = Rgba([0, 0, 0, 0]);
    return;
  }
  for channel in 0..3 {
    let source = f32::from(color[channel]) / 255.0;
    let target = f32::from(destination[channel]) / 255.0;
    let output =
      (source * source_alpha + target * destination_alpha * (1.0 - source_alpha)) / output_alpha;
    destination[channel] = (output * 255.0).round() as u8;
  }
  destination[3] = (output_alpha * 255.0).round() as u8;
}

fn egui_color(color: ColorRgba, opacity: f32) -> Color32 {
  Color32::from_rgba_unmultiplied(
    color.red,
    color.green,
    color.blue,
    (f32::from(color.alpha) * opacity.clamp(0.0, 1.0)).round() as u8,
  )
}

fn rgba(color: ColorRgba) -> Rgba<u8> {
  Rgba([color.red, color.green, color.blue, color.alpha])
}

#[cfg(test)]
mod tests {
  use chrono::{TimeZone, Utc};
  use common::{
    ArrowHead, ArrowPayload, CapturedDisplay, DocumentId, ElementId, GlobalBoundsPx,
    LabelPlacementPreference, RectangleLabel, RectanglePayload, SequenceMarkerPayload, StrokeStyle,
    TextPayload, TextStyle,
  };
  use uuid::Uuid;

  use super::*;

  fn document() -> BoardDocument {
    document_with_size(100, 80)
  }

  fn document_with_size(width_px: u32, height_px: u32) -> BoardDocument {
    BoardDocument::new_capture(
      DocumentId::from_uuid(Uuid::nil()),
      SizePx::new(width_px, height_px),
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx { x_px: 0, y_px: 0, width_px, height_px },
        scale_factor: 1.0,
      },
      Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).unwrap(),
    )
    .unwrap()
  }

  #[test]
  fn raster_renderer_draws_rectangle_geometry() {
    let mut document = document();
    let style = StrokeStyle::mvp(ColorRgba::RED, 4.0).unwrap();
    let label_style = TextStyle::mvp(style.color_rgba.contrasting_text(), 12.0).unwrap();
    document.elements.push(
      Element::new(
        ElementId::new(),
        0,
        ElementPayload::Rectangle(RectanglePayload {
          start_px: PointPx::new(20.0, 30.0),
          end_px: PointPx::new(70.0, 60.0),
          stroke_style: style,
          fill_rgba: None,
          label: RectangleLabel {
            text: "title".to_owned(),
            placement_preference: LabelPlacementPreference::Above,
            max_width_px: 80.0,
            padding_px: 2.0,
            anchor_offset_px: 2.0,
            text_style: label_style,
          },
        }),
        document.canvas_size_px,
      )
      .unwrap(),
    );
    let background = RgbaImage::from_pixel(100, 80, Rgba([0, 0, 0, 255]));
    let rendered = render_document_to_image(&document, &background);
    assert_eq!(rendered.dimensions(), (100, 80));
    assert!(rendered.get_pixel(20, 45)[0] > 200);
    assert!(rendered.get_pixel(45, 45)[0] < 20);
  }

  #[test]
  fn renderer_accepts_document_snapshot() {
    let document = document();
    let snapshot = document.snapshot(document.revision).unwrap();
    let background = RgbaImage::from_pixel(100, 80, Rgba([3, 4, 5, 255]));
    let rendered = render_document_to_image(&snapshot, &background);
    assert_eq!(rendered.get_pixel(0, 0), &Rgba([3, 4, 5, 255]));
  }

  #[test]
  fn bundled_cjk_font_loads_for_raster_output() {
    assert!(cjk_font().is_some());
  }

  #[test]
  fn raster_renderer_wraps_long_chinese_text_with_bundled_font() {
    let mut document = document_with_size(420, 180);
    let text_style = TextStyle::mvp(ColorRgba::RED, 24.0).unwrap();
    document.elements.push(
      Element::new(
        ElementId::new(),
        0,
        ElementPayload::Text(TextPayload {
          anchor_px: PointPx::new(20.0, 20.0),
          text: "这是一段很长的中文讲义批注，用来验证固定字体、自动换行和离屏渲染。".to_owned(),
          box_width_px: 132.0,
          text_style,
        }),
        document.canvas_size_px,
      )
      .unwrap(),
    );

    let background = RgbaImage::from_pixel(420, 180, Rgba([0, 0, 0, 255]));
    let rendered = render_document_to_image(&document, &background);
    let upper_line_pixels = count_pixels_matching(&rendered, 0..90, is_red_text);
    let lower_line_pixels = count_pixels_matching(&rendered, 90..180, is_red_text);
    assert!(upper_line_pixels > 20, "expected visible text in the first wrapped line");
    assert!(lower_line_pixels > 20, "expected visible text after wrapping");
  }

  #[test]
  fn raster_renderer_draws_multi_digit_sequence_marker() {
    let mut document = document_with_size(180, 120);
    let fill = ColorRgba::YELLOW;
    let stroke_style = StrokeStyle::mvp(fill, 4.0).unwrap();
    let text_style = TextStyle::mvp(fill.contrasting_text(), 24.0).unwrap();
    document.elements.push(
      Element::new(
        ElementId::new(),
        0,
        ElementPayload::SequenceMarker(SequenceMarkerPayload {
          center_px: PointPx::new(90.0, 60.0),
          number: 128,
          radius_px: 24.0,
          pill_width_px: 80.0,
          fill_rgba: fill,
          stroke_style,
          text_style,
        }),
        document.canvas_size_px,
      )
      .unwrap(),
    );

    let background = RgbaImage::from_pixel(180, 120, Rgba([255, 255, 255, 255]));
    let rendered = render_document_to_image(&document, &background);
    assert_eq!(rendered.get_pixel(60, 60), &Rgba([255, 214, 10, 255]));
    let dark_text_pixels = count_pixels_matching(&rendered, 45..75, |pixel| {
      pixel[0] < 80 && pixel[1] < 80 && pixel[2] < 80 && pixel[3] == 255
    });
    assert!(dark_text_pixels > 10, "expected visible three-digit marker text");
  }

  #[test]
  fn raster_renderer_preserves_srgb_rgba_values() {
    let mut document = document_with_size(80, 60);
    let style = StrokeStyle::mvp(ColorRgba::BLUE, 4.0).unwrap();
    let text_style = TextStyle::mvp(ColorRgba::BLUE.contrasting_text(), 12.0).unwrap();
    document.elements.push(
      Element::new(
        ElementId::new(),
        0,
        ElementPayload::Rectangle(RectanglePayload {
          start_px: PointPx::new(20.0, 20.0),
          end_px: PointPx::new(60.0, 45.0),
          stroke_style: style,
          fill_rgba: None,
          label: RectangleLabel {
            text: "标签".to_owned(),
            placement_preference: LabelPlacementPreference::Above,
            max_width_px: 60.0,
            padding_px: 2.0,
            anchor_offset_px: 2.0,
            text_style,
          },
        }),
        document.canvas_size_px,
      )
      .unwrap(),
    );

    let background = RgbaImage::from_pixel(80, 60, Rgba([3, 5, 7, 255]));
    let rendered = render_document_to_image(&document, &background);
    assert_eq!(rendered.get_pixel(0, 0), &Rgba([3, 5, 7, 255]));
    assert_eq!(rendered.get_pixel(20, 32), &Rgba([10, 132, 255, 255]));
  }

  #[test]
  fn raster_renderer_accepts_4k_and_8k_canvases() {
    let four_k = document_with_size(3_840, 2_160);
    let four_k_background = RgbaImage::from_pixel(3_840, 2_160, Rgba([1, 2, 3, 255]));
    let four_k_rendered = render_document_to_image(&four_k, &four_k_background);
    assert_eq!(four_k_rendered.dimensions(), (3_840, 2_160));

    let eight_k = document_with_size(8_192, 64);
    let eight_k_background = RgbaImage::from_pixel(8_192, 64, Rgba([4, 5, 6, 255]));
    let eight_k_rendered = render_document_to_image(&eight_k, &eight_k_background);
    assert_eq!(eight_k_rendered.dimensions(), (8_192, 64));
  }

  #[test]
  fn snapshot_and_reopened_document_render_identical_geometry() {
    let mut document = document_with_size(160, 120);
    let stroke_style = StrokeStyle::mvp(ColorRgba::GREEN, 8.0).unwrap();
    document.elements.push(
      Element::new(
        ElementId::new(),
        0,
        ElementPayload::Arrow(ArrowPayload {
          start_px: PointPx::new(24.0, 96.0),
          end_px: PointPx::new(132.0, 28.0),
          stroke_style,
          head: ArrowHead::for_stroke_width(8.0).unwrap(),
        }),
        document.canvas_size_px,
      )
      .unwrap(),
    );
    let background = RgbaImage::from_pixel(160, 120, Rgba([8, 8, 8, 255]));

    let live = render_document_to_image(&document, &background);
    let snapshot = document.snapshot(document.revision).unwrap();
    let preview = render_document_to_image(&snapshot, &background);
    let serialized = serde_json::to_string(&document).unwrap();
    let reopened: BoardDocument = serde_json::from_str(&serialized).unwrap();
    let final_png = render_document_to_image(&reopened, &background);

    assert_eq!(live.as_raw(), preview.as_raw());
    assert_eq!(live.as_raw(), final_png.as_raw());
  }

  fn count_pixels_matching(
    image: &RgbaImage,
    rows: std::ops::Range<u32>,
    predicate: impl Fn(&Rgba<u8>) -> bool,
  ) -> usize {
    rows
      .flat_map(|y| (0..image.width()).map(move |x| (x, y)))
      .filter(|&(x, y)| predicate(image.get_pixel(x, y)))
      .count()
  }

  fn is_red_text(pixel: &Rgba<u8>) -> bool {
    pixel[0] > 120 && pixel[1] < 90 && pixel[2] < 90 && pixel[3] == 255
  }
}
