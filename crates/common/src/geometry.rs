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
  if points.is_empty() {
    return Err(GeometryError::TooFewPoints);
  }
  if !width_px.is_finite() || width_px <= 0.0 {
    return Err(GeometryError::InvalidDimension);
  }
  Ok(points.to_vec())
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
  #[error("at least one point is required")]
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
  fn stroke_processing_preserves_every_input_point() {
    let points = vec![
      PointPx::new(0.0, 0.0),
      PointPx::new(0.1, 0.1),
      PointPx::new(4.0, 2.0),
      PointPx::new(8.0, 0.0),
    ];
    assert_eq!(process_stroke_points(&points, 4.0).unwrap(), points);
  }

  #[test]
  fn stroke_processing_accepts_a_single_point() {
    let points = [PointPx::new(4.0, 2.0)];
    assert_eq!(process_stroke_points(&points, 4.0).unwrap(), points);
  }

  #[test]
  fn non_finite_stroke_is_rejected() {
    let points = [PointPx::ZERO, PointPx::new(f32::NAN, 1.0)];
    assert_eq!(process_stroke_points(&points, 4.0), Err(GeometryError::NonFiniteCoordinate));
  }
}
