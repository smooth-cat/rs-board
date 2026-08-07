use std::{
  slice,
  sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
  },
  time::Duration,
};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use image::RgbaImage;
use objc2::{
  AllocAnyThread, DefinedClass, define_class, msg_send,
  rc::{Retained, autoreleasepool},
  runtime::ProtocolObject,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::kCGColorSpaceSRGB;
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
  CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
  CVPixelBufferGetDataSize, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
  CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
  CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
  SCContentFilter, SCDisplay, SCFrameStatus, SCRunningApplication, SCShareableContent, SCStream,
  SCStreamConfiguration, SCStreamErrorCode, SCStreamErrorDomain, SCStreamFrameInfoStatus,
  SCStreamOutput, SCStreamOutputType, SCWindow,
};

use super::{CaptureError, validate_pixel_dimensions};
use crate::performance::{
  PerformanceContext, PerformanceDetails, PerformanceOutcome, PerformanceTimer, record,
};

const CONTENT_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const SAMPLE_QUEUE_LABEL: &str = "com.linjiajian.rs-board.screen-capture";
const CLEANUP_QUEUE_LABEL: &str = "com.linjiajian.rs-board.screen-capture.cleanup";
const MAX_STOP_ATTEMPTS: u8 = 3;

struct ShareableContentSnapshot(Retained<SCShareableContent>);

// SAFETY: SCShareableContent is an immutable snapshot whose retained ownership
// is transferred exactly once from the framework completion callback to the
// capture worker. The worker is the only code that reads the snapshot.
unsafe impl Send for ShareableContentSnapshot {}

#[derive(Debug)]
struct ScreenCaptureOutputIvars {
  sender: SyncSender<Result<RgbaImage, CaptureError>>,
  completed: AtomicBool,
  performance: PerformanceContext,
  details: PerformanceDetails,
}

impl ScreenCaptureOutputIvars {
  fn claim(&self) -> bool {
    self.completed.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok()
  }

  fn send_claimed(&self, result: Result<RgbaImage, CaptureError>) {
    let _ = self.sender.try_send(result);
  }
}

define_class!(
  // SAFETY: NSObject has no extra subclassing requirements. The callback
  // state is thread-safe because ScreenCaptureKit invokes it off the UI thread.
  #[unsafe(super(NSObject))]
  #[name = "RSBoardScreenCaptureOutput"]
  #[ivars = ScreenCaptureOutputIvars]
  struct ScreenCaptureOutput;

  // SAFETY: The implementation only reads the supplied sample buffer during
  // the callback and sends an owned RGBA image through a synchronized channel.
  unsafe impl SCStreamOutput for ScreenCaptureOutput {
    #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
    unsafe fn did_output_sample_buffer(
      &self,
      _stream: &SCStream,
      sample_buffer: &CMSampleBuffer,
      output_type: SCStreamOutputType,
    ) {
      if output_type != SCStreamOutputType::Screen || !is_complete_frame(sample_buffer) {
        return;
      }
      if self.ivars().claim() {
        record(
          "capture.frame.complete_received",
          self.ivars().performance,
          self.ivars().details,
          PerformanceOutcome::Ok,
        );
        let timer = PerformanceTimer::start(
          "capture.pixel_convert",
          self.ivars().performance,
          self.ivars().details,
        );
        let result = sample_buffer_to_rgba(sample_buffer);
        match &result {
          Ok(_) => timer.finish_ok(),
          Err(error) => timer.finish_error(error),
        }
        self.ivars().send_claimed(result);
      }
    }
  }

  // SAFETY: NSObjectProtocol has no additional implementation requirements.
  unsafe impl NSObjectProtocol for ScreenCaptureOutput {}
);

impl ScreenCaptureOutput {
  fn new(
    sender: SyncSender<Result<RgbaImage, CaptureError>>,
    performance: PerformanceContext,
    details: PerformanceDetails,
  ) -> Retained<Self> {
    assert_send_sync::<Self>();
    let this = Self::alloc().set_ivars(ScreenCaptureOutputIvars {
      sender,
      completed: AtomicBool::new(false),
      performance,
      details,
    });
    unsafe { msg_send![super(this), init] }
  }
}

