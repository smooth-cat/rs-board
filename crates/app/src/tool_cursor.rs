use std::sync::{Arc, OnceLock};

use eframe::egui::CustomCursorImage;

use crate::editor::EditorTool;

const ARROW_CURSOR_SIZE: [u16; 2] = [28, 28];
const ARROW_HOTSPOT: [f32; 2] = [2.0, 2.0];
const BRUSH_CURSOR_SIZE: [u16; 2] = [28, 28];
const BRUSH_HOTSPOT: [f32; 2] = [3.0, 25.0];
const CURSOR_SIZE_SCALE: f32 = 0.75;
const BADGE_DESIGN_X_OFFSET: f32 = 1.0 / CURSOR_SIZE_SCALE;
const SUPERSAMPLE: usize = 4;
const NATIVE_CURSOR_RASTER_SCALE: f32 = 8.0;

const BLACK: [u8; 4] = [18, 18, 20, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];
const LIGHT_GRAY: [u8; 4] = [212, 214, 218, 255];
const DARK_GRAY: [u8; 4] = [74, 76, 82, 255];

const POINTER: [(f32, f32); 7] =
  [(2.0, 2.0), (2.0, 20.1), (6.4, 15.8), (9.9, 23.0), (13.0, 21.5), (9.6, 14.5), (15.7, 14.5)];

#[derive(Clone)]
pub(crate) struct NativeCursorImage {
  pub(crate) rgba: Arc<[u8]>,
  pub(crate) pixel_size: [u16; 2],
  pub(crate) logical_size: [f32; 2],
  pub(crate) hotspot: [f32; 2],
}

#[derive(Clone)]
struct ToolCursorImages {
  select: CustomCursorImage,
  rectangle: CustomCursorImage,
  arrow: CustomCursorImage,
  text: CustomCursorImage,
  brush: CustomCursorImage,
  sequence: CustomCursorImage,
}

#[derive(Clone)]
struct NativeToolCursorImages {
  select: NativeCursorImage,
  rectangle: NativeCursorImage,
  arrow: NativeCursorImage,
  text: NativeCursorImage,
  brush: NativeCursorImage,
  sequence: NativeCursorImage,
}

pub(crate) fn image_for(tool: EditorTool) -> Option<CustomCursorImage> {
  let images = CURSORS.get_or_init(ToolCursorImages::new);
  match tool {
    EditorTool::Select => Some(images.select.clone()),
    EditorTool::Rectangle => Some(images.rectangle.clone()),
    EditorTool::Arrow => Some(images.arrow.clone()),
    EditorTool::Text => Some(images.text.clone()),
    EditorTool::Stroke => Some(images.brush.clone()),
    EditorTool::Sequence => Some(images.sequence.clone()),
  }
}

pub(crate) fn native_image_for(tool: EditorTool) -> Option<NativeCursorImage> {
  let images = NATIVE_CURSORS.get_or_init(NativeToolCursorImages::new);
  match tool {
    EditorTool::Select => Some(images.select.clone()),
    EditorTool::Rectangle => Some(images.rectangle.clone()),
    EditorTool::Arrow => Some(images.arrow.clone()),
    EditorTool::Text => Some(images.text.clone()),
    EditorTool::Stroke => Some(images.brush.clone()),
    EditorTool::Sequence => Some(images.sequence.clone()),
  }
}

pub(crate) fn raster_size_for(tool: EditorTool, raster_scale: f32) -> Option<[u16; 2]> {
  match tool {
    EditorTool::Stroke => Some(scaled_size(BRUSH_CURSOR_SIZE, raster_scale)),
    EditorTool::Select
    | EditorTool::Rectangle
    | EditorTool::Arrow
    | EditorTool::Text
    | EditorTool::Sequence => Some(scaled_size(ARROW_CURSOR_SIZE, raster_scale)),
  }
}

pub(crate) fn image_for_scale(tool: EditorTool, raster_scale: f32) -> Option<CustomCursorImage> {
  match tool {
    EditorTool::Select => Some(pointer_cursor(raster_scale)),
    EditorTool::Rectangle => Some(badge_cursor(raster_scale, paint_rectangle_badge)),
    EditorTool::Arrow => Some(badge_cursor(raster_scale, paint_arrow_badge)),
    EditorTool::Text => Some(badge_cursor(raster_scale, paint_text_badge)),
    EditorTool::Stroke => Some(brush_cursor(raster_scale)),
    EditorTool::Sequence => Some(badge_cursor(raster_scale, paint_sequence_badge)),
  }
}

