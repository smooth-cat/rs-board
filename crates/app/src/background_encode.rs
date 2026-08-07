use std::{
  collections::HashMap,
  sync::{Arc, Condvar, Mutex, MutexGuard},
  thread,
};

use thiserror::Error;

use crate::{
  capture::NativeCaptureImage,
  performance::{PerformanceContext, PerformanceDetails, PerformanceTimer},
  storage::BackgroundData,
};

const MAX_ACTIVE_ENCODERS: usize = 2;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum BackgroundPrepareError {
  #[error("background encoding failed: {0}")]
  Failed(Arc<str>),
  #[error("background preparation was superseded by a newer capture")]
  Superseded,
}

#[derive(Clone)]
pub struct PreparedBackground {
  capture_sequence: u64,
  pixel_size: [u32; 2],
  shared: Arc<PreparedShared>,
}

struct PreparedShared {
  state: Mutex<PreparedState>,
  changed: Condvar,
}

#[derive(Clone)]
enum PreparedState {
  Pending,
  Ready(BackgroundData),
  Failed(Arc<str>),
  Superseded,
}

impl PreparedBackground {
  fn pending(capture_sequence: u64, pixel_size: [u32; 2]) -> Self {
    Self {
      capture_sequence,
      pixel_size,
      shared: Arc::new(PreparedShared {
        state: Mutex::new(PreparedState::Pending),
        changed: Condvar::new(),
      }),
    }
  }

  #[cfg(test)]
  pub(crate) fn pending_for_test(capture_sequence: u64, pixel_size: [u32; 2]) -> Self {
    Self::pending(capture_sequence, pixel_size)
  }

  pub fn ready(
    capture_sequence: u64,
    background: BackgroundData,
  ) -> Result<Self, BackgroundPrepareError> {
    let (width_px, height_px) = background
      .dimensions()
      .map_err(|error| BackgroundPrepareError::Failed(Arc::from(error.to_string())))?;
    Ok(Self {
      capture_sequence,
      pixel_size: [width_px, height_px],
      shared: Arc::new(PreparedShared {
        state: Mutex::new(PreparedState::Ready(background)),
        changed: Condvar::new(),
      }),
    })
  }

  pub fn capture_sequence(&self) -> u64 {
    self.capture_sequence
  }

  pub fn pixel_size(&self) -> [u32; 2] {
    self.pixel_size
  }

  pub fn needs_retry(&self) -> bool {
    matches!(*lock_unpoisoned(&self.shared.state), PreparedState::Failed(_))
  }

  pub fn wait(&self) -> Result<BackgroundData, BackgroundPrepareError> {
    let mut state = lock_unpoisoned(&self.shared.state);
    loop {
      match &*state {
        PreparedState::Pending => {
          state = wait_unpoisoned(&self.shared.changed, state);
        }
        PreparedState::Ready(background) => return Ok(background.clone()),
        PreparedState::Failed(error) => {
          return Err(BackgroundPrepareError::Failed(Arc::clone(error)));
        }
        PreparedState::Superseded => return Err(BackgroundPrepareError::Superseded),
      }
    }
  }

  fn complete(&self, result: Result<BackgroundData, String>) {
    let mut state = lock_unpoisoned(&self.shared.state);
    if !matches!(*state, PreparedState::Pending) {
      return;
    }
    *state = match result {
      Ok(background) => PreparedState::Ready(background),
      Err(error) => PreparedState::Failed(Arc::from(error)),
    };
    self.shared.changed.notify_all();
  }

  pub(crate) fn supersede(&self) {
    let mut state = lock_unpoisoned(&self.shared.state);
    if matches!(*state, PreparedState::Pending) {
      *state = PreparedState::Superseded;
      self.shared.changed.notify_all();
    }
  }
}

type Encoder =
  dyn Fn(u64, &NativeCaptureImage) -> Result<BackgroundData, String> + Send + Sync + 'static;

#[derive(Clone)]
pub struct BackgroundEncodeScheduler {
  inner: Arc<SchedulerInner>,
}

struct SchedulerInner {
  state: Mutex<SchedulerState>,
  encoder: Arc<Encoder>,
}

#[derive(Default)]
struct SchedulerState {
  latest_capture_sequence: u64,
  next_encode_id: u64,
  active: HashMap<u64, ActiveEncode>,
  pending_latest: Option<EncodeRequest>,
}

struct ActiveEncode {
  capture_sequence: u64,
  prepared: PreparedBackground,
}