#[derive(Clone)]
struct CaptureSession {
  stream: Retained<SCStream>,
  output: Retained<ScreenCaptureOutput>,
  sample_queue: DispatchRetained<DispatchQueue>,
  cleanup_queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: ScreenCaptureKit stream control is asynchronous and has no caller
// thread affinity, while the output's ivars are Send + Sync. Control operations
// run on cleanup_queue, and final release is serialized behind frame callbacks.
unsafe impl Send for CaptureSession {}

impl CaptureSession {
  fn remove_output(&self) -> Result<(), CaptureError> {
    let stream_output: &ProtocolObject<dyn SCStreamOutput> =
      ProtocolObject::from_ref(&*self.output);
    unsafe { self.stream.removeStreamOutput_type_error(stream_output, SCStreamOutputType::Screen) }
      .map_err(|error| capture_error_from_ns_error(&error, "removing stream output"))
  }
}

struct StartState {
  session: Option<CaptureSession>,
  completion_seen: bool,
  cancelled: bool,
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  match mutex.lock() {
    Ok(guard) => guard,
    Err(poisoned) => poisoned.into_inner(),
  }
}

fn assert_send_sync<T: Send + Sync>() {}

fn error_completion_block<F>(callback: F) -> RcBlock<dyn Fn(*mut NSError)>
where
  F: Fn(*mut NSError) + Send + Sync + 'static,
{
  RcBlock::new(callback)
}

fn content_completion_block<F>(
  callback: F,
) -> RcBlock<dyn Fn(*mut SCShareableContent, *mut NSError)>
where
  F: Fn(*mut SCShareableContent, *mut NSError) + Send + Sync + 'static,
{
  RcBlock::new(callback)
}

pub(super) fn capture_display(
  display_id: u32,
  expected_pixel_size: [u32; 2],
  include_cursor: bool,
  performance: PerformanceContext,
) -> Result<RgbaImage, CaptureError> {
  let details =
    PerformanceDetails::default().display_id(display_id).pixel_size(expected_pixel_size);
  let timer = PerformanceTimer::start("capture.macos_backend.total", performance, details);
  let result = autoreleasepool(|_| {
    capture_display_inner(display_id, expected_pixel_size, include_cursor, performance, details)
  });
  match &result {
    Ok(_) => timer.finish_ok(),
    Err(error) => timer.finish_error(error),
  }
  result
}

fn capture_display_inner(
  display_id: u32,
  expected_pixel_size: [u32; 2],
  include_cursor: bool,
  performance: PerformanceContext,
  details: PerformanceDetails,
) -> Result<RgbaImage, CaptureError> {
  let content_timer = PerformanceTimer::start("capture.shareable_content", performance, details);
  let content_result = shareable_content();
  match &content_result {
    Ok(_) => content_timer.finish_ok(),
    Err(error) => content_timer.finish_error(error),
  }
  let content = content_result?;

  let setup_timer = PerformanceTimer::start("capture.stream.setup", performance, details);
  let setup_result = (|| {
    let display = find_display(&content, display_id)?;
    let filter = content_filter(&content, &display)?;
    let configuration = stream_configuration(expected_pixel_size, include_cursor);

    let (frame_sender, frame_receiver) = sync_channel(1);
    let output = ScreenCaptureOutput::new(frame_sender, performance, details);
    let sample_queue = DispatchQueue::new(SAMPLE_QUEUE_LABEL, DispatchQueueAttr::SERIAL);
    let cleanup_queue = DispatchQueue::new(CLEANUP_QUEUE_LABEL, DispatchQueueAttr::SERIAL);
    let stream = unsafe {
      SCStream::initWithFilter_configuration_delegate(
        SCStream::alloc(),
        &filter,
        &configuration,
        None,
      )
    };
    let stream_output: &ProtocolObject<dyn SCStreamOutput> = ProtocolObject::from_ref(&*output);
    unsafe {
      stream.addStreamOutput_type_sampleHandlerQueue_error(
        stream_output,
        SCStreamOutputType::Screen,
        Some(&sample_queue),
      )
    }
    .map_err(|error| capture_error_from_ns_error(&error, "adding stream output"))?;
    Ok::<_, CaptureError>((
      CaptureSession { stream, output, sample_queue, cleanup_queue },
      frame_receiver,
    ))
  })();
  match &setup_result {
    Ok(_) => setup_timer.finish_ok(),
    Err(error) => setup_timer.finish_error(error),
  }
  let (session, frame_receiver) = setup_result?;

  let start_timer = PerformanceTimer::start("capture.stream.start", performance, details);
  let start_result = start_stream(session);
  match &start_result {
    Ok(_) => start_timer.finish_ok(),
    Err(error) => start_timer.finish_error(error),
  }
  let session = start_result?;
  let frame_timer = PerformanceTimer::start("capture.frame.wait", performance, details);
  let frame_result = receive_result(frame_receiver, FRAME_TIMEOUT, "waiting for a complete frame");
  match &frame_result {
    Ok(_) => frame_timer.finish_ok(),
    Err(error) => frame_timer.finish_error(error),
  }
  let stop_timer = PerformanceTimer::start("capture.stream.stop", performance, details);
  let stop_result = stop_stream(session);
  match &stop_result {
    Ok(_) => stop_timer.finish_ok(),
    Err(error) => stop_timer.finish_error(error),
  }
  let image = match (frame_result, stop_result) {
    (Err(error), _) => return Err(error),
    (Ok(_), Err(error)) => return Err(error),
    (Ok(image), Ok(())) => image,
  };

  let actual_pixel_size = [image.width(), image.height()];
  if actual_pixel_size != expected_pixel_size {
    return Err(CaptureError::CaptureFailed(format!(
      "ScreenCaptureKit returned {}x{} pixels; expected {}x{}",
      actual_pixel_size[0], actual_pixel_size[1], expected_pixel_size[0], expected_pixel_size[1]
    )));
  }
  Ok(image)
}

fn shareable_content() -> Result<Retained<SCShareableContent>, CaptureError> {
  let (sender, receiver) = sync_channel(1);
  let completion = content_completion_block(move |content, error| {
    let result = unsafe { Retained::retain(content) }
      .map(ShareableContentSnapshot)
      .ok_or_else(|| capture_error_from_optional_ns_error(error, "enumerating shareable content"));
    let _ = sender.try_send(result);
  });
  unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&completion) };
  receive_result(receiver, CONTENT_TIMEOUT, "enumerating shareable content")
    .map(|snapshot| snapshot.0)
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

