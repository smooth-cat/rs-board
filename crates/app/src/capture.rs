use std::fmt;

use chrono::{DateTime, Utc};
use image::RgbaImage;
use thiserror::Error;
use uuid::Uuid;
use xcap::Monitor;

use crate::capture_surface::DisplaySnapshot;
use crate::performance::{PerformanceContext, PerformanceDetails, PerformanceTimer};
use crate::platform::global_cursor_position;

#[cfg(target_os = "macos")]
mod macos;

pub const MAX_CAPTURE_DIMENSION_PX: u32 = 8_192;

/// An unencoded screen capture ready to be handed to the editor.
///
/// `display_bounds_global` is `[x, y, width, height]` in global logical
/// display coordinates. `pixel_size` describes the physical RGBA buffer.
#[derive(Clone, PartialEq)]
pub struct CaptureFrame {
  pub request_id: Uuid,
  pub capture_sequence: u64,
  pub display_id: u32,
  pub rgba_pixels: Vec<u8>,
  pub pixel_size: [u32; 2],
  pub display_bounds_global: [i32; 4],
  pub scale_factor: f32,
  pub captured_at: DateTime<Utc>,
}

impl CaptureFrame {
  pub fn new(
    request_id: Uuid,
    capture_sequence: u64,
    display: DisplaySnapshot,
    rgba_pixels: Vec<u8>,
    pixel_size: [u32; 2],
    captured_at: DateTime<Utc>,
  ) -> Result<Self, CaptureError> {
    let frame = Self {
      request_id,
      capture_sequence,
      display_id: display.display_id,
      rgba_pixels,
      pixel_size,
      display_bounds_global: display.bounds_global,
      scale_factor: display.scale_factor,
      captured_at,
    };
    frame.validate()?;
    Ok(frame)
  }

  pub fn validate(&self) -> Result<(), CaptureError> {
    validate_pixel_dimensions(self.pixel_size)?;

    let expected_len = self.pixel_size[0] as usize * self.pixel_size[1] as usize * 4;
    if self.rgba_pixels.len() != expected_len {
      return Err(CaptureError::CaptureFailed(format!(
        "invalid RGBA buffer length: expected {expected_len}, got {}",
        self.rgba_pixels.len()
      )));
    }

    if self.display_bounds_global[2] <= 0 || self.display_bounds_global[3] <= 0 {
      return Err(CaptureError::CaptureFailed(
        "display bounds must have positive width and height".to_owned(),
      ));
    }

    if !self.scale_factor.is_finite() || self.scale_factor <= 0.0 {
      return Err(CaptureError::CaptureFailed(
        "display scale factor must be finite and positive".to_owned(),
      ));
    }

    Ok(())
  }
}

impl fmt::Debug for CaptureFrame {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("CaptureFrame")
      .field("request_id", &self.request_id)
      .field("capture_sequence", &self.capture_sequence)
      .field("display_id", &self.display_id)
      .field("rgba_bytes", &self.rgba_pixels.len())
      .field("pixel_size", &self.pixel_size)
      .field("display_bounds_global", &self.display_bounds_global)
      .field("scale_factor", &self.scale_factor)
      .field("captured_at", &self.captured_at)
      .finish()
  }
}

#[derive(Debug, Error)]
pub enum CaptureError {
  #[error("screen recording permission was denied")]
  PermissionDenied,
  #[error("screen capture failed: {0}")]
  CaptureFailed(String),
  #[error("no active display is available")]
  NoDisplay,
  #[error(
    "captured image is too large ({width_px}x{height_px}); maximum dimension is {max_dimension_px}px"
  )]
  TooLarge { width_px: u32, height_px: u32, max_dimension_px: u32 },
}

/// Captures the complete display containing the current mouse cursor.
///
/// If the global cursor position cannot be queried, or no display contains
/// that point, this falls back to the primary display.
pub fn capture_display_under_cursor(
  request_id: Uuid,
  capture_sequence: u64,
) -> Result<CaptureFrame, CaptureError> {
  capture_display_under_cursor_with_options(request_id, capture_sequence, CaptureOptions::default())
}

pub fn capture_display_under_cursor_with_options(
  request_id: Uuid,
  capture_sequence: u64,
  options: CaptureOptions,
) -> Result<CaptureFrame, CaptureError> {
  capture_prepared_display(
    request_id,
    capture_sequence,
    prepare_display_capture_under_cursor(options)?,
  )
}

/// Checks macOS Screen Recording access and asks the system to prompt when possible.
///
/// The app calls this once during startup so first-run users see the permission
/// prompt before they try F1. Capture entry points still call the same check so
/// denied users get another OS-level request attempt each time they retry.
pub fn request_screen_recording_permission() -> Result<(), CaptureError> {
  ensure_screen_recording_permission()
}

pub fn prepare_display_capture_under_cursor(
  options: CaptureOptions,
) -> Result<PreparedCapture, CaptureError> {
  prepare_display_capture_under_cursor_at(options, global_cursor_position())
}