struct EncodeRequest {
  encode_id: u64,
  capture_sequence: u64,
  image: NativeCaptureImage,
  prepared: PreparedBackground,
  performance: PerformanceContext,
}

impl BackgroundEncodeScheduler {
  pub fn new() -> Self {
    Self::with_encoder(Arc::new(|_, image| encode_native_background(image)))
  }

  fn with_encoder(encoder: Arc<Encoder>) -> Self {
    Self {
      inner: Arc::new(SchedulerInner { state: Mutex::new(SchedulerState::default()), encoder }),
    }
  }

  pub fn submit(
    &self,
    capture_sequence: u64,
    image: NativeCaptureImage,
    performance: PerformanceContext,
  ) -> PreparedBackground {
    let prepared = PreparedBackground::pending(capture_sequence, image.pixel_size());
    let launch = {
      let mut state = lock_unpoisoned(&self.inner.state);
      if capture_sequence > state.latest_capture_sequence {
        state.latest_capture_sequence = capture_sequence;
        for active in state.active.values() {
          if active.capture_sequence < capture_sequence {
            active.prepared.supersede();
          }
        }
        if state
          .pending_latest
          .as_ref()
          .is_some_and(|pending| pending.capture_sequence < capture_sequence)
          && let Some(pending) = state.pending_latest.take()
        {
          pending.prepared.supersede();
        }
      }

      state.next_encode_id = state.next_encode_id.wrapping_add(1).max(1);
      let request = EncodeRequest {
        encode_id: state.next_encode_id,
        capture_sequence,
        image,
        prepared: prepared.clone(),
        performance,
      };
      if capture_sequence < state.latest_capture_sequence {
        request.prepared.supersede();
        None
      } else if state.active.len() < MAX_ACTIVE_ENCODERS {
        state
          .active
          .insert(request.encode_id, ActiveEncode { capture_sequence, prepared: prepared.clone() });
        Some(request)
      } else {
        if let Some(replaced) = state.pending_latest.replace(request) {
          replaced.prepared.supersede();
        }
        None
      }
    };
    if let Some(request) = launch {
      launch_encode(Arc::clone(&self.inner), request);
    }
    prepared
  }

  #[cfg(test)]
  fn counts(&self) -> (usize, usize) {
    let state = lock_unpoisoned(&self.inner.state);
    (state.active.len(), usize::from(state.pending_latest.is_some()))
  }
}

impl Default for BackgroundEncodeScheduler {
  fn default() -> Self {
    Self::new()
  }
}

fn launch_encode(inner: Arc<SchedulerInner>, request: EncodeRequest) {
  let prepared = request.prepared.clone();
  let encode_id = request.encode_id;
  let spawn_result = thread::Builder::new().name("background-png-encode".into()).spawn({
    let inner = Arc::clone(&inner);
    move || {
      let timer = PerformanceTimer::start(
        "background_encode.png",
        request.performance,
        PerformanceDetails::default().pixel_size(request.prepared.pixel_size()),
      );
      let result = (inner.encoder)(request.capture_sequence, &request.image);
      match &result {
        Ok(_) => timer.finish_ok(),
        Err(_) => timer.finish_error_code("background_encode_failed"),
      }
      request.prepared.complete(result);
      finish_encode(inner, request.encode_id);
    }
  });
  if let Err(error) = spawn_result {
    prepared.complete(Err(format!("unable to start background encoder: {error}")));
    finish_encode(inner, encode_id);
  }
}

fn finish_encode(inner: Arc<SchedulerInner>, encode_id: u64) {
  let pending = {
    let mut state = lock_unpoisoned(&inner.state);
    state.active.remove(&encode_id);
    let pending = state.pending_latest.take();
    if let Some(request) = &pending {
      state.active.insert(
        request.encode_id,
        ActiveEncode {
          capture_sequence: request.capture_sequence,
          prepared: request.prepared.clone(),
        },
      );
    }
    pending
  };
  if let Some(request) = pending {
    launch_encode(inner, request);
  }
}