fn stream_configuration(
  [width_px, height_px]: [u32; 2],
  include_cursor: bool,
) -> Retained<SCStreamConfiguration> {
  let configuration = unsafe { SCStreamConfiguration::new() };
  unsafe {
    configuration.setWidth(width_px as usize);
    configuration.setHeight(height_px as usize);
    configuration.setPixelFormat(kCVPixelFormatType_32BGRA);
    configuration.setColorSpaceName(kCGColorSpaceSRGB);
    configuration.setShowsCursor(include_cursor);
    configuration.setQueueDepth(1);
  }
  configuration
}

fn start_stream(session: CaptureSession) -> Result<CaptureSession, CaptureError> {
  let (sender, receiver) = sync_channel(1);
  let stream = session.stream.clone();
  let state = Arc::new(Mutex::new(StartState {
    session: Some(session),
    completion_seen: false,
    cancelled: false,
  }));
  let callback_state = Arc::clone(&state);
  let completion = error_completion_block(move |error| {
    let result = completion_result(error, "starting stream");
    let failed = result.is_err();
    let session_to_stop = {
      let mut state = lock_unpoisoned(&callback_state);
      state.completion_seen = true;
      if state.cancelled || failed { state.session.take() } else { None }
    };
    if let Some(session) = session_to_stop {
      let _ = request_stop(session);
    }
    let _ = sender.try_send(result);
  });
  unsafe { stream.startCaptureWithCompletionHandler(Some(&completion)) };

  match receiver.recv_timeout(CONTENT_TIMEOUT) {
    Ok(Ok(())) => lock_unpoisoned(&state).session.take().ok_or_else(|| {
      CaptureError::CaptureFailed(
        "ScreenCaptureKit start completed without an owned capture session".to_owned(),
      )
    }),
    Ok(Err(error)) => Err(error),
    Err(RecvTimeoutError::Timeout) => {
      let session_to_stop = {
        let mut state = lock_unpoisoned(&state);
        state.cancelled = true;
        if state.completion_seen { state.session.take() } else { state.session.clone() }
      };
      if let Some(session) = session_to_stop {
        let _ = request_stop(session);
      }
      Err(CaptureError::CaptureFailed(
        "ScreenCaptureKit timed out while starting stream".to_owned(),
      ))
    }
    Err(RecvTimeoutError::Disconnected) => {
      if let Some(session) = lock_unpoisoned(&state).session.take() {
        let _ = request_stop(session);
      }
      Err(CaptureError::CaptureFailed(
        "ScreenCaptureKit callback disconnected while starting stream".to_owned(),
      ))
    }
  }
}

