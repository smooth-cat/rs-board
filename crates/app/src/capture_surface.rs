use std::{
  collections::{HashMap, HashSet},
  time::{Duration, Instant, SystemTime},
};

use eframe::egui::{self, ViewportBuilder, ViewportId, WindowLevel};
use thiserror::Error;
use uuid::Uuid;
use xcap::Monitor;

use crate::capture::NativeCaptureImage;

const DISPLAY_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const COLD_REFRESH_WALL_CLOCK_GAP: Duration = Duration::from_secs(5);
const CAPTURE_OVERLAY_TITLE_PREFIX: &str = "RS Board Capture Overlay";

fn capture_overlay_title(display_id: u32) -> String {
  format!("{CAPTURE_OVERLAY_TITLE_PREFIX} [{display_id}]")
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplaySnapshot {
  pub display_id: u32,
  pub bounds_global: [i32; 4],
  pub scale_factor: f32,
}

impl DisplaySnapshot {
  pub fn from_monitor(monitor: &Monitor) -> Result<Self, CaptureSurfaceError> {
    let width = monitor.width().map_err(CaptureSurfaceError::monitor)?;
    let height = monitor.height().map_err(CaptureSurfaceError::monitor)?;
    let width = i32::try_from(width)
      .map_err(|_| CaptureSurfaceError::InvalidDisplay("display width is too large".into()))?;
    let height = i32::try_from(height)
      .map_err(|_| CaptureSurfaceError::InvalidDisplay("display height is too large".into()))?;
    let snapshot = Self {
      display_id: monitor.id().map_err(CaptureSurfaceError::monitor)?,
      bounds_global: [
        monitor.x().map_err(CaptureSurfaceError::monitor)?,
        monitor.y().map_err(CaptureSurfaceError::monitor)?,
        width,
        height,
      ],
      scale_factor: monitor.scale_factor().map_err(CaptureSurfaceError::monitor)?,
    };
    snapshot.validate()?;
    Ok(snapshot)
  }

  pub fn validate(self) -> Result<(), CaptureSurfaceError> {
    if self.bounds_global[2] <= 0 || self.bounds_global[3] <= 0 {
      return Err(CaptureSurfaceError::InvalidDisplay(
        "display bounds must have positive dimensions".into(),
      ));
    }
    if !self.scale_factor.is_finite() || self.scale_factor <= 0.0 {
      return Err(CaptureSurfaceError::InvalidDisplay(
        "display scale factor must be finite and positive".into(),
      ));
    }
    Ok(())
  }

  pub fn contains(self, point: [i32; 2]) -> bool {
    let [x, y, width, height] = self.bounds_global;
    point[0] >= x
      && point[0] < x.saturating_add(width)
      && point[1] >= y
      && point[1] < y.saturating_add(height)
  }

  fn viewport_builder(self, lifecycle: SurfaceLifecycle) -> ViewportBuilder {
    let [x, y, width, height] = self.bounds_global;
    let visible = !matches!(lifecycle, SurfaceLifecycle::Hidden);
    let builder = ViewportBuilder::default()
      .with_title(capture_overlay_title(self.display_id))
      .with_app_id("com.linjiajian.rs-board.capture-overlay")
      .with_position(egui::pos2(x as f32, y as f32))
      .with_inner_size(egui::vec2(width as f32, height as f32))
      .with_min_inner_size(egui::vec2(width as f32, height as f32))
      .with_max_inner_size(egui::vec2(width as f32, height as f32))
      .with_clamp_size_to_monitor_size(false)
      .with_decorations(false)
      .with_resizable(false)
      .with_transparent(true)
      .with_has_shadow(false)
      .with_close_button(false)
      .with_minimize_button(false)
      .with_maximize_button(false)
      .with_mouse_passthrough(!visible)
      .with_active(visible)
      .with_visible(visible);
    if cfg!(target_os = "macos") {
      builder
    } else {
      builder.with_window_level(WindowLevel::AlwaysOnTop)
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceLifecycle {
  Hidden,
  Presenting { request_id: Uuid },
  Editing { session_id: Uuid },
}

#[derive(Clone)]
pub struct SurfaceViewport {
  pub display: DisplaySnapshot,
  pub lifecycle: SurfaceLifecycle,
  pub viewport_id: ViewportId,
  pub builder: ViewportBuilder,
}

impl SurfaceViewport {
  pub fn is_active(&self) -> bool {
    !matches!(self.lifecycle, SurfaceLifecycle::Hidden)
  }
}

struct DisplayCaptureSurface {
  display: DisplaySnapshot,
  lifecycle: SurfaceLifecycle,
  pending_display: Option<DisplaySnapshot>,
  #[cfg(target_os = "macos")]
  frozen_panel: Option<macos::FrozenImagePanel>,
  #[cfg(target_os = "macos")]
  overlay_window: Option<macos::OverlayWindow>,
}

impl DisplayCaptureSurface {
  fn new(display: DisplaySnapshot) -> Self {
    Self {
      display,
      lifecycle: SurfaceLifecycle::Hidden,
      pending_display: None,
      #[cfg(target_os = "macos")]
      frozen_panel: macos::FrozenImagePanel::new(display).ok(),
      #[cfg(target_os = "macos")]
      overlay_window: None,
    }
  }

  fn update_display(&mut self, display: DisplaySnapshot) {
    if self.lifecycle == SurfaceLifecycle::Hidden {
      self.display = display;
      #[cfg(target_os = "macos")]
      if let Some(panel) = self.frozen_panel.as_mut() {
        panel.update_display(display);
      }
    } else if self.display != display {
      self.pending_display = Some(display);
    }
  }

  fn present(&mut self, request_id: Uuid) {
    self.lifecycle = SurfaceLifecycle::Presenting { request_id };
  }

  fn set_frozen_image(&mut self, image: &NativeCaptureImage) -> Result<(), CaptureSurfaceError> {
    #[cfg(target_os = "macos")]
    {
      let panel = self.frozen_panel.as_ref().ok_or_else(|| {
        CaptureSurfaceError::Native(format!(
          "native capture panel for display {} is unavailable",
          self.display.display_id
        ))
      })?;
      panel.set_image(image);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = image;
    Ok(())
  }

  fn begin_editing(&mut self, session_id: Uuid) {
    self.lifecycle = SurfaceLifecycle::Editing { session_id };
  }

  fn hide(&mut self) {
    self.lifecycle = SurfaceLifecycle::Hidden;
    #[cfg(target_os = "macos")]
    macos::hide_window_pair(
      self.display.display_id,
      &mut self.overlay_window,
      self.frozen_panel.as_ref(),
    );
    if let Some(display) = self.pending_display.take() {
      self.update_display(display);
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayRefreshOutcome {
  Unchanged,
  DisplaysChanged,
  ActiveDisplayRemoved(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayWindowReadiness {
  Ready,
  Pending(&'static str),
  Failed,
}

pub struct CaptureSurfaceCoordinator {
  surfaces: HashMap<u32, DisplayCaptureSurface>,
  active_display_id: Option<u32>,
  retained_display_id: Option<u32>,
  last_refresh: Instant,
  last_refresh_wall_clock: SystemTime,
  focus_restore: FocusRestore,
}

impl CaptureSurfaceCoordinator {
  pub fn discover() -> Result<Self, CaptureSurfaceError> {
    let displays = available_displays()?;
    let mut coordinator = Self {
      surfaces: HashMap::new(),
      active_display_id: None,
      retained_display_id: None,
      last_refresh: Instant::now(),
      last_refresh_wall_clock: SystemTime::now(),
      focus_restore: FocusRestore::default(),
    };
    coordinator.refresh(displays);
    Ok(coordinator)
  }

  pub fn should_refresh(&self) -> bool {
    self.last_refresh.elapsed() >= DISPLAY_REFRESH_INTERVAL
      || SystemTime::now()
        .duration_since(self.last_refresh_wall_clock)
        .is_ok_and(|elapsed| elapsed >= COLD_REFRESH_WALL_CLOCK_GAP)
  }

  pub fn refresh_available_displays(
    &mut self,
  ) -> Result<DisplayRefreshOutcome, CaptureSurfaceError> {
    self.last_refresh = Instant::now();
    Ok(self.refresh_after_wall_clock_gap(available_displays()?, SystemTime::now()))
  }

  fn refresh_after_wall_clock_gap(
    &mut self,
    displays: Vec<DisplaySnapshot>,
    now: SystemTime,
  ) -> DisplayRefreshOutcome {
    let cold_refresh = now
      .duration_since(self.last_refresh_wall_clock)
      .is_ok_and(|elapsed| elapsed >= COLD_REFRESH_WALL_CLOCK_GAP);
    self.last_refresh_wall_clock = now;
    let outcome = self.refresh(displays);
    if !cold_refresh {
      return outcome;
    }
    for surface in self.surfaces.values_mut() {
      if surface.lifecycle == SurfaceLifecycle::Hidden {
        *surface = DisplayCaptureSurface::new(surface.display);
      }
    }
    match outcome {
      DisplayRefreshOutcome::ActiveDisplayRemoved(_) => outcome,
      DisplayRefreshOutcome::Unchanged | DisplayRefreshOutcome::DisplaysChanged => {
        DisplayRefreshOutcome::DisplaysChanged
      }
    }
  }

  pub fn refresh(&mut self, displays: Vec<DisplaySnapshot>) -> DisplayRefreshOutcome {
    let changed = displays.len() != self.surfaces.len()
      || displays.iter().any(|display| {
        self.surfaces.get(&display.display_id).is_none_or(|surface| surface.display != *display)
      });
    let available_ids: HashSet<_> = displays.iter().map(|display| display.display_id).collect();
    let removed_active =
      self.active_display_id.filter(|display_id| !available_ids.contains(display_id));

    for display in displays {
      self
        .surfaces
        .entry(display.display_id)
        .and_modify(|surface| surface.update_display(display))
        .or_insert_with(|| DisplayCaptureSurface::new(display));
    }
    self.surfaces.retain(|display_id, surface| {
      available_ids.contains(display_id)
        || Some(*display_id) == self.active_display_id
        || Some(*display_id) == self.retained_display_id
        || surface.lifecycle != SurfaceLifecycle::Hidden
    });

    if let Some(display_id) = removed_active {
      DisplayRefreshOutcome::ActiveDisplayRemoved(display_id)
    } else if changed {
      DisplayRefreshOutcome::DisplaysChanged
    } else {
      DisplayRefreshOutcome::Unchanged
    }
  }

  pub fn remember_frontmost_application(&mut self) {
    self.focus_restore = FocusRestore::capture();
  }

  pub fn present(&mut self, display_id: u32, request_id: Uuid) -> Result<(), CaptureSurfaceError> {
    if let Some(active) = self.active_display_id
      && active != display_id
    {
      return Err(CaptureSurfaceError::Busy(active));
    }
    let surface = self
      .surfaces
      .get_mut(&display_id)
      .ok_or(CaptureSurfaceError::DisplayUnavailable(display_id))?;
    surface.present(request_id);
    if self.retained_display_id == Some(display_id) {
      self.retained_display_id = None;
    }
    self.active_display_id = Some(display_id);
    Ok(())
  }

  pub fn set_frozen_image(
    &mut self,
    display_id: u32,
    image: &NativeCaptureImage,
  ) -> Result<(), CaptureSurfaceError> {
    let surface = self
      .surfaces
      .get_mut(&display_id)
      .ok_or(CaptureSurfaceError::DisplayUnavailable(display_id))?;
    surface.set_frozen_image(image)
  }

  pub fn exclude_application_windows(&self) {
    #[cfg(target_os = "macos")]
    macos::exclude_application_windows();
  }

  pub fn begin_editing(
    &mut self,
    display_id: u32,
    session_id: Uuid,
  ) -> Result<(), CaptureSurfaceError> {
    if self.active_display_id != Some(display_id) {
      return Err(CaptureSurfaceError::DisplayUnavailable(display_id));
    }
    let surface = self
      .surfaces
      .get_mut(&display_id)
      .ok_or(CaptureSurfaceError::DisplayUnavailable(display_id))?;
    surface.begin_editing(session_id);
    Ok(())
  }

  pub fn display_for_bounds(&self, bounds: [i32; 4]) -> Option<u32> {
    self
      .surfaces
      .values()
      .find(|surface| surface.display.bounds_global == bounds)
      .map(|surface| surface.display.display_id)
  }

  pub fn display_for_point(&self, point: [i32; 2]) -> Option<u32> {
    self
      .surfaces
      .values()
      .find(|surface| surface.display.contains(point))
      .map(|surface| surface.display.display_id)
  }

  pub fn fallback_display(&self) -> Option<u32> {
    self.surfaces.keys().copied().min()
  }

  pub fn active_display_id(&self) -> Option<u32> {
    self.active_display_id
  }

  pub fn hide_active(&mut self) {
    if let Some(display_id) = self.active_display_id.take()
      && let Some(surface) = self.surfaces.get_mut(&display_id)
    {
      surface.hide();
      self.retained_display_id = Some(display_id);
    }
    self.focus_restore.restore();
  }

  pub fn viewport_specs(&self) -> Vec<SurfaceViewport> {
    let mut viewports: Vec<_> = self
      .surfaces
      .values()
      .map(|surface| SurfaceViewport {
        display: surface.display,
        lifecycle: surface.lifecycle,
        viewport_id: ViewportId::from_hash_of(("capture-overlay", surface.display.display_id)),
        builder: surface.display.viewport_builder(surface.lifecycle),
      })
      .collect();
    viewports.sort_by_key(|viewport| viewport.display.display_id);
    viewports
  }

  pub fn viewport_specs_for_frame(&self) -> Vec<SurfaceViewport> {
    let mut viewports = Vec::with_capacity(2);

    if let Some(display_id) = self.retained_display_id
      && Some(display_id) != self.active_display_id
      && let Some(surface) = self.surfaces.get(&display_id)
    {
      viewports.push(retained_surface_viewport(surface));
    }

    if let Some(surface) = self.active_display_id.and_then(|id| self.surfaces.get(&id))
      && surface.lifecycle != SurfaceLifecycle::Hidden
    {
      // Keep the active viewport last so the current GL context always belongs to a viewport
      // that eframe will retain on the following frame.
      viewports.push(surface_viewport(surface));
    }

    viewports
  }

  pub fn configure_active_overlay_window(&mut self) -> OverlayWindowReadiness {
    #[cfg(target_os = "macos")]
    {
      let Some(surface) = self.active_display_id.and_then(|id| self.surfaces.get_mut(&id)) else {
        return OverlayWindowReadiness::Failed;
      };
      let Some(panel) = surface.frozen_panel.as_ref() else {
        return OverlayWindowReadiness::Failed;
      };
      macos::configure_active_window_pair(surface.display, &mut surface.overlay_window, panel)
    }

    #[cfg(not(target_os = "macos"))]
    {
      if self.active_display_id.is_some() {
        OverlayWindowReadiness::Ready
      } else {
        OverlayWindowReadiness::Failed
      }
    }
  }
}

impl Default for CaptureSurfaceCoordinator {
  fn default() -> Self {
    Self {
      surfaces: HashMap::new(),
      active_display_id: None,
      retained_display_id: None,
      last_refresh: Instant::now(),
      last_refresh_wall_clock: SystemTime::now(),
      focus_restore: FocusRestore::default(),
    }
  }
}

fn surface_viewport(surface: &DisplayCaptureSurface) -> SurfaceViewport {
  SurfaceViewport {
    display: surface.display,
    lifecycle: surface.lifecycle,
    viewport_id: ViewportId::from_hash_of(("capture-overlay", surface.display.display_id)),
    builder: surface.display.viewport_builder(surface.lifecycle),
  }
}

fn retained_surface_viewport(surface: &DisplayCaptureSurface) -> SurfaceViewport {
  let mut viewport = surface_viewport(surface);
  // On macOS, hiding the current native window clears NSOpenGLContext.view before eframe can
  // switch back to the root surface. Keep the retained window transparent and click-through.
  // Changing `active` recreates the eframe 0.36 native viewport. Preserve the active builder
  // value so the current CGL surface survives until the root viewport has rendered.
  viewport.builder =
    viewport.builder.with_visible(true).with_active(true).with_mouse_passthrough(true);
  viewport
}

fn available_displays() -> Result<Vec<DisplaySnapshot>, CaptureSurfaceError> {
  Monitor::all()
    .map_err(CaptureSurfaceError::monitor)?
    .iter()
    .map(DisplaySnapshot::from_monitor)
    .collect()
}

#[derive(Debug, Error)]
pub enum CaptureSurfaceError {
  #[error("display discovery failed: {0}")]
  Monitor(String),
  #[error("invalid display metadata: {0}")]
  InvalidDisplay(String),
  #[error("display {0} is no longer available")]
  DisplayUnavailable(u32),
  #[error("capture surface for display {0} is already active")]
  Busy(u32),
  #[error("native capture surface failed: {0}")]
  Native(String),
}

impl CaptureSurfaceError {
  fn monitor(error: xcap::XCapError) -> Self {
    Self::Monitor(error.to_string())
  }
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct FocusRestore {
  application: Option<objc2::rc::Retained<objc2_app_kit::NSRunningApplication>>,
  own_key_window: Option<objc2::rc::Retained<objc2_app_kit::NSWindow>>,
}

#[cfg(target_os = "macos")]
impl FocusRestore {
  fn capture() -> Self {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSRunningApplication, NSWorkspace};

    let application = NSWorkspace::sharedWorkspace().frontmostApplication();
    let current_pid = NSRunningApplication::currentApplication().processIdentifier();
    let own_key_window = application
      .as_ref()
      .filter(|application| application.processIdentifier() == current_pid)
      .and_then(|_| MainThreadMarker::new())
      .and_then(|mtm| NSApplication::sharedApplication(mtm).keyWindow());
    Self { application, own_key_window }
  }

  fn restore(&mut self) {
    use objc2_app_kit::NSApplicationActivationOptions;

    let own_key_window = self.own_key_window.take();
    let Some(application) = self.application.take() else {
      return;
    };
    if !application.isTerminated() {
      #[allow(deprecated)]
      let _ =
        application.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
      if let Some(window) = own_key_window {
        window.makeKeyAndOrderFront(None);
      }
    }
  }
}

#[cfg(not(target_os = "macos"))]
#[derive(Default)]
struct FocusRestore;

#[cfg(not(target_os = "macos"))]
impl FocusRestore {
  fn capture() -> Self {
    Self
  }

  fn restore(&mut self) {}
}

#[cfg(target_os = "macos")]
mod macos {
  use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained, runtime::AnyObject};
  use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSNormalWindowLevel, NSPanel, NSScreen,
    NSScreenSaverWindowLevel, NSWindow, NSWindowAnimationBehavior, NSWindowCollectionBehavior,
    NSWindowLevel, NSWindowOrderingMode, NSWindowSharingType, NSWindowStyleMask,
  };
  use objc2_foundation::{NSNumber, NSRect, NSString};
  use objc2_quartz_core::{CALayer, CATransaction, kCAGravityResize};

  use super::{
    CaptureSurfaceError, DisplaySnapshot, OverlayWindowReadiness, capture_overlay_title,
  };
  use crate::capture::NativeCaptureImage;

  const EDITOR_WINDOW_SHARING_TYPE: NSWindowSharingType = NSWindowSharingType::ReadOnly;

  pub(super) struct OverlayWindow {
    window: Retained<NSWindow>,
    title: String,
    ready: bool,
  }

  pub(super) struct FrozenImagePanel {
    panel: Retained<NSPanel>,
    layer: Retained<CALayer>,
  }

  impl FrozenImagePanel {
    pub(super) fn new(display: DisplaySnapshot) -> Result<Self, CaptureSurfaceError> {
      let mtm = MainThreadMarker::new().ok_or_else(|| {
        CaptureSurfaceError::Native("capture panels must be created on the main thread".into())
      })?;
      let screen = screen_for_display(mtm, display.display_id).ok_or_else(|| {
        CaptureSurfaceError::Native(format!("NSScreen {} is unavailable", display.display_id))
      })?;
      let frame = screen.frame();
      let panel = NSPanel::initWithContentRect_styleMask_backing_defer_screen(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
        Some(&screen),
      );
      panel.setOpaque(true);
      panel.setBackgroundColor(Some(&NSColor::blackColor()));
      panel.setHasShadow(false);
      panel.setIgnoresMouseEvents(true);
      panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
      panel.setCanHide(false);
      panel.setHidesOnDeactivate(false);
      panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
          | NSWindowCollectionBehavior::Stationary
          | NSWindowCollectionBehavior::IgnoresCycle
          | NSWindowCollectionBehavior::FullScreenAuxiliary,
      );
      panel.setSharingType(NSWindowSharingType::None);
      unsafe { panel.setReleasedWhenClosed(false) };
      panel.setFloatingPanel(false);
      panel.setBecomesKeyOnlyIfNeeded(true);
      panel.setLevel(frozen_panel_window_level());

      let view = panel
        .contentView()
        .ok_or_else(|| CaptureSurfaceError::Native("capture panel has no content view".into()))?;
      let layer = CALayer::layer();
      layer.setContentsScale(display.scale_factor as f64);
      layer.setContentsGravity(unsafe { kCAGravityResize });
      view.setWantsLayer(true);
      view.setLayer(Some(&layer));
      panel.orderOut(None);
      Ok(Self { panel, layer })
    }

    pub(super) fn update_display(&mut self, display: DisplaySnapshot) {
      let Some(mtm) = MainThreadMarker::new() else {
        return;
      };
      if let Some(screen) = screen_for_display(mtm, display.display_id) {
        self.panel.setFrame_display(screen.frame(), false);
        self.layer.setContentsScale(display.scale_factor as f64);
      }
    }

    pub(super) fn set_image(&self, image: &NativeCaptureImage) {
      let contents = unsafe { &*(image.cg_image() as *const _ as *const AnyObject) };
      CATransaction::begin();
      CATransaction::setDisableActions(true);
      unsafe { self.layer.setContents(Some(contents)) };
      CATransaction::commit();
      CATransaction::flush();
    }

    pub(super) fn present(&self) {
      self.panel.setLevel(frozen_panel_window_level());
      self.panel.orderFront(None);
    }

    pub(super) fn hide(&self) {
      self.panel.orderOut(None);
      CATransaction::begin();
      CATransaction::setDisableActions(true);
      unsafe { self.layer.setContents(None) };
      CATransaction::commit();
    }
  }

  pub(super) fn exclude_application_windows() {
    let Some(mtm) = MainThreadMarker::new() else {
      return;
    };
    for window in NSApplication::sharedApplication(mtm).windows() {
      window.setSharingType(NSWindowSharingType::None);
    }
  }

  pub(super) fn configure_active_window_pair(
    display: DisplaySnapshot,
    overlay_window: &mut Option<OverlayWindow>,
    frozen_panel: &FrozenImagePanel,
  ) -> OverlayWindowReadiness {
    let Some(mtm) = MainThreadMarker::new() else {
      return OverlayWindowReadiness::Failed;
    };
    let Some(screen) = screen_for_display(mtm, display.display_id) else {
      conceal_window_pair(overlay_window.as_mut(), frozen_panel);
      return OverlayWindowReadiness::Failed;
    };
    let application = NSApplication::sharedApplication(mtm);
    acquire_overlay_window(mtm, display.display_id, overlay_window);
    let Some(overlay_window) = overlay_window.as_mut() else {
      conceal_frozen_panel(frozen_panel);
      return OverlayWindowReadiness::Pending("overlay_window_unavailable");
    };
    let window = &overlay_window.window;
    allow_window_pair_capture(window, frozen_panel);
    if overlay_window.ready
      && window_pair_is_ready(&application, &screen, window, frozen_panel, true)
      && overlay_input_target_is_ready(true, content_view_is_first_responder(window))
    {
      return OverlayWindowReadiness::Ready;
    }
    overlay_window.ready = false;

    if window.styleMask() != NSWindowStyleMask::Borderless {
      window.setStyleMask(NSWindowStyleMask::Borderless);
    }
    // Winit's content view owns key and IME input. Keep it retained while AppKit finishes moving
    // the window to the target display; a cross-DPI frame change can clear the first responder.
    let Some(content_view) = window.contentView() else {
      prepare_window_pair_retry(window, frozen_panel);
      return OverlayWindowReadiness::Pending("content_view_unavailable");
    };
    window.setFrame_display(screen.frame(), false);
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(false);
    window.setIgnoresMouseEvents(true);
    window.setCanHide(false);
    window.setHidesOnDeactivate(false);
    window.setLevel(editor_overlay_window_level());
    window.setAnimationBehavior(NSWindowAnimationBehavior::None);
    window.setCollectionBehavior(
      NSWindowCollectionBehavior::CanJoinAllSpaces
        | NSWindowCollectionBehavior::Stationary
        | NSWindowCollectionBehavior::IgnoresCycle
        | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );

    // Do not reveal the frozen image until the editor window can receive the escape/cancel input.
    #[allow(deprecated)]
    application.activateIgnoringOtherApps(true);
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
    if !application.isActive() {
      prepare_window_pair_retry(window, frozen_panel);
      return OverlayWindowReadiness::Pending("application_inactive");
    }
    if !window.isVisible() {
      prepare_window_pair_retry(window, frozen_panel);
      return OverlayWindowReadiness::Pending("overlay_window_invisible");
    }
    if !window.isKeyWindow() {
      prepare_window_pair_retry(window, frozen_panel);
      return OverlayWindowReadiness::Pending("overlay_window_not_key");
    }

    attach_frozen_panel_below(window, frozen_panel, screen.frame());
    frozen_panel.present();
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
    if !window.makeFirstResponder(Some(&content_view)) {
      prepare_window_pair_retry(window, frozen_panel);
      return OverlayWindowReadiness::Pending("first_responder_rejected");
    }

    let pending_reason =
      window_pair_pending_reason(&application, &screen, window, frozen_panel, false).or_else(
        || {
          (!overlay_input_target_is_ready(false, content_view_is_first_responder(window)))
            .then_some("content_view_not_first_responder")
        },
      );
    if let Some(reason) = pending_reason {
      prepare_window_pair_retry(window, frozen_panel);
      OverlayWindowReadiness::Pending(reason)
    } else {
      window.setIgnoresMouseEvents(false);
      overlay_window.ready = true;
      OverlayWindowReadiness::Ready
    }
  }

  pub(super) fn hide_window_pair(
    display_id: u32,
    overlay_window: &mut Option<OverlayWindow>,
    frozen_panel: Option<&FrozenImagePanel>,
  ) {
    let Some(mtm) = MainThreadMarker::new() else {
      return;
    };
    acquire_overlay_window(mtm, display_id, overlay_window);
    if let Some(overlay_window) = overlay_window.as_mut() {
      overlay_window.ready = false;
      overlay_window.window.setIgnoresMouseEvents(true);
      if overlay_window.window.isKeyWindow() {
        overlay_window.window.resignKeyWindow();
      }
    }
    if let Some(frozen_panel) = frozen_panel {
      detach_frozen_panel(frozen_panel);
      frozen_panel.hide();
    }
    if let Some(overlay_window) = overlay_window.as_ref() {
      demote_overlay_window(&overlay_window.window);
    }
  }

  fn frozen_panel_window_level() -> NSWindowLevel {
    NSScreenSaverWindowLevel
  }

  fn editor_overlay_window_level() -> NSWindowLevel {
    NSScreenSaverWindowLevel
  }

  fn inactive_overlay_window_level() -> NSWindowLevel {
    NSNormalWindowLevel
  }

  fn attach_frozen_panel_below(
    window: &NSWindow,
    frozen_panel: &FrozenImagePanel,
    desired_frame: NSRect,
  ) {
    if frozen_panel_is_child_of(window, frozen_panel) {
      frozen_panel.panel.setFrame_display(desired_frame, false);
      return;
    }
    detach_frozen_panel(frozen_panel);
    // Both retained windows are main-thread AppKit objects and are detached again on every exit.
    unsafe {
      window.addChildWindow_ordered(&frozen_panel.panel, NSWindowOrderingMode::Below);
    }
    // AppKit offsets the panel's detached frame when establishing the child relationship.
    // Apply the final screen-space frame only after the panel is attached.
    frozen_panel.panel.setFrame_display(desired_frame, false);
  }

  fn detach_frozen_panel(frozen_panel: &FrozenImagePanel) {
    if let Some(parent) = frozen_panel.panel.parentWindow() {
      parent.removeChildWindow(&frozen_panel.panel);
    }
  }

  fn conceal_frozen_panel(frozen_panel: &FrozenImagePanel) {
    detach_frozen_panel(frozen_panel);
    frozen_panel.panel.orderOut(None);
  }

  fn conceal_window_pair(
    overlay_window: Option<&mut OverlayWindow>,
    frozen_panel: &FrozenImagePanel,
  ) {
    conceal_frozen_panel(frozen_panel);
    if let Some(overlay_window) = overlay_window {
      overlay_window.ready = false;
      demote_overlay_window(&overlay_window.window);
    }
  }

  fn prepare_window_pair_retry(window: &NSWindow, frozen_panel: &FrozenImagePanel) {
    window.setIgnoresMouseEvents(true);
    conceal_frozen_panel(frozen_panel);
  }

  fn allow_window_pair_capture(window: &NSWindow, frozen_panel: &FrozenImagePanel) {
    window.setSharingType(EDITOR_WINDOW_SHARING_TYPE);
    frozen_panel.panel.setSharingType(EDITOR_WINDOW_SHARING_TYPE);
  }

  fn demote_overlay_window(window: &NSWindow) {
    window.setIgnoresMouseEvents(true);
    if window.isKeyWindow() {
      window.resignKeyWindow();
    }
    window.setLevel(inactive_overlay_window_level());
    window.orderBack(None);
  }

  fn frozen_panel_is_child_of(window: &NSWindow, frozen_panel: &FrozenImagePanel) -> bool {
    frozen_panel
      .panel
      .parentWindow()
      .is_some_and(|parent| Retained::as_ptr(&parent) == std::ptr::from_ref(window))
  }

  fn window_pair_is_ready(
    application: &NSApplication,
    screen: &NSScreen,
    window: &NSWindow,
    frozen_panel: &FrozenImagePanel,
    was_ready: bool,
  ) -> bool {
    window_pair_is_structurally_ready(application, screen, window, frozen_panel, was_ready)
      && !window.ignoresMouseEvents()
  }

  fn content_view_is_first_responder(window: &NSWindow) -> bool {
    let Some(content_view) = window.contentView() else {
      return false;
    };
    window.firstResponder().is_some_and(|first_responder| {
      Retained::as_ptr(&first_responder).cast::<AnyObject>()
        == Retained::as_ptr(&content_view).cast::<AnyObject>()
    })
  }

  fn overlay_input_target_is_ready(was_ready: bool, content_view_is_first_responder: bool) -> bool {
    was_ready || content_view_is_first_responder
  }

  fn overlay_activation_is_ready(
    was_ready: bool,
    application_is_active: bool,
    window_is_key: bool,
  ) -> bool {
    was_ready || (application_is_active && window_is_key)
  }

  fn window_pair_is_structurally_ready(
    application: &NSApplication,
    screen: &NSScreen,
    window: &NSWindow,
    frozen_panel: &FrozenImagePanel,
    was_ready: bool,
  ) -> bool {
    window_pair_pending_reason(application, screen, window, frozen_panel, was_ready).is_none()
  }

  fn window_pair_pending_reason(
    application: &NSApplication,
    screen: &NSScreen,
    window: &NSWindow,
    frozen_panel: &FrozenImagePanel,
    was_ready: bool,
  ) -> Option<&'static str> {
    let screen_frame = screen.frame();
    if !overlay_activation_is_ready(was_ready, application.isActive(), window.isKeyWindow()) {
      if !application.isActive() {
        Some("application_inactive")
      } else {
        Some("overlay_window_not_key")
      }
    } else if !frozen_panel.panel.isVisible() {
      Some("frozen_panel_invisible")
    } else if frozen_panel.panel.level() != frozen_panel_window_level() {
      Some("frozen_panel_wrong_level")
    } else if frozen_panel.panel.frame() != screen_frame {
      Some("frozen_panel_wrong_frame")
    } else if !window.isVisible() {
      Some("overlay_window_invisible")
    } else if window.level() != editor_overlay_window_level() {
      Some("overlay_window_wrong_level")
    } else if window.frame() != screen_frame {
      Some("overlay_window_wrong_frame")
    } else if !frozen_panel_is_child_of(window, frozen_panel) {
      Some("frozen_panel_detached")
    } else {
      None
    }
  }

  fn acquire_overlay_window(
    mtm: MainThreadMarker,
    display_id: u32,
    overlay_window: &mut Option<OverlayWindow>,
  ) {
    let title = capture_overlay_title(display_id);
    let windows: Vec<_> = NSApplication::sharedApplication(mtm).windows().into_iter().collect();
    let retained_window_is_current = overlay_window.as_ref().is_some_and(|overlay_window| {
      overlay_window.title == title
        && windows.iter().any(|window| {
          Retained::as_ptr(window) == Retained::as_ptr(&overlay_window.window)
            && window.title().to_string() == title
        })
    });
    if retained_window_is_current {
      return;
    }

    if let Some(stale_window) = overlay_window.as_ref() {
      demote_overlay_window(&stale_window.window);
    }

    *overlay_window = windows
      .into_iter()
      .find(|window| window.title().to_string() == title)
      .map(|window| OverlayWindow { window, title, ready: false });
  }

  impl Drop for FrozenImagePanel {
    fn drop(&mut self) {
      detach_frozen_panel(self);
      self.panel.orderOut(None);
    }
  }

  fn screen_for_display(mtm: MainThreadMarker, display_id: u32) -> Option<Retained<NSScreen>> {
    for screen in NSScreen::screens(mtm) {
      let description = screen.deviceDescription();
      let number = description.objectForKey(&NSString::from_str("NSScreenNumber"))?;
      let number = number.downcast::<NSNumber>().ok()?;
      if number.unsignedIntValue() == display_id {
        return Some(screen);
      }
    }
    None
  }

  #[cfg(test)]
  mod tests {
    use objc2_app_kit::{NSNormalWindowLevel, NSScreenSaverWindowLevel};

    use super::{
      EDITOR_WINDOW_SHARING_TYPE, editor_overlay_window_level, frozen_panel_window_level,
      inactive_overlay_window_level, overlay_activation_is_ready, overlay_input_target_is_ready,
    };

    #[test]
    fn capture_window_levels_cover_system_ui_with_the_editor_on_top() {
      assert_eq!(frozen_panel_window_level(), NSScreenSaverWindowLevel);
      assert_eq!(editor_overlay_window_level(), frozen_panel_window_level());
      assert_eq!(inactive_overlay_window_level(), NSNormalWindowLevel);
    }

    #[test]
    fn initial_overlay_requires_input_target_without_reclaiming_it_after_ready() {
      assert!(!overlay_input_target_is_ready(false, false));
      assert!(overlay_input_target_is_ready(false, true));
      assert!(overlay_input_target_is_ready(true, false));
    }

    #[test]
    fn configured_overlay_allows_another_application_to_take_focus() {
      assert!(!overlay_activation_is_ready(false, false, false));
      assert!(!overlay_activation_is_ready(false, true, false));
      assert!(overlay_activation_is_ready(false, true, true));
      assert!(overlay_activation_is_ready(true, false, false));
    }

    #[test]
    fn editor_windows_are_visible_to_external_capture_tools() {
      assert_eq!(EDITOR_WINDOW_SHARING_TYPE, objc2_app_kit::NSWindowSharingType::ReadOnly);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn display(display_id: u32, bounds_global: [i32; 4]) -> DisplaySnapshot {
    DisplaySnapshot { display_id, bounds_global, scale_factor: 2.0 }
  }

  #[test]
  fn keeps_active_display_snapshot_until_hidden() {
    let mut coordinator = CaptureSurfaceCoordinator::default();
    coordinator.refresh(vec![display(7, [0, 0, 100, 80])]);
    coordinator.present(7, Uuid::new_v4()).unwrap();
    coordinator.refresh(vec![display(7, [10, 20, 200, 160])]);
    assert_eq!(coordinator.viewport_specs()[0].display.bounds_global, [0, 0, 100, 80]);

    coordinator.hide_active();
    assert_eq!(coordinator.viewport_specs()[0].display.bounds_global, [10, 20, 200, 160]);
  }

  #[test]
  fn reports_active_display_removal_without_dropping_surface() {
    let mut coordinator = CaptureSurfaceCoordinator::default();
    coordinator.refresh(vec![display(4, [-100, 0, 100, 80])]);
    coordinator.present(4, Uuid::new_v4()).unwrap();
    assert_eq!(coordinator.refresh(Vec::new()), DisplayRefreshOutcome::ActiveDisplayRemoved(4));
    assert_eq!(coordinator.active_display_id(), Some(4));
    assert_eq!(coordinator.viewport_specs().len(), 1);

    coordinator.hide_active();
    assert_eq!(coordinator.refresh(Vec::new()), DisplayRefreshOutcome::DisplaysChanged);
    let retained = coordinator.viewport_specs_for_frame().pop().unwrap();
    assert_eq!(retained.display.display_id, 4);
    assert_eq!(retained.lifecycle, SurfaceLifecycle::Hidden);
    assert!(coordinator.viewport_specs_for_frame().pop().is_some());
    assert_eq!(coordinator.refresh(Vec::new()), DisplayRefreshOutcome::DisplaysChanged);
    assert_eq!(coordinator.viewport_specs().len(), 1);
  }

  #[test]
  fn selects_displays_with_negative_global_coordinates() {
    let mut coordinator = CaptureSurfaceCoordinator::default();
    coordinator.refresh(vec![display(1, [-1920, 0, 1920, 1080]), display(2, [0, 0, 1728, 1117])]);
    assert_eq!(coordinator.display_for_point([-10, 20]), Some(1));
    assert_eq!(coordinator.display_for_point([100, 20]), Some(2));
  }

  #[test]
  fn overlay_titles_strongly_identify_their_display_viewports() {
    let first = display(1, [0, 0, 100, 80]).viewport_builder(SurfaceLifecycle::Hidden);
    let second = display(2, [100, 0, 100, 80]).viewport_builder(SurfaceLifecycle::Hidden);

    assert_eq!(first.title.as_deref(), Some("RS Board Capture Overlay [1]"));
    assert_eq!(second.title.as_deref(), Some("RS Board Capture Overlay [2]"));
    assert_ne!(first.title, second.title);
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn macos_overlay_builder_leaves_window_level_to_appkit() {
    let builder = display(1, [0, 0, 1920, 1080])
      .viewport_builder(SurfaceLifecycle::Editing { session_id: Uuid::new_v4() });

    assert_eq!(builder.window_level, None);
  }

  #[test]
  fn retains_the_last_egui_viewport_after_hiding() {
    let mut coordinator = CaptureSurfaceCoordinator::default();
    coordinator.refresh(vec![display(1, [0, 0, 100, 80]), display(2, [100, 0, 100, 80])]);

    assert!(coordinator.viewport_specs_for_frame().is_empty());

    coordinator.present(2, Uuid::new_v4()).unwrap();
    let viewport = coordinator.viewport_specs_for_frame().pop().unwrap();
    assert_eq!(viewport.display.display_id, 2);
    assert!(viewport.is_active());

    coordinator.hide_active();
    let retained = coordinator.viewport_specs_for_frame().pop().unwrap();
    assert_eq!(retained.display.display_id, 2);
    assert_eq!(retained.lifecycle, SurfaceLifecycle::Hidden);
    assert!(!retained.is_active());
    assert_eq!(retained.builder.visible, Some(true));
    assert_eq!(retained.builder.active, Some(true));
    assert_eq!(retained.builder.mouse_passthrough, Some(true));
    assert!(coordinator.viewport_specs_for_frame().pop().is_some());
  }

  #[test]
  fn a_new_presentation_reuses_the_retained_viewport_for_the_same_display() {
    let mut coordinator = CaptureSurfaceCoordinator::default();
    coordinator.refresh(vec![display(2, [100, 0, 100, 80])]);
    coordinator.present(2, Uuid::new_v4()).unwrap();
    coordinator.hide_active();

    coordinator.present(2, Uuid::new_v4()).unwrap();

    let viewports = coordinator.viewport_specs_for_frame();
    assert_eq!(viewports.len(), 1);
    assert_eq!(viewports[0].display.display_id, 2);
    assert!(viewports[0].is_active());
  }

  #[test]
  fn a_retained_viewport_precedes_a_new_active_viewport() {
    let mut coordinator = CaptureSurfaceCoordinator::default();
    coordinator.refresh(vec![display(1, [0, 0, 100, 80]), display(2, [100, 0, 100, 80])]);
    coordinator.present(1, Uuid::new_v4()).unwrap();
    coordinator.hide_active();
    coordinator.present(2, Uuid::new_v4()).unwrap();

    let viewports = coordinator.viewport_specs_for_frame();
    assert_eq!(viewports.len(), 2);
    assert_eq!(viewports[0].display.display_id, 1);
    assert!(!viewports[0].is_active());
    assert_eq!(viewports[1].display.display_id, 2);
    assert!(viewports[1].is_active());
  }

  #[test]
  fn long_wall_clock_gap_forces_a_cold_refresh_without_replacing_active_snapshot() {
    let mut coordinator = CaptureSurfaceCoordinator::default();
    let before_sleep = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
    coordinator.last_refresh_wall_clock = before_sleep;
    coordinator.refresh(vec![display(7, [0, 0, 100, 80]), display(9, [100, 0, 100, 80])]);
    coordinator.present(7, Uuid::new_v4()).unwrap();

    let outcome = coordinator.refresh_after_wall_clock_gap(
      vec![display(7, [10, 20, 200, 160]), display(9, [100, 0, 100, 80])],
      before_sleep + COLD_REFRESH_WALL_CLOCK_GAP,
    );

    assert_eq!(outcome, DisplayRefreshOutcome::DisplaysChanged);
    let active = coordinator
      .viewport_specs()
      .into_iter()
      .find(|viewport| viewport.display.display_id == 7)
      .unwrap();
    assert_eq!(active.display.bounds_global, [0, 0, 100, 80]);
  }
}