// Keep the baseline rasters stable; density-specific rasters are cached as egui textures by the
// editor and are rebuilt only when their target physical size changes.
static CURSORS: OnceLock<ToolCursorImages> = OnceLock::new();
static NATIVE_CURSORS: OnceLock<NativeToolCursorImages> = OnceLock::new();

impl ToolCursorImages {
  fn new() -> Self {
    Self {
      select: pointer_cursor(1.0),
      rectangle: badge_cursor(1.0, paint_rectangle_badge),
      arrow: badge_cursor(1.0, paint_arrow_badge),
      text: badge_cursor(1.0, paint_text_badge),
      brush: brush_cursor(1.0),
      sequence: badge_cursor(1.0, paint_sequence_badge),
    }
  }
}

impl NativeToolCursorImages {
  fn new() -> Self {
    Self {
      select: native_cursor(EditorTool::Select),
      rectangle: native_cursor(EditorTool::Rectangle),
      arrow: native_cursor(EditorTool::Arrow),
      text: native_cursor(EditorTool::Text),
      brush: native_cursor(EditorTool::Stroke),
      sequence: native_cursor(EditorTool::Sequence),
    }
  }
}

fn native_cursor(tool: EditorTool) -> NativeCursorImage {
  let image = image_for_scale(tool, NATIVE_CURSOR_RASTER_SCALE)
    .expect("native cursor tool should have an image");
  // Apply the app's visual scale in logical points. AppKit then applies the user's accessibility
  // cursor scale; the dense backing raster retains enough pixels for that enlarged presentation.
  let (design_size, design_hotspot) = match tool {
    EditorTool::Stroke => (BRUSH_CURSOR_SIZE.map(f32::from), BRUSH_HOTSPOT),
    EditorTool::Select
    | EditorTool::Rectangle
    | EditorTool::Arrow
    | EditorTool::Text
    | EditorTool::Sequence => (ARROW_CURSOR_SIZE.map(f32::from), ARROW_HOTSPOT),
  };
  let logical_size = design_size.map(|dimension| dimension * CURSOR_SIZE_SCALE);
  let hotspot = design_hotspot.map(|coordinate| coordinate * CURSOR_SIZE_SCALE);
  NativeCursorImage { rgba: image.rgba, pixel_size: image.size, logical_size, hotspot }
}

fn pointer_cursor(raster_scale: f32) -> CustomCursorImage {
  badge_cursor(raster_scale, |_| {})
}

fn badge_cursor(raster_scale: f32, paint_badge: impl FnOnce(&mut Raster)) -> CustomCursorImage {
  let mut raster = Raster::new(ARROW_CURSOR_SIZE, raster_scale);
  raster.stroke_polygon(&POINTER, 3.5, WHITE);
  raster.fill_polygon(&POINTER, BLACK);
  paint_badge(&mut raster);
  raster.finish(ARROW_HOTSPOT)
}

fn paint_rectangle_badge(raster: &mut Raster) {
  let corners = [(15.0, 15.0), (23.0, 15.0), (23.0, 23.0), (15.0, 23.0)].map(offset_badge_point);
  raster.stroke_polygon(&corners, 3.4, WHITE);
  raster.stroke_polygon(&corners, 1.4, BLACK);
}

fn paint_arrow_badge(raster: &mut Raster) {
  let segments = [
    (offset_badge_point((14.5, 22.5)), offset_badge_point((23.0, 14.0))),
    (offset_badge_point((18.0, 14.0)), offset_badge_point((23.0, 14.0))),
    (offset_badge_point((23.0, 14.0)), offset_badge_point((23.0, 19.0))),
  ];
  for color_and_width in [(WHITE, 3.4), (BLACK, 1.35)] {
    for (start, end) in segments {
      raster.line(start, end, color_and_width.1, color_and_width.0);
    }
  }
}

fn paint_text_badge(raster: &mut Raster) {
  let segments = [
    (offset_badge_point((15.5, 14.5)), offset_badge_point((22.5, 14.5))),
    (offset_badge_point((19.0, 14.5)), offset_badge_point((19.0, 23.0))),
    (offset_badge_point((15.5, 23.0)), offset_badge_point((22.5, 23.0))),
  ];
  for color_and_width in [(WHITE, 3.4), (BLACK, 1.35)] {
    for (start, end) in segments {
      raster.line(start, end, color_and_width.1, color_and_width.0);
    }
  }
}

