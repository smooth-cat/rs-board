use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_CANVAS_DIMENSION_PX: u32 = 8_192;
const GEOMETRY_EPSILON: f32 = 0.001;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointPx {
  pub x_px: f32,
  pub y_px: f32,
}

impl PointPx {
  pub const ZERO: Self = Self { x_px: 0.0, y_px: 0.0 };

  pub const fn new(x_px: f32, y_px: f32) -> Self {
    Self { x_px, y_px }
  }

  pub fn is_finite(self) -> bool {
    self.x_px.is_finite() && self.y_px.is_finite()
  }

  pub fn distance_to(self, other: Self) -> f32 {
    let x = self.x_px - other.x_px;
    let y = self.y_px - other.y_px;
    x.hypot(y)
  }
}

impl std::ops::Add for PointPx {
  type Output = Self;

  fn add(self, rhs: Self) -> Self::Output {
    Self::new(self.x_px + rhs.x_px, self.y_px + rhs.y_px)
  }
}

impl std::ops::Sub for PointPx {
  type Output = Self;

  fn sub(self, rhs: Self) -> Self::Output {
    Self::new(self.x_px - rhs.x_px, self.y_px - rhs.y_px)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizePx {
  pub width_px: u32,
  pub height_px: u32,
}

impl SizePx {
  pub const fn new(width_px: u32, height_px: u32) -> Self {
    Self { width_px, height_px }
  }

  pub fn validate(self) -> Result<(), GeometryError> {
    if self.width_px == 0 || self.height_px == 0 {
      return Err(GeometryError::EmptyCanvas);
    }
    if self.width_px > MAX_CANVAS_DIMENSION_PX || self.height_px > MAX_CANVAS_DIMENSION_PX {
      return Err(GeometryError::CanvasTooLarge {
        width_px: self.width_px,
        height_px: self.height_px,
      });
    }
    Ok(())
  }

  pub fn bounds(self) -> RectPx {
    RectPx::from_min_max(PointPx::ZERO, PointPx::new(self.width_px as f32, self.height_px as f32))
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RectPx {
  pub min: PointPx,
  pub max: PointPx,
}

impl RectPx {
  pub fn from_points(first: PointPx, second: PointPx) -> Self {
    Self {
      min: PointPx::new(first.x_px.min(second.x_px), first.y_px.min(second.y_px)),
      max: PointPx::new(first.x_px.max(second.x_px), first.y_px.max(second.y_px)),
    }
  }

  pub const fn from_min_max(min: PointPx, max: PointPx) -> Self {
    Self { min, max }
  }

  pub fn from_center_size(center: PointPx, width_px: f32, height_px: f32) -> Self {
    let half_width = width_px / 2.0;
    let half_height = height_px / 2.0;
    Self::from_min_max(
      PointPx::new(center.x_px - half_width, center.y_px - half_height),
      PointPx::new(center.x_px + half_width, center.y_px + half_height),
    )
  }

  pub fn validate(self) -> Result<(), GeometryError> {
    if !self.min.is_finite() || !self.max.is_finite() {
      return Err(GeometryError::NonFiniteCoordinate);
    }
    if self.min.x_px > self.max.x_px || self.min.y_px > self.max.y_px {
      return Err(GeometryError::InvertedBounds);
    }
    Ok(())
  }

  pub fn width(self) -> f32 {
    self.max.x_px - self.min.x_px
  }

  pub fn height(self) -> f32 {
    self.max.y_px - self.min.y_px
  }

  pub fn center(self) -> PointPx {
    PointPx::new((self.min.x_px + self.max.x_px) / 2.0, (self.min.y_px + self.max.y_px) / 2.0)
  }

  pub fn translated(self, delta: PointPx) -> Self {
    Self::from_min_max(self.min + delta, self.max + delta)
  }

  pub fn expanded(self, amount_px: f32) -> Self {
    Self::from_min_max(
      PointPx::new(self.min.x_px - amount_px, self.min.y_px - amount_px),
      PointPx::new(self.max.x_px + amount_px, self.max.y_px + amount_px),
    )
  }

  pub fn union(self, other: Self) -> Self {
    Self::from_min_max(
      PointPx::new(self.min.x_px.min(other.min.x_px), self.min.y_px.min(other.min.y_px)),
      PointPx::new(self.max.x_px.max(other.max.x_px), self.max.y_px.max(other.max.y_px)),
    )
  }

  pub fn contains_rect(self, other: Self) -> bool {
    other.min.x_px >= self.min.x_px - GEOMETRY_EPSILON
      && other.min.y_px >= self.min.y_px - GEOMETRY_EPSILON
      && other.max.x_px <= self.max.x_px + GEOMETRY_EPSILON
      && other.max.y_px <= self.max.y_px + GEOMETRY_EPSILON
  }

  pub fn intersects(self, other: Self) -> bool {
    self.min.x_px <= other.max.x_px
      && self.max.x_px >= other.min.x_px
      && self.min.y_px <= other.max.y_px
      && self.max.y_px >= other.min.y_px
  }

  pub fn translation_to_fit(self, canvas_size: SizePx) -> Result<PointPx, GeometryError> {
    canvas_size.validate()?;
    let canvas = canvas_size.bounds();
    if self.width() > canvas.width() + GEOMETRY_EPSILON
      || self.height() > canvas.height() + GEOMETRY_EPSILON
    {
      return Err(GeometryError::GeometryTooLargeForCanvas);
    }

    let x_px = if self.min.x_px < canvas.min.x_px {
      canvas.min.x_px - self.min.x_px
    } else if self.max.x_px > canvas.max.x_px {
      canvas.max.x_px - self.max.x_px
    } else {
      0.0
    };
    let y_px = if self.min.y_px < canvas.min.y_px {
      canvas.min.y_px - self.min.y_px
    } else if self.max.y_px > canvas.max.y_px {
      canvas.max.y_px - self.max.y_px
    } else {
      0.0
    };
    Ok(PointPx::new(x_px, y_px))
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokeProcessingOptions {
  pub distance_filter_px: f32,
  pub simplify_tolerance_px: f32,
}

impl StrokeProcessingOptions {
  pub fn for_line_width(width_px: f32) -> Result<Self, GeometryError> {
    if !width_px.is_finite() || width_px <= 0.0 {
      return Err(GeometryError::InvalidDimension);
    }
    Ok(Self {
      distance_filter_px: (width_px * 0.25).max(1.0),
      simplify_tolerance_px: (width_px * 0.2).max(0.5),
    })
  }
}

pub fn minimum_geometry_extent(width_px: f32) -> Result<f32, GeometryError> {
  if !width_px.is_finite() || width_px <= 0.0 {
    return Err(GeometryError::InvalidDimension);
  }
  Ok((width_px * 1.5).max(2.0))
}

pub fn process_stroke_points(
  points: &[PointPx],
  width_px: f32,
) -> Result<Vec<PointPx>, GeometryError> {
  if points.iter().any(|point| !point.is_finite()) {
    return Err(GeometryError::NonFiniteCoordinate);
  }
  if points.len() < 2 {
    return Err(GeometryError::TooFewPoints);
  }
  let options = StrokeProcessingOptions::for_line_width(width_px)?;
  let filtered = distance_filter(points, options.distance_filter_px);
  let simplified = simplify_ramer_douglas_peucker(&filtered, options.simplify_tolerance_px);
  Ok(smooth_points(&simplified))
}

fn distance_filter(points: &[PointPx], minimum_distance: f32) -> Vec<PointPx> {
  let mut filtered = Vec::with_capacity(points.len());
  filtered.push(points[0]);
  let mut last_kept = points[0];
  for point in &points[1..points.len() - 1] {
    if last_kept.distance_to(*point) >= minimum_distance {
      filtered.push(*point);
      last_kept = *point;
    }
  }
  let last = points[points.len() - 1];
  if filtered.last().copied() != Some(last) {
    filtered.push(last);
  }
  filtered
}

fn simplify_ramer_douglas_peucker(points: &[PointPx], tolerance: f32) -> Vec<PointPx> {
  if points.len() <= 2 {
    return points.to_vec();
  }

  let mut keep = vec![false; points.len()];
  keep[0] = true;
  keep[points.len() - 1] = true;
  let mut ranges = vec![(0usize, points.len() - 1)];
  while let Some((start, end)) = ranges.pop() {
    if end <= start + 1 {
      continue;
    }
    let mut maximum_distance = -1.0f32;
    let mut maximum_index = start + 1;
    for index in start + 1..end {
      let distance = perpendicular_distance(points[index], points[start], points[end]);
      if distance > maximum_distance {
        maximum_distance = distance;
        maximum_index = index;
      }
    }
    if maximum_distance > tolerance {
      keep[maximum_index] = true;
      ranges.push((maximum_index, end));
      ranges.push((start, maximum_index));
    }
  }

  points.iter().zip(keep).filter_map(|(point, keep)| keep.then_some(*point)).collect()
}

fn perpendicular_distance(point: PointPx, start: PointPx, end: PointPx) -> f32 {
  let length = start.distance_to(end);
  if length <= f32::EPSILON {
    return point.distance_to(start);
  }
  let numerator = ((end.y_px - start.y_px) * point.x_px - (end.x_px - start.x_px) * point.y_px
    + end.x_px * start.y_px
    - end.y_px * start.x_px)
    .abs();
  numerator / length
}

fn smooth_points(points: &[PointPx]) -> Vec<PointPx> {
  if points.len() <= 2 {
    return points.to_vec();
  }
  let mut smoothed = Vec::with_capacity(points.len());
  smoothed.push(points[0]);
  for window in points.windows(3) {
    smoothed.push(PointPx::new(
      (window[0].x_px + 2.0 * window[1].x_px + window[2].x_px) / 4.0,
      (window[0].y_px + 2.0 * window[1].y_px + window[2].y_px) / 4.0,
    ));
  }
  smoothed.push(points[points.len() - 1]);
  smoothed
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum GeometryError {
  #[error("canvas dimensions must be non-zero")]
  EmptyCanvas,
  #[error("canvas {width_px}x{height_px} exceeds the 8K limit")]
  CanvasTooLarge { width_px: u32, height_px: u32 },
  #[error("coordinate must be finite")]
  NonFiniteCoordinate,
  #[error("bounds are inverted")]
  InvertedBounds,
  #[error("dimension must be finite and positive")]
  InvalidDimension,
  #[error("geometry is larger than the canvas")]
  GeometryTooLargeForCanvas,
  #[error("at least two points are required")]
  TooFewPoints,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn translation_to_fit_handles_each_canvas_edge() {
    let canvas = SizePx::new(100, 80);
    let bounds = RectPx::from_min_max(PointPx::new(-10.0, 70.0), PointPx::new(20.0, 90.0));
    let delta = bounds.translation_to_fit(canvas).unwrap();
    assert_eq!(delta, PointPx::new(10.0, -10.0));
    assert!(canvas.bounds().contains_rect(bounds.translated(delta)));
  }

  #[test]
  fn stroke_processing_is_deterministic_and_keeps_endpoints() {
    let points = vec![
      PointPx::new(0.0, 0.0),
      PointPx::new(0.1, 0.1),
      PointPx::new(4.0, 2.0),
      PointPx::new(8.0, 0.0),
    ];
    let first = process_stroke_points(&points, 4.0).unwrap();
    let second = process_stroke_points(&points, 4.0).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.first(), points.first());
    assert_eq!(first.last(), points.last());
  }

  #[test]
  fn non_finite_stroke_is_rejected() {
    let points = [PointPx::ZERO, PointPx::new(f32::NAN, 1.0)];
    assert_eq!(process_stroke_points(&points, 4.0), Err(GeometryError::NonFiniteCoordinate));
  }
}