fn encode_native_background(image: &NativeCaptureImage) -> Result<BackgroundData, String> {
  let pixel_size = image.pixel_size();
  let bytes = image.encode_png().map_err(|error| error.to_string())?;
  let background = BackgroundData::encoded_png(bytes).map_err(|error| error.to_string())?;
  let dimensions = background.dimensions().map_err(|error| error.to_string())?;
  if dimensions != (pixel_size[0], pixel_size[1]) {
    return Err(format!(
      "encoded background is {}x{}; expected {}x{}",
      dimensions.0, dimensions.1, pixel_size[0], pixel_size[1]
    ));
  }
  Ok(background)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_unpoisoned<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
  condvar.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
  use std::{
    collections::HashSet,
    sync::{
      Condvar, Mutex,
      atomic::{AtomicUsize, Ordering},
      mpsc,
    },
  };

  use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGImageAlphaInfo,
    CGImageByteOrderInfo,
  };

  use super::*;

  fn native_image() -> NativeCaptureImage {
    let mut pixel = [10_u8, 20, 30, 255];
    let color_space = CGColorSpace::new_device_rgb().unwrap();
    let bitmap = unsafe {
      CGBitmapContextCreate(
        pixel.as_mut_ptr().cast(),
        1,
        1,
        8,
        4,
        Some(&color_space),
        CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
      )
    }
    .unwrap();
    NativeCaptureImage::from_cg_image(CGBitmapContextCreateImage(Some(&bitmap)).unwrap()).unwrap()
  }

  #[test]
  fn limits_active_encodes_and_keeps_only_latest_pending_request() {
    let (started_sender, started_receiver) = mpsc::channel();
    let released = Arc::new((Mutex::new(HashSet::new()), Condvar::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let maximum_active = Arc::new(AtomicUsize::new(0));
    let encoder = {
      let released = Arc::clone(&released);
      let active = Arc::clone(&active);
      let maximum_active = Arc::clone(&maximum_active);
      Arc::new(move |sequence: u64, image: &NativeCaptureImage| {
        let now_active = active.fetch_add(1, Ordering::AcqRel) + 1;
        maximum_active.fetch_max(now_active, Ordering::AcqRel);
        started_sender.send(sequence).unwrap();
        let (lock, changed) = &*released;
        let mut released_sequences = lock_unpoisoned(lock);
        while !released_sequences.contains(&sequence) {
          released_sequences = wait_unpoisoned(changed, released_sequences);
        }
        active.fetch_sub(1, Ordering::AcqRel);
        encode_native_background(image)
      }) as Arc<Encoder>
    };
    let scheduler = BackgroundEncodeScheduler::with_encoder(encoder);

    let first = scheduler.submit(1, native_image(), PerformanceContext::default());
    let second = scheduler.submit(2, native_image(), PerformanceContext::default());
    let mut started = [started_receiver.recv().unwrap(), started_receiver.recv().unwrap()];
    started.sort_unstable();
    assert_eq!(started, [1, 2]);

    let third = scheduler.submit(3, native_image(), PerformanceContext::default());
    let fourth = scheduler.submit(4, native_image(), PerformanceContext::default());
    assert_eq!(scheduler.counts(), (2, 1));
    assert!(matches!(first.wait(), Err(BackgroundPrepareError::Superseded)));
    assert!(matches!(second.wait(), Err(BackgroundPrepareError::Superseded)));
    assert!(matches!(third.wait(), Err(BackgroundPrepareError::Superseded)));

    let (lock, changed) = &*released;
    lock_unpoisoned(lock).extend([1, 2]);
    changed.notify_all();
    assert_eq!(started_receiver.recv().unwrap(), 4);
    lock_unpoisoned(lock).insert(4);
    changed.notify_all();
    assert!(fourth.wait().is_ok());
    assert_eq!(maximum_active.load(Ordering::Acquire), MAX_ACTIVE_ENCODERS);
  }

  #[test]
  fn failed_current_capture_can_be_retried_with_the_same_sequence() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let encoder = {
      let attempts = Arc::clone(&attempts);
      Arc::new(move |_, image: &NativeCaptureImage| {
        if attempts.fetch_add(1, Ordering::AcqRel) == 0 {
          Err("injected encoding failure".into())
        } else {
          encode_native_background(image)
        }
      }) as Arc<Encoder>
    };
    let scheduler = BackgroundEncodeScheduler::with_encoder(encoder);
    let failed = scheduler.submit(7, native_image(), PerformanceContext::default());
    assert!(matches!(failed.wait(), Err(BackgroundPrepareError::Failed(_))));

    let retried = scheduler.submit(7, native_image(), PerformanceContext::default());
    let background = retried.wait().unwrap();
    let (_, _, pixels) = background.decode_rgba8().unwrap();
    assert_eq!(&*pixels, &[10, 20, 30, 255]);
  }
}