pub fn prepare_display_capture_under_cursor_at(
  options: CaptureOptions,
  cursor_position: Option<[i32; 2]>,
) -> Result<PreparedCapture, CaptureError> {
  ensure_screen_recording_permission()?;
  let monitor = cursor_position
    .and_then(|[x, y]| Monitor::from_point(x, y).ok())
    .map(Ok)
    .unwrap_or_else(primary_monitor)?;

  let display = DisplaySnapshot::from_monitor(&monitor)
    .map_err(|error| CaptureError::CaptureFailed(error.to_string()))?;
  Ok(PreparedCapture { monitor, display, options })
}

pub fn capture_prepared_display(
  request_id: Uuid,
  capture_sequence: u64,
  prepared: PreparedCapture,
) -> Result<CaptureFrame, CaptureError> {
  let performance = PerformanceContext::capture(request_id, capture_sequence);
  let timer =
    PerformanceTimer::start("capture.api.total", performance, PerformanceDetails::default());
  let result = capture_monitor(request_id, capture_sequence, prepared);
  match &result {
    Ok(_) => timer.finish_ok(),
    Err(error) => timer.finish_error(error),
  }
  result
}

/// Captures the complete primary display.
pub fn capture_primary_display(
  request_id: Uuid,
  capture_sequence: u64,
) -> Result<CaptureFrame, CaptureError> {
  capture_primary_display_with_options(request_id, capture_sequence, CaptureOptions::default())
}

