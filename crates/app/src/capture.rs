use std::{
  fmt,
  sync::Arc,
  time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
#[cfg(not(target_os = "macos"))]
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
pub const CAPTURE_DEADLINE: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub struct NativeCaptureImage {
  #[cfg(target_os = "macos")]
  image: objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>,
  #[cfg(not(target_os = "macos"))]
  image: Arc<RgbaImage>,
}

impl NativeCaptureImage {
  #[cfg(target_os = "macos")]
  pub(crate) fn from_cg_image(
    image: objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>,
  ) -> Result<Self, CaptureError> {
    let pixel_size = [
      u32::try_from(objc2_core_graphics::CGImage::width(Some(&image)))
        .map_err(|_| CaptureError::CaptureFailed("captured image width is too large".into()))?,
      u32::try_from(objc2_core_graphics::CGImage::height(Some(&image)))
        .map_err(|_| CaptureError::CaptureFailed("captured image height is too large".into()))?,
    ];
    validate_pixel_dimensions(pixel_size)?;
    Ok(Self { image })
  }

  #[cfg(not(target_os = "macos"))]
  fn from_rgba(image: RgbaImage) -> Result<Self, CaptureError> {
    validate_pixel_dimensions([image.width(), image.height()])?;
    Ok(Self { image: Arc::new(image) })
  }

  pub fn pixel_size(&self) -> [u32; 2] {
    #[cfg(target_os = "macos")]
    {
      [
        u32::try_from(objc2_core_graphics::CGImage::width(Some(&self.image))).unwrap_or(u32::MAX),
        u32::try_from(objc2_core_graphics::CGImage::height(Some(&self.image))).unwrap_or(u32::MAX),
      ]
    }
    #[cfg(not(target_os = "macos"))]
    {
      [self.image.width(), self.image.height()]
    }
  }

  #[cfg(target_os = "macos")]
  pub(crate) fn cg_image(&self) -> &objc2_core_graphics::CGImage {
    &self.image
  }

  pub fn to_rgba8(&self) -> Result<Arc<[u8]>, CaptureError> {
    #[cfg(target_os = "macos")]
    {
      use objc2_core_graphics::{
        CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGImageByteOrderInfo,
      };

      let [width_px, height_px] = self.pixel_size();
      let bytes_per_row = (width_px as usize)
        .checked_mul(4)
        .ok_or_else(|| CaptureError::CaptureFailed("captured image row is too large".into()))?;
      let byte_len = bytes_per_row
        .checked_mul(height_px as usize)
        .ok_or_else(|| CaptureError::CaptureFailed("captured image buffer is too large".into()))?;
      let mut pixels = vec![0_u8; byte_len];
      let color_space = CGColorSpace::new_device_rgb()
        .ok_or_else(|| CaptureError::CaptureFailed("creating RGB color space failed".into()))?;
      let bitmap_info = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
      let bitmap = unsafe {
        CGBitmapContextCreate(
          pixels.as_mut_ptr().cast(),
          width_px as usize,
          height_px as usize,
          8,
          bytes_per_row,
          Some(&color_space),
          bitmap_info,
        )
      }
      .ok_or_else(|| CaptureError::CaptureFailed("creating RGBA bitmap context failed".into()))?;
      CGContext::translate_ctm(Some(&bitmap), 0.0, height_px as f64);
      CGContext::scale_ctm(Some(&bitmap), 1.0, -1.0);
      CGContext::draw_image(
        Some(&bitmap),
        objc2_core_foundation::CGRect::new(
          objc2_core_foundation::CGPoint::ZERO,
          objc2_core_foundation::CGSize::new(width_px as f64, height_px as f64),
        ),
        Some(&self.image),
      );
      drop(bitmap);
      for pixel in pixels.chunks_exact_mut(4) {
        let alpha = pixel[3];
        if alpha != 0 && alpha != u8::MAX {
          for component in &mut pixel[..3] {
            *component = ((*component as u32 * u8::MAX as u32 + alpha as u32 / 2) / alpha as u32)
              .min(u8::MAX as u32) as u8;
          }
        }
      }
      Ok(Arc::from(pixels))
    }
    #[cfg(not(target_os = "macos"))]
    {
      Ok(Arc::from(self.image.as_raw().clone()))
    }
  }
}

impl fmt::Debug for NativeCaptureImage {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("NativeCaptureImage").field("pixel_size", &self.pixel_size()).finish()
  }
}