fn paint_sequence_badge(raster: &mut Raster) {
  let center = offset_badge_point((19.0, 19.0));
  raster.circle(center, 6.2, WHITE);
  raster.circle(center, 5.0, BLACK);
  raster.line(offset_badge_point((17.6, 17.4)), offset_badge_point((19.1, 16.3)), 1.25, WHITE);
  raster.line(offset_badge_point((19.1, 16.3)), offset_badge_point((19.1, 21.8)), 1.25, WHITE);
  raster.line(offset_badge_point((17.7, 21.8)), offset_badge_point((20.6, 21.8)), 1.25, WHITE);
}

fn offset_badge_point((x, y): (f32, f32)) -> (f32, f32) {
  (x + BADGE_DESIGN_X_OFFSET, y)
}

fn brush_cursor(raster_scale: f32) -> CustomCursorImage {
  let mut raster = Raster::new(BRUSH_CURSOR_SIZE, raster_scale);
  let bristles = [(3.0, 25.0), (4.7, 18.0), (10.4, 23.6)];
  let ferrule = [(4.7, 18.0), (9.2, 13.5), (14.9, 19.2), (10.4, 23.6)];
  let handle = [(8.9, 13.8), (18.5, 4.2), (22.8, 8.5), (13.2, 18.1)];

  for polygon in [&bristles[..], &ferrule[..], &handle[..]] {
    raster.stroke_polygon(polygon, 3.5, WHITE);
  }
  raster.fill_polygon(&handle, DARK_GRAY);
  raster.fill_polygon(&ferrule, LIGHT_GRAY);
  raster.fill_polygon(&bristles, BLACK);
  raster.line((18.5, 4.2), (22.8, 8.5), 1.2, BLACK);
  raster.finish(BRUSH_HOTSPOT)
}

struct Raster {
  base_size: [u16; 2],
  size: [u16; 2],
  width: usize,
  height: usize,
  sample_scale: [f32; 2],
  pixels: Vec<[u8; 4]>,
}

impl Raster {
  fn new(base_size: [u16; 2], raster_scale: f32) -> Self {
    let size = scaled_size(base_size, raster_scale);
    let width = usize::from(size[0]) * SUPERSAMPLE;
    let height = usize::from(size[1]) * SUPERSAMPLE;
    let sample_scale =
      [width as f32 / f32::from(base_size[0]), height as f32 / f32::from(base_size[1])];
    Self { base_size, size, width, height, sample_scale, pixels: vec![[0; 4]; width * height] }
  }

  fn fill_polygon(&mut self, points: &[(f32, f32)], color: [u8; 4]) {
    self.paint_bounds(points, 0.0, |point| point_in_polygon(point, points), color);
  }

  fn stroke_polygon(&mut self, points: &[(f32, f32)], width: f32, color: [u8; 4]) {
    for index in 0..points.len() {
      self.line(points[index], points[(index + 1) % points.len()], width, color);
    }
  }

  fn line(&mut self, start: (f32, f32), end: (f32, f32), width: f32, color: [u8; 4]) {
    let radius = width / 2.0;
    self.paint_bounds(
      &[start, end],
      radius,
      |point| distance_to_segment(point, start, end) <= radius,
      color,
    );
  }

  fn circle(&mut self, center: (f32, f32), radius: f32, color: [u8; 4]) {
    self.paint_bounds(&[center], radius, |point| distance(point, center) <= radius, color);
  }

  fn paint_bounds(
    &mut self,
    points: &[(f32, f32)],
    padding: f32,
    contains: impl Fn((f32, f32)) -> bool,
    color: [u8; 4],
  ) {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for &(x, y) in points {
      min_x = min_x.min(x);
      min_y = min_y.min(y);
      max_x = max_x.max(x);
      max_y = max_y.max(y);
    }
    let x_start = (((min_x - padding) * self.sample_scale[0]).floor() as isize).max(0) as usize;
    let y_start = (((min_y - padding) * self.sample_scale[1]).floor() as isize).max(0) as usize;
    let x_end = (((max_x + padding) * self.sample_scale[0]).ceil() as isize)
      .clamp(0, self.width as isize) as usize;
    let y_end = (((max_y + padding) * self.sample_scale[1]).ceil() as isize)
      .clamp(0, self.height as isize) as usize;
    for y in y_start..y_end {
      for x in x_start..x_end {
        let point =
          ((x as f32 + 0.5) / self.sample_scale[0], (y as f32 + 0.5) / self.sample_scale[1]);
        if contains(point) {
          self.pixels[y * self.width + x] = color;
        }
      }
    }
  }

