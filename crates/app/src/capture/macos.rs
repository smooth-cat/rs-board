use std::{
  ptr::NonNull,
  sync::{
    Mutex, MutexGuard, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, RecvTimeoutError, sync_channel},
  },
  time::{Duration, Instant},
};

use block2::RcBlock;
use objc2::{
  AnyThread,
  rc::{Retained, autoreleasepool},
  runtime::AnyClass,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
  CGImage, CGWindowImageOption, CGWindowListOption, kCGColorSpaceSRGB, kCGNullWindowID,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
  SCContentFilter, SCDisplay, SCRunningApplication, SCScreenshotManager, SCShareableContent,
  SCStreamConfiguration, SCStreamErrorCode, SCStreamErrorDomain, SCWindow,
};

use super::{CaptureError, NativeCaptureImage};
use crate::performance::{PerformanceContext, PerformanceDetails, PerformanceTimer};

static CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);
static SHAREABLE_CONTENT: OnceLock<Mutex<Option<ShareableContentSnapshot>>> = OnceLock::new();

#[derive(Clone)]
struct ShareableContentSnapshot(Retained<SCShareableContent>);

// SAFETY: SCShareableContent is an immutable framework snapshot. Access to the
// cached retained reference is serialized and consumers only read it.
unsafe impl Send for ShareableContentSnapshot {}
unsafe impl Sync for ShareableContentSnapshot {}

struct CaptureGate;

impl CaptureGate {
  fn claim() -> Result<Self, CaptureError> {
    CAPTURE_ACTIVE
      .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
      .map(|_| Self)
      .map_err(|_| CaptureError::CaptureFailed("another capture request is already active".into()))
  }
}

impl Drop for CaptureGate {
  fn drop(&mut self) {
    CAPTURE_ACTIVE.store(false, Ordering::Release);
  }
}

pub(super) fn prewarm() {
  if lock_unpoisoned(content_cache()).is_some() {
    return;
  }
  let _ = std::thread::Builder::new().name("capture-prewarm".into()).spawn(|| {
    let deadline = Instant::now() + Duration::from_secs(10);
    if let Ok(content) = fetch_shareable_content(deadline) {
      *lock_unpoisoned(content_cache()) = Some(content);
    }
  });
}

pub(super) fn invalidate_cached_content() {
  *lock_unpoisoned(content_cache()) = None;
  prewarm();
}

pub(super) fn capture_display(
  display_id: u32,
  expected_pixel_size: [u32; 2],
  include_cursor: bool,
  performance: PerformanceContext,
  deadline: Instant,
) -> Result<NativeCaptureImage, CaptureError> {
  let _gate = CaptureGate::claim()?;
  ensure_time_remaining(deadline, "starting capture")?;
  let details =
    PerformanceDetails::default().display_id(display_id).pixel_size(expected_pixel_size);
  let timer = PerformanceTimer::start("capture.macos_backend.total", performance, details);
  let result = autoreleasepool(|_| {
    let image = if screenshot_manager_available() {
      capture_with_screenshot_manager(
        display_id,
        expected_pixel_size,
        include_cursor,
        performance,
        details,
        deadline,
      )
    } else {
      capture_with_core_graphics(display_id, expected_pixel_size, include_cursor, deadline)
    }?;
    ensure_time_remaining(deadline, "validating capture result")?;
    let image = NativeCaptureImage::from_cg_image(image)?;
    if image.pixel_size() != expected_pixel_size {
      return Err(CaptureError::CaptureFailed(format!(
        "capture returned {}x{} pixels; expected {}x{}",
        image.pixel_size()[0],
        image.pixel_size()[1],
        expected_pixel_size[0],
        expected_pixel_size[1]
      )));
    }
    Ok(image)
  });
  match &result {
    Ok(_) => timer.finish_ok(),
    Err(error) => timer.finish_error(error),
  }
  result
}

fn screenshot_manager_available() -> bool {
  AnyClass::get(c"SCScreenshotManager").is_some()
}