/// An unencoded screen capture ready to be handed to the editor.
///
/// `display_bounds_global` is `[x, y, width, height]` in global logical
/// display coordinates. `pixel_size` describes the retained native image.
#[derive(Clone)]
pub struct CaptureFrame {
  pub request_id: Uuid,
  pub capture_sequence: u64,
  pub display_id: u32,
  pub image: NativeCaptureImage,
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
    image: NativeCaptureImage,
    captured_at: DateTime<Utc>,
  ) -> Result<Self, CaptureError> {
    let frame = Self {
      request_id,
      capture_sequence,
      display_id: display.display_id,
      pixel_size: image.pixel_size(),
      image,
      display_bounds_global: display.bounds_global,
      scale_factor: display.scale_factor,
      captured_at,
    };
    frame.validate()?;
    Ok(frame)
  }

  pub fn validate(&self) -> Result<(), CaptureError> {
    validate_pixel_dimensions(self.pixel_size)?;

    if self.image.pixel_size() != self.pixel_size {
      return Err(CaptureError::CaptureFailed(format!(
        "native image dimensions {:?} do not match frame dimensions {:?}",
        self.image.pixel_size(),
        self.pixel_size,
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
      .field("image", &self.image)
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

pub fn prewarm_capture_backend() {
  #[cfg(target_os = "macos")]
  macos::prewarm();
}

pub fn invalidate_capture_backend_cache() {
  #[cfg(target_os = "macos")]
  macos::invalidate_cached_content();
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
  let deadline = Instant::now() + CAPTURE_DEADLINE;
  ensure_screen_recording_permission()?;
  let monitor = cursor_position
    .and_then(|[x, y]| Monitor::from_point(x, y).ok())
    .map(Ok)
    .unwrap_or_else(primary_monitor)?;

  let display = DisplaySnapshot::from_monitor(&monitor)
    .map_err(|error| CaptureError::CaptureFailed(error.to_string()))?;
  Ok(PreparedCapture { monitor, display, options, deadline })
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
  capture_monitor(
    request_id,
    capture_sequence,
    PreparedCapture { monitor, display, options, deadline: Instant::now() + CAPTURE_DEADLINE },
  )
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
  deadline: Instant,
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
    prepared.deadline,
  )?;

  CaptureFrame::new(request_id, capture_sequence, prepared.display, image, Utc::now())
}

#[cfg(target_os = "macos")]
fn capture_monitor_image(
  monitor: &Monitor,
  expected_pixel_size: [u32; 2],
  include_cursor: bool,
  performance: PerformanceContext,
  deadline: Instant,
) -> Result<NativeCaptureImage, CaptureError> {
  let display_id = monitor.id().map_err(map_xcap_error)?;
  macos::capture_display(display_id, expected_pixel_size, include_cursor, performance, deadline)
}

#[cfg(not(target_os = "macos"))]
fn capture_monitor_image(
  monitor: &Monitor,
  _expected_pixel_size: [u32; 2],
  _include_cursor: bool,
  _performance: PerformanceContext,
  _deadline: Instant,
) -> Result<NativeCaptureImage, CaptureError> {
  NativeCaptureImage::from_rgba(monitor.capture_image().map_err(map_xcap_error)?)
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

  #[cfg(target_os = "macos")]
  #[test]
  fn materializes_unpremultiplied_rgba_only_on_demand() {
    use objc2_core_graphics::{
      CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGImageAlphaInfo,
      CGImageByteOrderInfo,
    };

    let mut premultiplied = [128_u8, 0, 0, 128, 0, 255, 0, 255];
    let color_space = CGColorSpace::new_device_rgb().unwrap();
    let bitmap = unsafe {
      CGBitmapContextCreate(
        premultiplied.as_mut_ptr().cast(),
        2,
        1,
        8,
        8,
        Some(&color_space),
        CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
      )
    }
    .unwrap();
    let image = CGBitmapContextCreateImage(Some(&bitmap)).unwrap();
    let native = NativeCaptureImage::from_cg_image(image).unwrap();

    assert_eq!(&*native.to_rgba8().unwrap(), &[255, 0, 0, 128, 0, 255, 0, 255]);
  }
}