  fn finish(self, hotspot: [f32; 2]) -> CustomCursorImage {
    let output_width = usize::from(self.size[0]);
    let output_height = usize::from(self.size[1]);
    let samples_per_pixel = (SUPERSAMPLE * SUPERSAMPLE) as u32;
    let mut rgba = Vec::with_capacity(output_width * output_height * 4);
    for output_y in 0..output_height {
      for output_x in 0..output_width {
        let mut alpha_sum = 0_u32;
        let mut premultiplied = [0_u32; 3];
        for sample_y in 0..SUPERSAMPLE {
          for sample_x in 0..SUPERSAMPLE {
            let x = output_x * SUPERSAMPLE + sample_x;
            let y = output_y * SUPERSAMPLE + sample_y;
            let [red, green, blue, alpha] = self.pixels[y * self.width + x];
            let alpha = u32::from(alpha);
            alpha_sum += alpha;
            premultiplied[0] += u32::from(red) * alpha;
            premultiplied[1] += u32::from(green) * alpha;
            premultiplied[2] += u32::from(blue) * alpha;
          }
        }
        let alpha = ((alpha_sum + samples_per_pixel / 2) / samples_per_pixel) as u8;
        let Some(alpha_sum) = std::num::NonZeroU32::new(alpha_sum) else {
          rgba.extend_from_slice(&[0, 0, 0, 0]);
          continue;
        };
        let alpha_sum = alpha_sum.get();
        rgba.extend_from_slice(&[
          ((premultiplied[0] + alpha_sum / 2) / alpha_sum) as u8,
          ((premultiplied[1] + alpha_sum / 2) / alpha_sum) as u8,
          ((premultiplied[2] + alpha_sum / 2) / alpha_sum) as u8,
          alpha,
        ]);
      }
    }
    let hotspot = [
      scaled_hotspot_coordinate(hotspot[0], self.base_size[0], self.size[0]),
      scaled_hotspot_coordinate(hotspot[1], self.base_size[1], self.size[1]),
    ];
    CustomCursorImage { rgba: Arc::from(rgba), size: self.size, hotspot }
  }
}

fn scaled_size(base_size: [u16; 2], raster_scale: f32) -> [u16; 2] {
  let raster_scale =
    if raster_scale.is_finite() { raster_scale.clamp(0.5, 16.0) } else { 1.0 } * CURSOR_SIZE_SCALE;
  base_size.map(|dimension| {
    (f32::from(dimension) * raster_scale).round().clamp(1.0, f32::from(u16::MAX)) as u16
  })
}

fn scaled_hotspot_coordinate(coordinate: f32, base_size: u16, output_size: u16) -> u16 {
  (coordinate * f32::from(output_size) / f32::from(base_size))
    .round()
    .clamp(0.0, f32::from(output_size.saturating_sub(1))) as u16
}

fn point_in_polygon(point: (f32, f32), polygon: &[(f32, f32)]) -> bool {
  let mut inside = false;
  let mut previous = polygon[polygon.len() - 1];
  for &current in polygon {
    let crosses = (current.1 > point.1) != (previous.1 > point.1)
      && point.0
        < (previous.0 - current.0) * (point.1 - current.1) / (previous.1 - current.1) + current.0;
    if crosses {
      inside = !inside;
    }
    previous = current;
  }
  inside
}

fn distance_to_segment(point: (f32, f32), start: (f32, f32), end: (f32, f32)) -> f32 {
  let delta = (end.0 - start.0, end.1 - start.1);
  let length_squared = delta.0 * delta.0 + delta.1 * delta.1;
  if length_squared <= f32::EPSILON {
    return distance(point, start);
  }
  let projection = ((point.0 - start.0) * delta.0 + (point.1 - start.1) * delta.1) / length_squared;
  let projection = projection.clamp(0.0, 1.0);
  distance(point, (start.0 + projection * delta.0, start.1 + projection * delta.1))
}

fn distance(left: (f32, f32), right: (f32, f32)) -> f32 {
  (left.0 - right.0).hypot(left.1 - right.1)
}

#[cfg(test)]
mod tests {
  use super::*;

  const CUSTOM_TOOLS: [EditorTool; 6] = EditorTool::ALL;

  #[test]
  fn all_tools_have_valid_custom_cursors() {
    for tool in CUSTOM_TOOLS {
      let image = image_for(tool).expect("custom tool should have a cursor image");
      assert!(image.size[0] <= 32 && image.size[1] <= 32, "{tool:?}: {:?}", image.size);
      assert_eq!(
        image.rgba.len(),
        usize::from(image.size[0]) * usize::from(image.size[1]) * 4,
        "{tool:?}"
      );
      assert!(image.hotspot[0] < image.size[0] && image.hotspot[1] < image.size[1]);
      assert!(alpha_at(&image, image.hotspot) > 0, "{tool:?} hotspot must touch its tip");
    }
  }