fn capture_with_screenshot_manager(
  display_id: u32,
  expected_pixel_size: [u32; 2],
  include_cursor: bool,
  performance: PerformanceContext,
  details: PerformanceDetails,
  deadline: Instant,
) -> Result<CFRetained<CGImage>, CaptureError> {
  let content_timer = PerformanceTimer::start("capture.shareable_content", performance, details);
  let content_result = cached_shareable_content(deadline);
  match &content_result {
    Ok(_) => content_timer.finish_ok(),
    Err(error) => content_timer.finish_error(error),
  }
  let content = content_result?;

  let setup_timer = PerformanceTimer::start("capture.screenshot.setup", performance, details);
  let setup_result = (|| {
    let display = find_display(&content.0, display_id)?;
    let filter = content_filter(&content.0, &display)?;
    let configuration = screenshot_configuration(expected_pixel_size, include_cursor);
    Ok::<_, CaptureError>((filter, configuration))
  })();
  match &setup_result {
    Ok(_) => setup_timer.finish_ok(),
    Err(error) => setup_timer.finish_error(error),
  }
  let (filter, configuration) = setup_result?;

  let (sender, receiver) = sync_channel(1);
  let completion = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
    let result = NonNull::new(image)
      .map(|image| unsafe { CFRetained::retain(image) })
      .ok_or_else(|| capture_error_from_optional_ns_error(error, "capturing image"));
    let _ = sender.try_send(result);
  });
  let capture_timer = PerformanceTimer::start("capture.screenshot.wait", performance, details);
  unsafe {
    SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
      &filter,
      &configuration,
      Some(&completion),
    );
  }
  let result = receive_until(receiver, deadline, "capturing image");
  match &result {
    Ok(_) => capture_timer.finish_ok(),
    Err(error) => capture_timer.finish_error(error),
  }
  result
}

#[allow(deprecated)]
fn capture_with_core_graphics(
  display_id: u32,
  expected_pixel_size: [u32; 2],
  include_cursor: bool,
  deadline: Instant,
) -> Result<CFRetained<CGImage>, CaptureError> {
  use objc2_core_graphics::{CGDisplayBounds, CGWindowListCreateImage};

  ensure_time_remaining(deadline, "starting CoreGraphics capture")?;
  let bounds = CGDisplayBounds(display_id);
  #[allow(deprecated)]
  let image = CGWindowListCreateImage(
    bounds,
    CGWindowListOption::OptionOnScreenOnly,
    kCGNullWindowID,
    CGWindowImageOption::BestResolution | CGWindowImageOption::ShouldBeOpaque,
  )
  .ok_or_else(|| CaptureError::CaptureFailed("CoreGraphics returned no image".into()))?;
  let image =
    if include_cursor { composite_cursor(image, bounds, expected_pixel_size)? } else { image };
  ensure_time_remaining(deadline, "finishing CoreGraphics capture")?;
  Ok(image)
}