fn stop_stream(session: CaptureSession) -> Result<(), CaptureError> {
  receive_result(request_stop(session), STOP_TIMEOUT, "stopping stream")
}

fn request_stop(session: CaptureSession) -> Receiver<Result<(), CaptureError>> {
  let (sender, receiver) = sync_channel(1);
  let cleanup_queue = session.cleanup_queue.clone();
  cleanup_queue.exec_async(move || issue_stop(session, sender, 0));
  receiver
}

fn issue_stop(session: CaptureSession, sender: SyncSender<Result<(), CaptureError>>, attempt: u8) {
  let stream = session.stream.clone();
  let pending_session = Arc::new(Mutex::new(Some(session)));
  let callback_session = Arc::clone(&pending_session);
  let completion = error_completion_block(move |error| {
    let stop_result = completion_result(error, "stopping stream");
    let session = lock_unpoisoned(&callback_session).take();
    let Some(session) = session else {
      return;
    };

    let cleanup_queue = session.cleanup_queue.clone();
    let cleanup_sender = sender.clone();
    cleanup_queue.exec_async(move || {
      finish_stop_attempt(session, stop_result, cleanup_sender, attempt);
    });
  });
  unsafe { stream.stopCaptureWithCompletionHandler(Some(&completion)) };
}

fn finish_stop_attempt(
  session: CaptureSession,
  stop_result: Result<(), CaptureError>,
  sender: SyncSender<Result<(), CaptureError>>,
  attempt: u8,
) {
  let remove_result = session.remove_output();
  let can_release = stop_result.is_ok() || remove_result.is_ok();
  let result = match (stop_result, remove_result) {
    (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    (Ok(()), Ok(())) => Ok(()),
  };

  if can_release {
    let sample_queue = session.sample_queue.clone();
    sample_queue.exec_async(move || {
      let _ = sender.try_send(result);
      drop(session);
    });
  } else if attempt + 1 < MAX_STOP_ATTEMPTS {
    let cleanup_queue = session.cleanup_queue.clone();
    cleanup_queue.exec_async(move || issue_stop(session, sender, attempt + 1));
  } else {
    let _ = sender.try_send(result);
    // Both stop and output removal failed repeatedly. Keeping the registered
    // receiver alive is safer than releasing an object the framework may call.
    std::mem::forget(session);
  }
}

fn completion_result(error: *mut NSError, operation: &'static str) -> Result<(), CaptureError> {
  match unsafe { error.as_ref() } {
    Some(error) => Err(capture_error_from_ns_error(error, operation)),
    None => Ok(()),
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

fn receive_result<T>(
  receiver: Receiver<Result<T, CaptureError>>,
  timeout: Duration,
  operation: &'static str,
) -> Result<T, CaptureError> {
  match receiver.recv_timeout(timeout) {
    Ok(result) => result,
    Err(RecvTimeoutError::Timeout) => {
      Err(CaptureError::CaptureFailed(format!("ScreenCaptureKit timed out while {operation}")))
    }
    Err(RecvTimeoutError::Disconnected) => Err(CaptureError::CaptureFailed(format!(
      "ScreenCaptureKit callback disconnected while {operation}"
    ))),
  }
}

fn is_complete_frame(sample_buffer: &CMSampleBuffer) -> bool {
  if !unsafe { sample_buffer.is_valid() } {
    return false;
  }
  let Some(attachments) = (unsafe { sample_buffer.sample_attachments_array(false) }) else {
    return false;
  };
  let attachments: CFRetained<CFArray<CFDictionary<CFString, CFType>>> =
    unsafe { CFRetained::cast_unchecked(attachments) };
  let Some(frame_info) = attachments.get(0) else {
    return false;
  };
  let status_key = unsafe { SCStreamFrameInfoStatus };
  frame_info
    .get(status_key.as_ref())
    .and_then(|status| status.downcast::<CFNumber>().ok())
    .and_then(|status| status.as_i32())
    == Some(SCFrameStatus::Complete.0 as i32)
}

fn sample_buffer_to_rgba(sample_buffer: &CMSampleBuffer) -> Result<RgbaImage, CaptureError> {
  let pixel_buffer = unsafe { sample_buffer.image_buffer() }.ok_or_else(|| {
    CaptureError::CaptureFailed("complete frame did not contain a pixel buffer".to_owned())
  })?;
  ensure_bgra_pixel_format(CVPixelBufferGetPixelFormatType(&pixel_buffer))?;

  let lock_flags = CVPixelBufferLockFlags::ReadOnly;
  let lock_result = unsafe { CVPixelBufferLockBaseAddress(&pixel_buffer, lock_flags) };
  if lock_result != kCVReturnSuccess {
    return Err(CaptureError::CaptureFailed(format!(
      "locking ScreenCaptureKit pixel buffer failed with CoreVideo status {lock_result}"
    )));
  }

  let result = copy_locked_pixel_buffer(&pixel_buffer);
  let unlock_result = unsafe { CVPixelBufferUnlockBaseAddress(&pixel_buffer, lock_flags) };
  match (result, unlock_result) {
    (Err(error), _) => Err(error),
    (Ok(_), status) if status != kCVReturnSuccess => Err(CaptureError::CaptureFailed(format!(
      "unlocking ScreenCaptureKit pixel buffer failed with CoreVideo status {status}"
    ))),
    (Ok(image), _) => Ok(image),
  }
}

fn ensure_bgra_pixel_format(pixel_format: u32) -> Result<(), CaptureError> {
  if pixel_format == kCVPixelFormatType_32BGRA {
    Ok(())
  } else {
    Err(CaptureError::CaptureFailed(format!(
      "ScreenCaptureKit returned unsupported pixel format 0x{pixel_format:08X}"
    )))
  }
}

fn copy_locked_pixel_buffer(pixel_buffer: &CVPixelBuffer) -> Result<RgbaImage, CaptureError> {
  let width = CVPixelBufferGetWidth(pixel_buffer);
  let height = CVPixelBufferGetHeight(pixel_buffer);
  let width_u32 = u32::try_from(width)
    .map_err(|_| CaptureError::CaptureFailed("captured image width is too large".to_owned()))?;
  let height_u32 = u32::try_from(height)
    .map_err(|_| CaptureError::CaptureFailed("captured image height is too large".to_owned()))?;
  validate_pixel_dimensions([width_u32, height_u32])?;

  let bytes_per_row = CVPixelBufferGetBytesPerRow(pixel_buffer);
  let required_len = bytes_per_row
    .checked_mul(height)
    .ok_or_else(|| CaptureError::CaptureFailed("captured image data is too large".to_owned()))?;
  let data_size = CVPixelBufferGetDataSize(pixel_buffer);
  if data_size < required_len {
    return Err(CaptureError::CaptureFailed(format!(
      "pixel buffer contains {data_size} bytes; its row stride requires {required_len}"
    )));
  }
  let base_address = CVPixelBufferGetBaseAddress(pixel_buffer);
  if base_address.is_null() {
    return Err(CaptureError::CaptureFailed(
      "ScreenCaptureKit pixel buffer has no base address".to_owned(),
    ));
  }
  let bytes = unsafe { slice::from_raw_parts(base_address.cast::<u8>(), required_len) };
  bgra_rows_to_rgba(width_u32, height_u32, bytes_per_row, bytes)
}

fn bgra_rows_to_rgba(
  width: u32,
  height: u32,
  bytes_per_row: usize,
  bytes: &[u8],
) -> Result<RgbaImage, CaptureError> {
  let width = width as usize;
  let height = height as usize;
  let packed_row_len = width
    .checked_mul(4)
    .ok_or_else(|| CaptureError::CaptureFailed("captured image row is too wide".to_owned()))?;
  if bytes_per_row < packed_row_len {
    return Err(CaptureError::CaptureFailed(format!(
      "pixel buffer row stride {bytes_per_row} is smaller than packed row size {packed_row_len}"
    )));
  }
  let required_len = bytes_per_row
    .checked_mul(height)
    .ok_or_else(|| CaptureError::CaptureFailed("captured image data is too large".to_owned()))?;
  if bytes.len() < required_len {
    return Err(CaptureError::CaptureFailed(format!(
      "pixel buffer contains {} bytes; expected at least {required_len}",
      bytes.len()
    )));
  }
  let output_len = packed_row_len
    .checked_mul(height)
    .ok_or_else(|| CaptureError::CaptureFailed("captured image buffer is too large".to_owned()))?;
  let mut rgba = Vec::with_capacity(output_len);
  for row in bytes.chunks_exact(bytes_per_row).take(height) {
    for pixel in row[..packed_row_len].chunks_exact(4) {
      rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
  }
  RgbaImage::from_raw(width as u32, height as u32, rgba).ok_or_else(|| {
    CaptureError::CaptureFailed("unable to construct RGBA image from pixel buffer".to_owned())
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use objc2_foundation::ns_string;

  #[test]
  fn converts_bgra_rows_to_rgba_without_copying_padding() {
    let bytes =
      [3, 2, 1, 4, 7, 6, 5, 8, 99, 99, 99, 99, 13, 12, 11, 14, 17, 16, 15, 18, 88, 88, 88, 88];
    let image = bgra_rows_to_rgba(2, 2, 12, &bytes).unwrap();
    assert_eq!(image.into_raw(), vec![1, 2, 3, 4, 5, 6, 7, 8, 11, 12, 13, 14, 15, 16, 17, 18]);
  }

  #[test]
  fn rejects_bgra_row_stride_smaller_than_pixel_width() {
    let error = bgra_rows_to_rgba(2, 1, 7, &[0; 8]).unwrap_err();
    assert!(error.to_string().contains("row stride"));
  }

  #[test]
  fn rejects_short_bgra_buffer() {
    let error = bgra_rows_to_rgba(1, 2, 4, &[0; 7]).unwrap_err();
    assert!(error.to_string().contains("expected at least 8"));
  }

  #[test]
  fn rejects_non_bgra_pixel_format() {
    assert!(ensure_bgra_pixel_format(kCVPixelFormatType_32BGRA).is_ok());
    assert!(ensure_bgra_pixel_format(0).is_err());
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
