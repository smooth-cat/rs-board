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
const CAPTURE_OVERLAY_TITLE: &str = "RS Board Capture Overlay";

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
    ViewportBuilder::default()
      .with_title(CAPTURE_OVERLAY_TITLE)
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
      .with_window_level(WindowLevel::AlwaysOnTop)
      .with_mouse_passthrough(!visible)
      .with_active(visible)
      .with_visible(visible)
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
  overlay_window_configured: bool,
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
      overlay_window_configured: false,
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
    #[cfg(target_os = "macos")]
    {
      self.overlay_window_configured = false;
      if let Some(panel) = self.frozen_panel.as_ref() {
        panel.present();
      }
    }
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
    {
      self.overlay_window_configured = false;
      if let Some(panel) = self.frozen_panel.as_ref() {
        panel.hide();
      }
    }
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

  pub fn configure_active_overlay_window(&mut self) {
    #[cfg(target_os = "macos")]
    if let Some(surface) = self.active_display_id.and_then(|id| self.surfaces.get_mut(&id))
      && !surface.overlay_window_configured
      && macos::configure_overlay_window(surface.display.display_id)
    {
      surface.overlay_window_configured = true;
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
}

#[cfg(target_os = "macos")]
impl FocusRestore {
  fn capture() -> Self {
    use objc2_app_kit::NSWorkspace;

    Self { application: NSWorkspace::sharedWorkspace().frontmostApplication() }
  }

  fn restore(&mut self) {
    use objc2_app_kit::NSApplicationActivationOptions;

    let Some(application) = self.application.take() else {
      return;
    };
    if !application.isTerminated() {
      #[allow(deprecated)]
      let _ =
        application.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
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
    NSApplication, NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSPanel, NSScreen,
    NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
  };
  use objc2_foundation::{NSNumber, NSString};
  use objc2_quartz_core::{CALayer, CATransaction, kCAGravityResize};

  use super::{CAPTURE_OVERLAY_TITLE, CaptureSurfaceError, DisplaySnapshot};
  use crate::capture::NativeCaptureImage;

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
      panel.setLevel(NSFloatingWindowLevel - 1);
      panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
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
      self.panel.orderFrontRegardless();
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

  pub(super) fn configure_overlay_window(display_id: u32) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
      return false;
    };
    let Some(screen) = screen_for_display(mtm, display_id) else {
      return false;
    };
    let application = NSApplication::sharedApplication(mtm);
    let Some(window) = application
      .windows()
      .into_iter()
      .find(|window| window.title().to_string() == CAPTURE_OVERLAY_TITLE)
    else {
      return false;
    };

    window.setStyleMask(NSWindowStyleMask::Borderless);
    window.setFrame_display(screen.frame(), false);
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(false);
    window.setIgnoresMouseEvents(false);
    window.setLevel(NSFloatingWindowLevel);
    window.setAnimationBehavior(NSWindowAnimationBehavior::None);
    window.setCollectionBehavior(
      NSWindowCollectionBehavior::CanJoinAllSpaces
        | NSWindowCollectionBehavior::Stationary
        | NSWindowCollectionBehavior::IgnoresCycle
        | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    window.setSharingType(NSWindowSharingType::None);
    window.orderFrontRegardless();
    true
  }

  impl Drop for FrozenImagePanel {
    fn drop(&mut self) {
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