#[allow(deprecated)]
fn composite_cursor(
  base_image: CFRetained<CGImage>,
  display_bounds: objc2_core_foundation::CGRect,
  [width_px, height_px]: [u32; 2],
) -> Result<CFRetained<CGImage>, CaptureError> {
  use std::ptr;

  use objc2_app_kit::NSCursor;
  use objc2_core_foundation::{CGPoint, CGRect, CGSize};
  use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGEvent,
    CGImageAlphaInfo, CGImageByteOrderInfo,
  };

  let cursor = NSCursor::currentSystemCursor()
    .ok_or_else(|| CaptureError::CaptureFailed("current system cursor is unavailable".into()))?;
  let cursor_image =
    unsafe { cursor.image().CGImageForProposedRect_context_hints(ptr::null_mut(), None, None) }
      .ok_or_else(|| {
        CaptureError::CaptureFailed("current cursor has no CoreGraphics image".into())
      })?;
  let pointer = CGEvent::new(None)
    .map(|event| CGEvent::location(Some(&event)))
    .ok_or_else(|| CaptureError::CaptureFailed("current cursor position is unavailable".into()))?;

  let color_space = CGColorSpace::new_device_rgb().ok_or_else(|| {
    CaptureError::CaptureFailed("creating cursor composite color space failed".into())
  })?;
  let bytes_per_row = (width_px as usize)
    .checked_mul(4)
    .ok_or_else(|| CaptureError::CaptureFailed("cursor composite row is too large".into()))?;
  let bitmap_info = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
  let bitmap = unsafe {
    CGBitmapContextCreate(
      ptr::null_mut(),
      width_px as usize,
      height_px as usize,
      8,
      bytes_per_row,
      Some(&color_space),
      bitmap_info,
    )
  }
  .ok_or_else(|| CaptureError::CaptureFailed("creating cursor composite context failed".into()))?;

  let image_bounds = CGRect::new(CGPoint::ZERO, CGSize::new(width_px as f64, height_px as f64));
  CGContext::draw_image(Some(&bitmap), image_bounds, Some(&base_image));

  let scale_x = width_px as f64 / display_bounds.size.width;
  let scale_y = height_px as f64 / display_bounds.size.height;
  let hotspot = cursor.hotSpot();
  let cursor_width = CGImage::width(Some(&cursor_image)) as f64;
  let cursor_height = CGImage::height(Some(&cursor_image)) as f64;
  let cursor_x = (pointer.x - display_bounds.origin.x - hotspot.x) * scale_x;
  let cursor_top = (pointer.y - display_bounds.origin.y - hotspot.y) * scale_y;
  let cursor_y = height_px as f64 - cursor_top - cursor_height;
  let cursor_bounds =
    CGRect::new(CGPoint::new(cursor_x, cursor_y), CGSize::new(cursor_width, cursor_height));
  CGContext::draw_image(Some(&bitmap), cursor_bounds, Some(&cursor_image));

  CGBitmapContextCreateImage(Some(&bitmap))
    .ok_or_else(|| CaptureError::CaptureFailed("finalizing cursor composite failed".into()))
}

fn cached_shareable_content(deadline: Instant) -> Result<ShareableContentSnapshot, CaptureError> {
  if let Some(content) = lock_unpoisoned(content_cache()).clone() {
    return Ok(content);
  }
  let content = fetch_shareable_content(deadline)?;
  *lock_unpoisoned(content_cache()) = Some(content.clone());
  Ok(content)
}

fn fetch_shareable_content(deadline: Instant) -> Result<ShareableContentSnapshot, CaptureError> {
  let (sender, receiver) = sync_channel(1);
  let completion = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
    let result = unsafe { Retained::retain(content) }
      .map(ShareableContentSnapshot)
      .ok_or_else(|| capture_error_from_optional_ns_error(error, "enumerating shareable content"));
    let _ = sender.try_send(result);
  });
  unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&completion) };
  receive_until(receiver, deadline, "enumerating shareable content")
}

fn find_display(
  content: &SCShareableContent,
  display_id: u32,
) -> Result<Retained<SCDisplay>, CaptureError> {
  unsafe { content.displays() }
    .iter()
    .find(|display| unsafe { display.displayID() } == display_id)
    .ok_or(CaptureError::NoDisplay)
}

fn content_filter(
  content: &SCShareableContent,
  display: &SCDisplay,
) -> Result<Retained<SCContentFilter>, CaptureError> {
  let current_pid = i32::try_from(std::process::id()).map_err(|_| {
    CaptureError::CaptureFailed("process id does not fit ScreenCaptureKit metadata".to_owned())
  })?;
  let excluded_applications: Vec<Retained<SCRunningApplication>> =
    unsafe { content.applications() }
      .iter()
      .filter(|application| unsafe { application.processID() } == current_pid)
      .collect();
  let excluded_applications = NSArray::from_retained_slice(&excluded_applications);
  let excepting_windows = NSArray::<SCWindow>::from_slice(&[]);
  Ok(unsafe {
    SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
      SCContentFilter::alloc(),
      display,
      &excluded_applications,
      &excepting_windows,
    )
  })
}