  #[test]
  fn arrow_tools_share_the_pointer_tip_and_brush_uses_its_bristle_tip() {
    for tool in [
      EditorTool::Select,
      EditorTool::Rectangle,
      EditorTool::Arrow,
      EditorTool::Text,
      EditorTool::Sequence,
    ] {
      assert_eq!(image_for(tool).unwrap().hotspot, [2, 2], "{tool:?}");
    }
    assert_eq!(image_for(EditorTool::Stroke).unwrap().hotspot, [2, 19]);
  }

  #[test]
  fn retina_raster_keeps_logical_geometry_at_double_pixel_density() {
    for tool in CUSTOM_TOOLS {
      let standard = image_for_scale(tool, 1.0).unwrap();
      let retina = image_for_scale(tool, 2.0).unwrap();

      assert_eq!(retina.size, standard.size.map(|dimension| dimension * 2), "{tool:?}");
      for axis in 0..2 {
        assert!(
          (f32::from(standard.hotspot[axis]) - f32::from(retina.hotspot[axis]) / 2.0).abs() <= 0.5,
          "{tool:?}"
        );
      }
      assert_eq!(
        retina.rgba.len(),
        usize::from(retina.size[0]) * usize::from(retina.size[1]) * 4,
        "{tool:?}"
      );
    }
  }

  #[test]
  fn raster_size_is_bounded_for_invalid_or_extreme_density() {
    assert_eq!(raster_size_for(EditorTool::Select, 2.0), Some([42, 42]));
    assert_eq!(raster_size_for(EditorTool::Rectangle, f32::NAN), Some([21, 21]));
    assert_eq!(raster_size_for(EditorTool::Stroke, 100.0), Some([336, 336]));
  }

  #[test]
  fn native_cursors_leave_accessibility_scaling_to_appkit() {
    for tool in CUSTOM_TOOLS {
      let image = native_image_for(tool).unwrap();
      assert_eq!(image.pixel_size, [168, 168], "{tool:?}");
      assert_eq!(image.logical_size, [21.0, 21.0], "{tool:?}");
      assert_eq!(
        image.hotspot,
        if tool == EditorTool::Stroke { [2.25, 18.75] } else { [1.5, 1.5] },
        "{tool:?}"
      );
    }
  }

  #[test]
  fn repeated_requests_reuse_the_same_os_cursor_bitmap() {
    for tool in CUSTOM_TOOLS {
      let first = image_for(tool).unwrap();
      let second = image_for(tool).unwrap();
      assert!(Arc::ptr_eq(&first.rgba, &second.rgba), "{tool:?}");
    }
  }

  #[test]
  fn pointer_badges_stay_in_the_compact_lower_right_region() {
    for tool in [EditorTool::Rectangle, EditorTool::Arrow, EditorTool::Text, EditorTool::Sequence] {
      let image = image_for(tool).unwrap();
      let badge_pixels = visible_pixels(&image, 12..21, 11..21);
      let near_pointer_pixels = visible_pixels(&image, 10..16, 10..16);
      assert!(badge_pixels >= 12, "{tool:?} badge is missing");
      assert!(near_pointer_pixels >= 1, "{tool:?} badge is not compact with the pointer");
    }
  }

  #[test]
  fn selection_cursor_contains_only_the_shared_pointer() {
    let image = image_for(EditorTool::Select).unwrap();
    let expected = pointer_cursor(1.0);
    assert_eq!(image.size, expected.size);
    assert_eq!(image.hotspot, expected.hotspot);
    assert_eq!(image.rgba.as_ref(), expected.rgba.as_ref());
    assert_eq!(visible_pixels(&image, 14..21, 13..21), 0);
  }

  fn alpha_at(image: &CustomCursorImage, point: [u16; 2]) -> u8 {
    let index =
      (usize::from(point[1]) * usize::from(image.size[0]) + usize::from(point[0])) * 4 + 3;
    image.rgba[index]
  }

  fn visible_pixels(
    image: &CustomCursorImage,
    x_range: std::ops::Range<usize>,
    y_range: std::ops::Range<usize>,
  ) -> usize {
    let width = usize::from(image.size[0]);
    y_range
      .flat_map(|y| x_range.clone().map(move |x| (y * width + x) * 4 + 3))
      .filter(|&index| image.rgba[index] > 32)
      .count()
  }
}