pub fn capture_primary_display_with_options(
  request_id: Uuid,
  capture_sequence: u64,
  options: CaptureOptions,
) -> Result<CaptureFrame, CaptureError> {
  ensure_screen_recording_permission()?;
  let monitor = primary_monitor()?;
  let display = DisplaySnapshot::from_monitor(&monitor)
    .map_err(|error| CaptureError::CaptureFailed(error.to_string()))?;
  capture_monitor(request_id, capture_sequence, PreparedCapture { monitor, display, options })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureOptions {
  pub include_cursor: bool,
}

#[derive(Debug, Clone)]
pub struct PreparedCapture {
  monitor: Monitor,
  display: DisplaySnapshot,
  options: CaptureOptions,
}

impl PreparedCapture {
  pub fn display(&self) -> DisplaySnapshot {
    self.display
  }
}

pub fn validate_pixel_dimensions(pixel_size: [u32; 2]) -> Result<(), CaptureError> {
  let [width_px, height_px] = pixel_size;
  if width_px == 0 || height_px == 0 {
    return Err(CaptureError::CaptureFailed(
      "captured image dimensions must be non-zero".to_owned(),
    ));
  }
  if width_px > MAX_CAPTURE_DIMENSION_PX || height_px > MAX_CAPTURE_DIMENSION_PX {
    return Err(CaptureError::TooLarge {
      width_px,
      height_px,
      max_dimension_px: MAX_CAPTURE_DIMENSION_PX,
    });
  }
  Ok(())
}

fn primary_monitor() -> Result<Monitor, CaptureError> {
  let monitors = Monitor::all().map_err(map_xcap_error)?;
  if monitors.is_empty() {
    return Err(CaptureError::NoDisplay);
  }

  for monitor in &monitors {
    if monitor.is_primary().unwrap_or(false) {
      return Ok(monitor.clone());
    }
  }

  // Some backends do not report a primary flag. Their first active monitor
  // is the most useful deterministic fallback.
  monitors.into_iter().next().ok_or(CaptureError::NoDisplay)
}

fn capture_monitor(
  request_id: Uuid,
  capture_sequence: u64,
  prepared: PreparedCapture,
) -> Result<CaptureFrame, CaptureError> {
  let performance = PerformanceContext::capture(request_id, capture_sequence);
  let DisplaySnapshot { bounds_global, scale_factor, .. } = prepared.display;
  let [_, _, bounds_width, bounds_height] = bounds_global;
  let logical_width = u32::try_from(bounds_width).map_err(|_| {
    CaptureError::CaptureFailed("display width does not fit capture dimensions".to_owned())
  })?;
  let logical_height = u32::try_from(bounds_height).map_err(|_| {
    CaptureError::CaptureFailed("display height does not fit capture dimensions".to_owned())
  })?;

  validate_pixel_dimensions([logical_width, logical_height])?;
  validate_scale_factor(scale_factor)?;
  let expected_pixel_size =
    expected_capture_pixel_size(logical_width, logical_height, scale_factor)?;

  let image = capture_monitor_image(
    &prepared.monitor,
    expected_pixel_size,
    prepared.options.include_cursor,
    performance,
  )?;
  let pixel_size = [image.width(), image.height()];

  CaptureFrame::new(
    request_id,
    capture_sequence,
    prepared.display,
    image.into_raw(),
    pixel_size,
    Utc::now(),
  )
}

#[cfg(target_os = "macos")]
fn capture_monitor_image(
  monitor: &Monitor,
  expected_pixel_size: [u32; 2],
  include_cursor: bool,
  performance: PerformanceContext,
) -> Result<RgbaImage, CaptureError> {
  let display_id = monitor.id().map_err(map_xcap_error)?;
  macos::capture_display(display_id, expected_pixel_size, include_cursor, performance)
}

#[cfg(not(target_os = "macos"))]
fn capture_monitor_image(
  monitor: &Monitor,
  _expected_pixel_size: [u32; 2],
  _include_cursor: bool,
  _performance: PerformanceContext,
) -> Result<RgbaImage, CaptureError> {
  monitor.capture_image().map_err(map_xcap_error)
}

fn validate_scale_factor(scale_factor: f32) -> Result<(), CaptureError> {
  if scale_factor.is_finite() && scale_factor > 0.0 {
    Ok(())
  } else {
    Err(CaptureError::CaptureFailed("display scale factor must be finite and positive".to_owned()))
  }
}

#[cfg(target_os = "macos")]
fn expected_capture_pixel_size(
  logical_width: u32,
  logical_height: u32,
  scale_factor: f32,
) -> Result<[u32; 2], CaptureError> {
  let width_px = (logical_width as f64 * scale_factor as f64).round();
  let height_px = (logical_height as f64 * scale_factor as f64).round();
  if width_px > MAX_CAPTURE_DIMENSION_PX as f64 || height_px > MAX_CAPTURE_DIMENSION_PX as f64 {
    return Err(CaptureError::TooLarge {
      width_px: width_px.min(u32::MAX as f64) as u32,
      height_px: height_px.min(u32::MAX as f64) as u32,
      max_dimension_px: MAX_CAPTURE_DIMENSION_PX,
    });
  }
  let pixel_size = [width_px as u32, height_px as u32];
  validate_pixel_dimensions(pixel_size)?;
  Ok(pixel_size)
}

#[cfg(not(target_os = "macos"))]
fn expected_capture_pixel_size(
  logical_width: u32,
  logical_height: u32,
  _scale_factor: f32,
) -> Result<[u32; 2], CaptureError> {
  Ok([logical_width, logical_height])
}

fn map_xcap_error(error: xcap::XCapError) -> CaptureError {
  let message = error.to_string();
  if message.to_ascii_lowercase().contains("permission") {
    CaptureError::PermissionDenied
  } else {
    CaptureError::CaptureFailed(message)
  }
}

#[cfg(target_os = "macos")]
fn ensure_screen_recording_permission() -> Result<(), CaptureError> {
  use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

  if CGPreflightScreenCaptureAccess() || CGRequestScreenCaptureAccess() {
    Ok(())
  } else {
    Err(CaptureError::PermissionDenied)
  }
}

#[cfg(not(target_os = "macos"))]
fn ensure_screen_recording_permission() -> Result<(), CaptureError> {
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn valid_frame(pixel_size: [u32; 2], rgba_pixels: Vec<u8>) -> CaptureFrame {
    CaptureFrame {
      request_id: Uuid::nil(),
      capture_sequence: 1,
      display_id: 1,
      rgba_pixels,
      pixel_size,
      display_bounds_global: [0, 0, 100, 100],
      scale_factor: 1.0,
      captured_at: Utc::now(),
    }
  }

  #[test]
  fn accepts_dimensions_at_8k_limit() {
    assert!(
      validate_pixel_dimensions([MAX_CAPTURE_DIMENSION_PX, MAX_CAPTURE_DIMENSION_PX]).is_ok()
    );
  }

  #[test]
  fn rejects_either_dimension_over_8k_limit() {
    for pixel_size in [[MAX_CAPTURE_DIMENSION_PX + 1, 1], [1, MAX_CAPTURE_DIMENSION_PX + 1]] {
      assert!(matches!(validate_pixel_dimensions(pixel_size), Err(CaptureError::TooLarge { .. })));
    }
  }

  #[test]
  fn rejects_invalid_rgba_buffer_length() {
    let frame = valid_frame([2, 2], vec![0; 15]);
    assert!(matches!(
        frame.validate(),
        Err(CaptureError::CaptureFailed(message))
            if message.contains("invalid RGBA buffer length")
    ));
  }

  #[test]
  fn accepts_matching_rgba_buffer_length() {
    let frame = valid_frame([2, 2], vec![0; 16]);
    assert!(frame.validate().is_ok());
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn computes_retina_capture_size_from_logical_bounds() {
    assert_eq!(expected_capture_pixel_size(1_728, 1_117, 2.0).unwrap(), [3_456, 2_234]);
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn rounds_scale_factor_noise_to_physical_pixels() {
    assert_eq!(expected_capture_pixel_size(1_920, 1_080, 1.999_999_9).unwrap(), [3_840, 2_160]);
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn rejects_expected_retina_size_over_8k_limit() {
    assert!(matches!(
      expected_capture_pixel_size(4_097, 2_160, 2.0),
      Err(CaptureError::TooLarge { .. })
    ));
  }
}