fn screenshot_configuration(
  [width_px, height_px]: [u32; 2],
  include_cursor: bool,
) -> Retained<SCStreamConfiguration> {
  let configuration = unsafe { SCStreamConfiguration::new() };
  unsafe {
    configuration.setWidth(width_px as usize);
    configuration.setHeight(height_px as usize);
    configuration.setColorSpaceName(kCGColorSpaceSRGB);
    configuration.setShowsCursor(include_cursor);
  }
  configuration
}

fn content_cache() -> &'static Mutex<Option<ShareableContentSnapshot>> {
  SHAREABLE_CONTENT.get_or_init(|| Mutex::new(None))
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  match mutex.lock() {
    Ok(guard) => guard,
    Err(poisoned) => poisoned.into_inner(),
  }
}

fn ensure_time_remaining(
  deadline: Instant,
  operation: &'static str,
) -> Result<Duration, CaptureError> {
  deadline
    .checked_duration_since(Instant::now())
    .filter(|remaining| !remaining.is_zero())
    .ok_or_else(|| {
      CaptureError::CaptureFailed(format!("screen capture timed out while {operation}"))
    })
}

fn receive_until<T>(
  receiver: Receiver<Result<T, CaptureError>>,
  deadline: Instant,
  operation: &'static str,
) -> Result<T, CaptureError> {
  match receiver.recv_timeout(ensure_time_remaining(deadline, operation)?) {
    Ok(result) => result,
    Err(RecvTimeoutError::Timeout) => {
      Err(CaptureError::CaptureFailed(format!("screen capture timed out while {operation}")))
    }
    Err(RecvTimeoutError::Disconnected) => Err(CaptureError::CaptureFailed(format!(
      "ScreenCaptureKit callback disconnected while {operation}"
    ))),
  }
}

fn capture_error_from_optional_ns_error(
  error: *mut NSError,
  operation: &'static str,
) -> CaptureError {
  unsafe { error.as_ref() }.map_or_else(
    || CaptureError::CaptureFailed(format!("ScreenCaptureKit failed while {operation}")),
    |error| capture_error_from_ns_error(error, operation),
  )
}

fn capture_error_from_ns_error(error: &NSError, operation: &'static str) -> CaptureError {
  let domain = error.domain();
  let code = error.code();
  let is_stream_error = &*domain == unsafe { SCStreamErrorDomain };
  if is_stream_error && code == SCStreamErrorCode::UserDeclined.0 {
    return CaptureError::PermissionDenied;
  }
  if is_stream_error
    && (code == SCStreamErrorCode::NoDisplayList.0 || code == SCStreamErrorCode::NoCaptureSource.0)
  {
    return CaptureError::NoDisplay;
  }
  CaptureError::CaptureFailed(format!(
    "ScreenCaptureKit {operation} failed ({} {code}): {}",
    domain,
    error.localizedDescription()
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use objc2_foundation::ns_string;

  #[test]
  fn expired_deadline_is_rejected_before_capture() {
    let error =
      ensure_time_remaining(Instant::now() - Duration::from_millis(1), "testing").unwrap_err();
    assert!(error.to_string().contains("timed out"));
  }

  #[test]
  fn capture_gate_rejects_a_second_request() {
    let first = CaptureGate::claim().unwrap();
    assert!(CaptureGate::claim().is_err());
    drop(first);
    assert!(CaptureGate::claim().is_ok());
  }

  #[test]
  fn maps_screen_capture_kit_user_declined_to_permission_denied() {
    let error = unsafe {
      NSError::errorWithDomain_code_userInfo(
        SCStreamErrorDomain,
        SCStreamErrorCode::UserDeclined.0,
        None,
      )
    };
    assert!(matches!(
      capture_error_from_ns_error(&error, "testing"),
      CaptureError::PermissionDenied
    ));
  }

  #[test]
  fn does_not_interpret_error_codes_from_another_domain() {
    let error = unsafe {
      NSError::errorWithDomain_code_userInfo(
        ns_string!("RSBoardTestErrorDomain"),
        SCStreamErrorCode::UserDeclined.0,
        None,
      )
    };
    assert!(matches!(
      capture_error_from_ns_error(&error, "testing"),
      CaptureError::CaptureFailed(_)
    ));
  }
}
