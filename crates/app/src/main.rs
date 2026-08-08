use std::{
  ffi::OsStr,
  path::{Path, PathBuf},
  process::ExitCode,
};

use app::{
  application::RsBoardApp,
  instance::{InstanceBridge, InstanceError, InstanceRole},
  performance::{PerformanceLogError, PerformanceLogGuard},
  settings::{Settings, SettingsError},
};
use rfd::{MessageButtons, MessageDialog, MessageLevel};
use thiserror::Error;

const APP_NAME: &str = "RS Board";
const APP_ID: &str = "com.linjiajian.rs-board";
const SHOW_WINDOW_ARGUMENT: &str = "--show";

fn main() -> ExitCode {
  match run() {
    Ok(()) => ExitCode::SUCCESS,
    Err(error) => {
      report_startup_error(&error);
      ExitCode::FAILURE
    }
  }
}

fn run() -> Result<(), StartupError> {
  let _performance_log = PerformanceLogGuard::from_environment()?;
  let arguments = StartupArguments::from_environment();
  let app_data_dir = Settings::app_data_dir()?;
  let egui_context = native_egui_context();
  let instance =
    match InstanceBridge::acquire_with_waker(&app_data_dir, arguments.files.clone(), {
      let egui_context = egui_context.clone();
      move || egui_context.request_repaint()
    })? {
      InstanceRole::Primary(instance) => instance,
      InstanceRole::Secondary => {
        if arguments.files.is_empty() {
          show_already_running_message();
        }
        return Ok(());
      }
    };

  let initially_visible = arguments.start_visible || !arguments.files.is_empty();
  let native_options = native_options(initially_visible);
  let startup_files = arguments.files;
  let start_visible = arguments.start_visible;

  eframe::run_native_ext(
    APP_NAME,
    native_options,
    Some(egui_context),
    Box::new(move |creation_context| {
      Ok(Box::new(RsBoardApp::new(creation_context, instance, startup_files, start_visible)?))
    }),
  )?;
  Ok(())
}

fn native_egui_context() -> eframe::egui::Context {
  let context = eframe::egui::Context::default();
  context.set_embed_viewports(false);
  context
}

fn native_options(initially_visible: bool) -> eframe::NativeOptions {
  let viewport = eframe::egui::ViewportBuilder::default()
    .with_title(APP_NAME)
    .with_app_id(APP_ID)
    .with_inner_size([1120.0, 760.0])
    // Glow reuses the root OpenGL pixel format for child viewports. The capture overlay
    // needs an alpha channel even though the library paints an opaque background.
    .with_transparent(true)
    .with_visible(true)
    .with_active(initially_visible);
  let viewport = if initially_visible {
    viewport.with_decorations(true).with_resizable(true).with_mouse_passthrough(false)
  } else {
    // Immediate viewports are only created while their parent is renderable. Keep an invisible
    // host alive in tray mode; RsBoardApp shrinks it to one transparent point after startup.
    viewport.with_decorations(false).with_resizable(false).with_mouse_passthrough(true)
  };
  let mut options =
    eframe::NativeOptions { viewport, centered: true, persist_window: false, ..Default::default() };

  #[cfg(target_os = "macos")]
  {
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

    options.event_loop_builder = Some(Box::new(|builder| {
      builder
        .with_activation_policy(ActivationPolicy::Accessory)
        .with_default_menu(false)
        .with_activate_ignoring_other_apps(false);
    }));
  }

  options
}

fn show_already_running_message() {
  let _ = MessageDialog::new()
    .set_level(MessageLevel::Info)
    .set_title(APP_NAME)
    .set_description("RS Board 已在运行，可通过菜单栏图标打开。")
    .set_buttons(MessageButtons::Ok)
    .show();
}

fn report_startup_error(error: &StartupError) {
  let description = format!("RS Board 启动失败：\n{error}");
  eprintln!("{description}");
  let _ = MessageDialog::new()
    .set_level(MessageLevel::Error)
    .set_title("RS Board 启动失败")
    .set_description(description)
    .set_buttons(MessageButtons::Ok)
    .show();
}

#[derive(Debug)]
struct StartupArguments {
  files: Vec<PathBuf>,
  start_visible: bool,
}

impl StartupArguments {
  fn from_environment() -> Self {
    let mut files = Vec::new();
    let mut start_visible = false;
    for argument in std::env::args_os().skip(1) {
      if argument == OsStr::new(SHOW_WINDOW_ARGUMENT) {
        start_visible = true;
        continue;
      }
      let path = PathBuf::from(argument);
      if is_rsboard_file(&path) {
        files.push(path);
      }
    }
    Self { files, start_visible }
  }
}

fn is_rsboard_file(path: &Path) -> bool {
  path.extension().is_some_and(|extension| extension == OsStr::new("rsboard"))
}

#[derive(Debug, Error)]
enum StartupError {
  #[error(transparent)]
  Settings(#[from] SettingsError),
  #[error(transparent)]
  Instance(#[from] InstanceError),
  #[error(transparent)]
  PerformanceLog(#[from] PerformanceLogError),
  #[error("无法创建原生窗口：{0}")]
  Native(#[from] eframe::Error),
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn recognizes_rsboard_extension_only() {
    assert!(is_rsboard_file(Path::new("lesson.rsboard")));
    assert!(!is_rsboard_file(Path::new("lesson.png")));
    assert!(!is_rsboard_file(Path::new("lesson.RSBOARD")));
  }

  #[test]
  fn bundle_metadata_declares_rsboard_document_type() {
    let cargo_toml = include_str!("../Cargo.toml");
    let plist_extension = include_str!("../assets/macos-info-plist-ext.xml");
    assert!(cargo_toml.contains("osx_info_plist_exts"));
    assert!(plist_extension.contains("CFBundleDocumentTypes"));
    assert!(plist_extension.contains("com.linjiajian.rs-board.document"));
    assert!(plist_extension.contains("<string>rsboard</string>"));
  }

  #[test]
  fn root_viewport_negotiates_transparency_for_capture_overlays() {
    assert_eq!(native_options(true).viewport.transparent, Some(true));
  }

  #[test]
  fn native_context_creates_capture_overlays_as_native_viewports() {
    assert!(!native_egui_context().embed_viewports());
  }

  #[test]
  fn tray_mode_keeps_a_transparent_mouse_passthrough_viewport_renderable() {
    let viewport = native_options(false).viewport;
    assert_eq!(viewport.visible, Some(true));
    assert_eq!(viewport.transparent, Some(true));
    assert_eq!(viewport.decorations, Some(false));
    assert_eq!(viewport.mouse_passthrough, Some(true));
  }
}
