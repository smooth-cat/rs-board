use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState, hotkey::HotKey};
use std::{
  path::PathBuf,
  str::FromStr,
  sync::mpsc::{self, Receiver},
};
use thiserror::Error;

#[cfg(target_os = "macos")]
use objc2::{
  DefinedClass, MainThreadMarker, MainThreadOnly, msg_send, rc::Retained, runtime::AnyObject, sel,
};
#[cfg(target_os = "macos")]
use objc2_core_services::{
  kAEOpenDocuments as OPEN_DOCUMENTS_EVENT_ID, kCoreEventClass as CORE_EVENT_CLASS,
  keyDirectObject as DIRECT_OBJECT_KEYWORD,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{
  NSAppleEventDescriptor, NSAppleEventManager, NSObject, NSObjectProtocol, NSURL,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformEvent {
  CaptureRequested,
}

#[derive(Debug, Error)]
pub enum PlatformError {
  #[error("global hotkey {operation} failed: {source}")]
  Hotkey {
    operation: &'static str,
    #[source]
    source: global_hotkey::Error,
  },
  #[error("login item update failed: {0}")]
  LoginItem(String),
  #[error("invalid global hotkey: {0}")]
  InvalidHotkey(String),
}

#[cfg(target_os = "macos")]
pub fn set_launch_at_login(enabled: bool) -> Result<(), PlatformError> {
  use smappservice_rs::{AppService, ServiceType};

  let service = AppService::new(ServiceType::MainApp);
  let result = if enabled { service.register() } else { service.unregister() };
  result.map_err(|error| PlatformError::LoginItem(error.to_string()))
}

#[cfg(not(target_os = "macos"))]
pub fn set_launch_at_login(_enabled: bool) -> Result<(), PlatformError> {
  Ok(())
}

/// Owns the application's bare F1 global shortcut registration.
///
/// Construct this on the UI thread after its event loop has been initialized.
/// Dropping it unregisters F1 before releasing the platform manager.
pub struct GlobalF1Hotkey {
  manager: GlobalHotKeyManager,
  hotkey: HotKey,
  events: Receiver<GlobalHotKeyEvent>,
}

impl GlobalF1Hotkey {
  pub fn new() -> Result<Self, PlatformError> {
    Self::from_shortcut_with_waker("F1", || {})
  }

  pub fn from_shortcut(shortcut: &str) -> Result<Self, PlatformError> {
    Self::from_shortcut_with_waker(shortcut, || {})
  }

  pub fn from_shortcut_with_waker(
    shortcut: &str,
    wake: impl Fn() + Send + Sync + 'static,
  ) -> Result<Self, PlatformError> {
    let manager = GlobalHotKeyManager::new()
      .map_err(|source| PlatformError::Hotkey { operation: "initialization", source })?;
    let hotkey = HotKey::from_str(shortcut)
      .map_err(|error| PlatformError::InvalidHotkey(error.to_string()))?;
    let (sender, events) = mpsc::channel();
    manager
      .register(hotkey)
      .map_err(|source| PlatformError::Hotkey { operation: "registration", source })?;
    GlobalHotKeyEvent::set_event_handler(Some(move |event| {
      let _ = sender.send(event);
      wake();
    }));

    Ok(Self { manager, hotkey, events })
  }

  pub fn hotkey_id(&self) -> u32 {
    self.hotkey.id()
  }

  /// Polls queued global-hotkey messages and returns the next app event.
  pub fn poll_event(&self) -> Option<PlatformEvent> {
    self.events.try_iter().find_map(|event| map_hotkey_event(self.hotkey.id(), event))
  }

  pub fn poll_pressed(&self) -> bool {
    self.poll_event() == Some(PlatformEvent::CaptureRequested)
  }

  pub fn update_shortcut(&mut self, shortcut: &str) -> Result<(), PlatformError> {
    let replacement = HotKey::from_str(shortcut)
      .map_err(|error| PlatformError::InvalidHotkey(error.to_string()))?;
    self
      .manager
      .register(replacement)
      .map_err(|source| PlatformError::Hotkey { operation: "registration", source })?;
    if let Err(source) = self.manager.unregister(self.hotkey) {
      let _ = self.manager.unregister(replacement);
      return Err(PlatformError::Hotkey { operation: "unregistration", source });
    }
    self.hotkey = replacement;
    Ok(())
  }
}

impl Drop for GlobalF1Hotkey {
  fn drop(&mut self) {
    let _ = self.manager.unregister(self.hotkey);
  }
}

fn map_hotkey_event(hotkey_id: u32, event: GlobalHotKeyEvent) -> Option<PlatformEvent> {
  (event.id == hotkey_id && event.state == HotKeyState::Pressed)
    .then_some(PlatformEvent::CaptureRequested)
}

/// Returns `[x, y]` in the global display coordinate space on macOS.
/// Other platforms return `None`, causing capture to use the primary display.
#[cfg(target_os = "macos")]
pub fn global_cursor_position() -> Option<[i32; 2]> {
  use objc2_core_graphics::CGEvent;

  let event = CGEvent::new(None)?;
  let location = CGEvent::location(Some(&event));
  Some([global_coordinate_to_i32(location.x)?, global_coordinate_to_i32(location.y)?])
}

#[cfg(target_os = "macos")]
fn global_coordinate_to_i32(coordinate: f64) -> Option<i32> {
  let coordinate = coordinate.floor();
  (coordinate.is_finite() && coordinate >= i32::MIN as f64 && coordinate <= i32::MAX as f64)
    .then_some(coordinate as i32)
}

#[cfg(not(target_os = "macos"))]
pub fn global_cursor_position() -> Option<[i32; 2]> {
  None
}

#[cfg(target_os = "macos")]
pub struct OpenFileBridge {
  receiver: Receiver<Vec<PathBuf>>,
  manager: Retained<NSAppleEventManager>,
  _handler: Retained<MacOpenFileHandler>,
}

#[cfg(target_os = "macos")]
impl OpenFileBridge {
  pub fn install() -> Option<Self> {
    let mtm = MainThreadMarker::new()?;
    let (sender, receiver) = mpsc::channel();
    let handler = MacOpenFileHandler::new(mtm, sender);
    let manager = NSAppleEventManager::sharedAppleEventManager();
    // Register with Foundation's Apple Event manager instead of replacing
    // NSApplication.delegate, which is owned and verified by Winit.
    unsafe {
      manager.setEventHandler_andSelector_forEventClass_andEventID(
        &*handler as &AnyObject,
        sel!(handleAppleEvent:withReplyEvent:),
        CORE_EVENT_CLASS,
        OPEN_DOCUMENTS_EVENT_ID,
      );
    }
    Some(Self { receiver, manager, _handler: handler })
  }

  pub fn try_recv(&self) -> Option<Vec<PathBuf>> {
    self.receiver.try_recv().ok()
  }
}

#[cfg(target_os = "macos")]
impl Drop for OpenFileBridge {
  fn drop(&mut self) {
    self
      .manager
      .removeEventHandlerForEventClass_andEventID(CORE_EVENT_CLASS, OPEN_DOCUMENTS_EVENT_ID);
  }
}

#[cfg(not(target_os = "macos"))]
#[derive(Default)]
pub struct OpenFileBridge;

#[cfg(not(target_os = "macos"))]
impl OpenFileBridge {
  pub fn install() -> Option<Self> {
    None
  }

  pub fn try_recv(&self) -> Option<Vec<PathBuf>> {
    None
  }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct MacOpenFileHandlerIvars {
  sender: mpsc::Sender<Vec<PathBuf>>,
}

#[cfg(target_os = "macos")]
objc2::define_class!(
  // SAFETY: NSObject has no extra subclassing requirements for this event handler.
  #[unsafe(super = NSObject)]
  #[thread_kind = MainThreadOnly]
  #[ivars = MacOpenFileHandlerIvars]
  struct MacOpenFileHandler;

  // SAFETY: NSObjectProtocol has no extra safety requirements.
  unsafe impl NSObjectProtocol for MacOpenFileHandler {}

  impl MacOpenFileHandler {
    #[unsafe(method(handleAppleEvent:withReplyEvent:))]
    fn handle_apple_event(
      &self,
      event: &NSAppleEventDescriptor,
      _reply_event: &NSAppleEventDescriptor,
    ) {
      let paths = file_paths_from_apple_event(event);
      if !paths.is_empty() {
        let _ = self.ivars().sender.send(paths);
      }
    }
  }
);

#[cfg(target_os = "macos")]
impl MacOpenFileHandler {
  fn new(mtm: MainThreadMarker, sender: mpsc::Sender<Vec<PathBuf>>) -> Retained<Self> {
    let this = Self::alloc(mtm).set_ivars(MacOpenFileHandlerIvars { sender });
    unsafe { msg_send![super(this), init] }
  }
}

#[cfg(target_os = "macos")]
fn file_paths_from_apple_event(event: &NSAppleEventDescriptor) -> Vec<PathBuf> {
  let Some(direct_object) = event.paramDescriptorForKeyword(DIRECT_OBJECT_KEYWORD) else {
    return Vec::new();
  };
  let mut paths = Vec::new();
  if let Some(url) = direct_object.fileURLValue() {
    append_file_url_path(&mut paths, &url);
    return paths;
  }
  let item_count = direct_object.numberOfItems();
  if item_count <= 0 {
    return paths;
  }
  for index in 1..=item_count {
    if let Some(descriptor) = direct_object.descriptorAtIndex(index)
      && let Some(url) = descriptor.fileURLValue()
    {
      append_file_url_path(&mut paths, &url);
    }
  }
  paths
}

#[cfg(target_os = "macos")]
fn append_file_url_path(paths: &mut Vec<PathBuf>, url: &NSURL) {
  if !url.isFileURL() {
    return;
  }
  let Some(path) = url.path() else {
    return;
  };
  paths.push(PathBuf::from(path.to_string()));
}

#[cfg(test)]
mod tests {
  use super::*;

  const HOTKEY_ID: u32 = 42;

  #[test]
  fn maps_only_matching_pressed_event_to_capture_request() {
    let pressed = GlobalHotKeyEvent { id: HOTKEY_ID, state: HotKeyState::Pressed };
    let released = GlobalHotKeyEvent { id: HOTKEY_ID, state: HotKeyState::Released };
    let other = GlobalHotKeyEvent { id: HOTKEY_ID + 1, state: HotKeyState::Pressed };

    assert_eq!(map_hotkey_event(HOTKEY_ID, pressed), Some(PlatformEvent::CaptureRequested));
    assert_eq!(map_hotkey_event(HOTKEY_ID, released), None);
    assert_eq!(map_hotkey_event(HOTKEY_ID, other), None);
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn converts_fractional_and_negative_global_coordinates() {
    assert_eq!(global_coordinate_to_i32(12.9), Some(12));
    assert_eq!(global_coordinate_to_i32(-0.1), Some(-1));
    assert_eq!(global_coordinate_to_i32(f64::NAN), None);
  }
}
