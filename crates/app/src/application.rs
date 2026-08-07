use std::{
  collections::HashMap,
  path::PathBuf,
  sync::{
    Arc,
    mpsc::{self, Receiver, Sender},
  },
  thread,
  time::{Duration, Instant},
};

use common::{
  BoardDocument, CapturedDisplay, CommandHistory, DirtyBaseline, DocumentId, Element,
  GlobalBoundsPx, SizePx,
};
use eframe::egui::{self, Color32, TextureHandle, TextureOptions, ViewportCommand, WindowLevel};
use image::RgbaImage;
use thiserror::Error;
use uuid::Uuid;

use crate::{
  background_encode::{BackgroundEncodeScheduler, BackgroundPrepareError, PreparedBackground},
  capture::{
    CaptureFrame, CaptureOptions, NativeCaptureImage, capture_prepared_display,
    invalidate_capture_backend_cache, prepare_display_capture_under_cursor_at,
    prewarm_capture_backend, request_screen_recording_permission,
  },
  capture_surface::{CaptureSurfaceCoordinator, DisplayRefreshOutcome, SurfaceLifecycle},
  draft_coordinator::{DraftCoordinator, DraftCoordinatorError, DraftResult, StashJob},
  editor::{EditorAction, EditorController, EditorTool},
  export::{copy_image, write_png_atomically},
  instance::InstanceBridge,
  performance::{
    PerformanceContext, PerformanceDetails, PerformanceOutcome, PerformanceTimer, record,
  },
  platform::{GlobalF1Hotkey, OpenFileBridge, global_cursor_position, set_launch_at_login},
  post_save::{PostSaveCoordinator, PostSaveCoordinatorError, PostSaveJob, PostSaveResult},
  recent::RecentDocuments,
  renderer::render_document_to_image,
  settings::{Settings, SettingsError},
  storage::{
    GenerationId, ImportRequest, ImportedDocument, LoadedDocument, LoadedDraft, LocalStore,
    PersistenceContext, SaveRequest, SavedDocument, StorageError, StorePaths,
  },
  tray::{TrayAction, TrayController},
};

const LIBRARY_PANEL_MARGIN: f32 = 16.0;
const LIBRARY_GRID_GAP: f32 = 14.0;
const LIBRARY_SCROLLBAR_RESERVE: f32 = 14.0;
const LIBRARY_CARD_WIDTH: f32 = 350.0;
const LIBRARY_CARD_INNER_MARGIN: f32 = 8.0;
const LIBRARY_CARD_CONTENT_GAP: f32 = 7.0;
const LIBRARY_CARD_ACTION_WIDTH: f32 = 40.0;
const LIBRARY_PREVIEW_SIZE: egui::Vec2 = egui::vec2(144.0, 81.0);
const LIBRARY_SIZE: egui::Vec2 = egui::vec2(
  2.0 * LIBRARY_CARD_WIDTH
    + LIBRARY_GRID_GAP
    + 2.0 * LIBRARY_PANEL_MARGIN
    + LIBRARY_SCROLLBAR_RESERVE,
  760.0,
);
const BUNDLED_CJK_FONT: &[u8] = include_bytes!("../assets/NotoSansSC-Regular.otf");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOrigin {
  NewCapture,
  ExistingDocument,
  LatestDraft { generation_id: GenerationId },
}

struct WorkingSession {
  session_id: Uuid,
  capture_sequence: Option<u64>,
  origin: SessionOrigin,
  document: BoardDocument,
  history: CommandHistory,
  dirty_baseline: DirtyBaseline,
  element_clipboard: Option<Element>,
  prepared_background: PreparedBackground,
  native_background: Option<NativeCaptureImage>,
  background_texture: Option<TextureHandle>,
  editor: EditorController,
}

struct SessionBackground {
  prepared: PreparedBackground,
  pixels: Option<Arc<[u8]>>,
  native: Option<NativeCaptureImage>,
}

impl WorkingSession {
  fn is_dirty(&self) -> bool {
    self.document.is_dirty_against(self.dirty_baseline)
  }

  fn persistence_context(
    &self,
    request_id: Uuid,
    stash_sequence: Option<u64>,
  ) -> PersistenceContext {
    let context = PersistenceContext::new(request_id, self.session_id)
      .with_sequences(self.capture_sequence, stash_sequence);
    match self.origin {
      SessionOrigin::LatestDraft { generation_id } => context.with_generation(generation_id),
      SessionOrigin::NewCapture | SessionOrigin::ExistingDocument => context,
    }
  }

  fn performance_context(
    &self,
    request_id: Uuid,
    stash_sequence: Option<u64>,
  ) -> PerformanceContext {
    PerformanceContext {
      request_id: Some(request_id),
      session_id: Some(self.session_id),
      capture_sequence: self.capture_sequence,
      stash_sequence,
      generation_id: match self.origin {
        SessionOrigin::LatestDraft { generation_id } => Some(generation_id.as_uuid()),
        SessionOrigin::NewCapture | SessionOrigin::ExistingDocument => None,
      },
      document_id: Some(self.document.document_id.as_uuid()),
      revision: Some(self.document.revision),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
  Idle,
  Capturing { request_id: Uuid, capture_sequence: u64, display_id: u32 },
  Editing,
  Saving { request_id: Uuid },
  Opening { request_id: Uuid, document_id: DocumentId },
  Restoring { request_id: Uuid },
  ConfirmingDiscard,
}

impl Phase {
  fn has_active_session(self) -> bool {
    matches!(self, Self::Editing | Self::Saving { .. } | Self::ConfirmingDiscard)
  }
}

enum WorkerEvent {
  Capture {
    request_id: Uuid,
    capture_sequence: u64,
    completed_at: Instant,
    result: Result<CaptureFrame, String>,
  },
  Open {
    request_id: Uuid,
    document_id: DocumentId,
    result: Result<LoadedDocument, String>,
  },
  Restore {
    request_id: Uuid,
    result: Result<Option<LoadedDraft>, String>,
  },
  Save {
    request_id: Uuid,
    context: PersistenceContext,
    completed_at: Instant,
    result: Result<SavedDocument, String>,
    post_save_job: Option<PostSaveJob>,
  },
  Import(Result<ImportedDocument, String>),
  LibraryChanged(Result<Option<DocumentId>, String>),
  Auxiliary(Result<String, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryAction {
  Open(DocumentId),
  Rename(DocumentId),
  CopyImage(DocumentId),
  ExportPng(DocumentId),
  ExportBundle(DocumentId),
  Delete(DocumentId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitChoice {
  Save,
  Discard,
  Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryKind {
  Save,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowSurface {
  Hidden,
  Library,
}

#[derive(Clone, Copy)]
struct CaptureTrigger {
  source: &'static str,
  received_at: Instant,
}

impl CaptureTrigger {
  fn now(source: &'static str) -> Self {
    Self { source, received_at: Instant::now() }
  }
}

#[derive(Clone, Copy)]
struct CapturePresentationTrace {
  performance: PerformanceContext,
  started_at: Instant,
  trigger: &'static str,
  pixel_size: Option<[u32; 2]>,
}

#[derive(Clone, Copy)]
struct PersistenceUiTrace {
  performance: PerformanceContext,
  started_at: Instant,
  workflow: &'static str,
  pixel_size: [u32; 2],
}

pub struct RsBoardApp {
  store: LocalStore,
  settings_path: PathBuf,
  settings: Settings,
  settings_draft: Settings,
  instance: InstanceBridge,
  open_file_bridge: Option<OpenFileBridge>,
  hotkey: Option<GlobalF1Hotkey>,
  tray: Option<TrayController>,
  surface: WindowSurface,
  capture_surfaces: CaptureSurfaceCoordinator,
  background_encoder: BackgroundEncodeScheduler,
  draft_coordinator: DraftCoordinator,
  post_save_coordinator: PostSaveCoordinator,
  phase: Phase,
  next_capture_sequence: u64,
  next_stash_sequence: u64,
  capture_presentation_trace: Option<CapturePresentationTrace>,
  persistence_ui_trace: Option<PersistenceUiTrace>,
  session: Option<WorkingSession>,
  recent: RecentDocuments,
  draft_available: bool,
  last_tool: Option<EditorTool>,
  worker_sender: Sender<WorkerEvent>,
  worker_receiver: Receiver<WorkerEvent>,
  preview_textures: HashMap<DocumentId, (PathBuf, TextureHandle)>,
  show_settings: bool,
  rename_dialog: Option<(DocumentId, String)>,
  delete_document_dialog: Option<DocumentId>,
  delete_draft_dialog: bool,
  clear_confirmation_stage: u8,
  exit_dialog: bool,
  quit_after_persist: bool,
  allow_close: bool,
  persistent_error: Option<String>,
  retry_kind: Option<RetryKind>,
  library_error: Option<String>,
  toast: Option<(String, Instant)>,
}

impl RsBoardApp {
  pub fn new(
    creation_context: &eframe::CreationContext<'_>,
    instance: InstanceBridge,
    startup_files: Vec<PathBuf>,
    start_visible: bool,
  ) -> Result<Self, ApplicationError> {
    configure_egui(&creation_context.egui_ctx);
    let settings_path = Settings::default_path()?;
    let settings = Settings::load_or_default(&settings_path)?;
    let paths = StorePaths::for_current_user()?;
    let (store, _) = LocalStore::open(paths)?;
    let mut recent = RecentDocuments::default();
    recent.refresh(&store)?;
    let draft_available = store.paths().latest_draft().exists();
    let startup_permission_error = request_screen_recording_permission().err();
    if startup_permission_error.is_none() {
      prewarm_capture_backend();
    }
    let (worker_sender, worker_receiver) = mpsc::channel();
    let draft_coordinator = DraftCoordinator::new(store.clone(), {
      let context = creation_context.egui_ctx.clone();
      move || context.request_repaint()
    })?;
    let post_save_coordinator = PostSaveCoordinator::new(store.clone(), {
      let context = creation_context.egui_ctx.clone();
      move || context.request_repaint()
    })?;
    let open_file_bridge = OpenFileBridge::install();
    let mut library_error = None;
    let capture_surfaces = match CaptureSurfaceCoordinator::discover() {
      Ok(coordinator) => coordinator,
      Err(error) => {
        library_error = Some(format!("截图窗口预热失败：{error}"));
        CaptureSurfaceCoordinator::default()
      }
    };
    let hotkey = match GlobalF1Hotkey::from_shortcut_with_waker(&settings.global_hotkey, {
      let context = creation_context.egui_ctx.clone();
      move || context.request_repaint()
    }) {
      Ok(hotkey) => Some(hotkey),
      Err(error) => {
        library_error = Some(format!("全局快捷键注册失败：{error}"));
        None
      }
    };
    let tray = TrayController::with_waker({
      let context = creation_context.egui_ctx.clone();
      move || context.request_repaint()
    })
    .ok();
    if let Some(tray) = &tray {
      tray.set_availability(false, draft_available);
    }
    let starts_with_library = start_visible || !startup_files.is_empty() || library_error.is_some();
    if starts_with_library && let Some(error) = startup_permission_error {
      let permission_message = format!("屏幕录制权限未授予；截图时会再次请求：{error}");
      library_error = Some(match library_error.take() {
        Some(existing) => format!("{existing}\n{permission_message}"),
        None => permission_message,
      });
    }

    let mut app = Self {
      store,
      settings_path,
      settings_draft: settings.clone(),
      settings,
      instance,
      open_file_bridge,
      hotkey,
      tray,
      surface: WindowSurface::Hidden,
      capture_surfaces,
      background_encoder: BackgroundEncodeScheduler::new(),
      draft_coordinator,
      post_save_coordinator,
      phase: Phase::Idle,
      next_capture_sequence: 0,
      next_stash_sequence: 0,
      capture_presentation_trace: None,
      persistence_ui_trace: None,
      session: None,
      recent,
      draft_available,
      last_tool: None,
      worker_sender,
      worker_receiver,
      preview_textures: HashMap::new(),
      show_settings: false,
      rename_dialog: None,
      delete_document_dialog: None,
      delete_draft_dialog: false,
      clear_confirmation_stage: 0,
      exit_dialog: false,
      quit_after_persist: false,
      allow_close: false,
      persistent_error: None,
      retry_kind: None,
      library_error,
      toast: None,
    };
    if starts_with_library {
      app.show_library_window(&creation_context.egui_ctx);
    } else {
      creation_context.egui_ctx.send_viewport_cmd(ViewportCommand::Visible(false));
    }
    for path in startup_files {
      app.start_import(path, &creation_context.egui_ctx);
    }
    Ok(app)
  }

  fn spawn_worker(
    &self,
    context: &egui::Context,
    work: impl FnOnce() -> WorkerEvent + Send + 'static,
  ) -> Result<(), std::io::Error> {
    self.spawn_worker_traced(context, PerformanceContext::default(), "auxiliary", work)
  }

  fn spawn_worker_traced(
    &self,
    context: &egui::Context,
    performance: PerformanceContext,
    workflow: &'static str,
    work: impl FnOnce() -> WorkerEvent + Send + 'static,
  ) -> Result<(), std::io::Error> {
    let sender = self.worker_sender.clone();
    let context = context.clone();
    let timer = PerformanceTimer::start(
      "worker.spawn",
      performance,
      PerformanceDetails::default().workflow(workflow),
    );
    let result = thread::Builder::new().name("rs-board-worker".into()).spawn(move || {
      let lifecycle_timer = PerformanceTimer::start(
        "worker.lifecycle",
        performance,
        PerformanceDetails::default().workflow(workflow),
      );
      let event = work();
      lifecycle_timer.finish_ok();
      let _ = sender.send(event);
      context.request_repaint();
    });
    match result {
      Ok(_) => {
        timer.finish_ok();
        Ok(())
      }
      Err(error) => {
        timer.finish_error(&error);
        Err(error)
      }
    }
  }

  fn start_capture(&mut self, context: &egui::Context, trigger: CaptureTrigger) {
    if self.phase != Phase::Idle {
      PerformanceTimer::started_at(
        "capture.request.dispatch",
        PerformanceContext::default(),
        PerformanceDetails::default().trigger(trigger.source),
        trigger.received_at,
      )
      .finish_rejected();
      self.set_toast("正在处理当前任务");
      return;
    }
    let request_id = Uuid::new_v4();
    let capture_sequence = next_sequence(&mut self.next_capture_sequence);
    self.capture_surfaces.remember_frontmost_application();
    self.capture_surfaces.exclude_application_windows();
    let performance = PerformanceContext::capture(request_id, capture_sequence);
    PerformanceTimer::started_at(
      "capture.request.dispatch",
      performance,
      PerformanceDetails::default().trigger(trigger.source),
      trigger.received_at,
    )
    .finish_ok();
    let capture_options = CaptureOptions { include_cursor: self.settings.include_cursor };
    let cursor_timer = PerformanceTimer::start(
      "capture.cursor.query",
      performance,
      PerformanceDetails::default().trigger(trigger.source),
    );
    let cursor_position = global_cursor_position();
    cursor_timer.finish_ok();
    let prepare_timer = PerformanceTimer::start(
      "capture.display.prepare",
      performance,
      PerformanceDetails::default().trigger(trigger.source),
    );
    let prepared_capture =
      match prepare_display_capture_under_cursor_at(capture_options, cursor_position) {
        Ok(prepared_capture) => {
          prepare_timer.finish_ok();
          prepared_capture
        }
        Err(error) => {
          prepare_timer.finish_error(&error);
          PerformanceTimer::started_at(
            "capture.request.total",
            performance,
            PerformanceDetails::default().trigger(trigger.source),
            trigger.received_at,
          )
          .finish_error(&error);
          self.phase = Phase::Idle;
          self.capture_surfaces.hide_active();
          self.library_error = Some(error.to_string());
          self.show_library_window(context);
          return;
        }
      };
    let display_id = prepared_capture.display().display_id;
    self.phase = Phase::Capturing { request_id, capture_sequence, display_id };
    self.capture_presentation_trace = Some(CapturePresentationTrace {
      performance,
      started_at: trigger.received_at,
      trigger: trigger.source,
      pixel_size: None,
    });
    self.library_error = None;
    self.update_tray();
    let spawn_result = self.spawn_worker_traced(context, performance, "capture", move || {
      let timer = PerformanceTimer::start(
        "capture.worker",
        performance,
        PerformanceDetails::default().trigger(trigger.source),
      );
      let result = capture_prepared_display(request_id, capture_sequence, prepared_capture);
      match &result {
        Ok(_) => timer.finish_ok(),
        Err(error) => timer.finish_error(error),
      }
      WorkerEvent::Capture {
        request_id,
        capture_sequence,
        completed_at: Instant::now(),
        result: result.map_err(|error| error.to_string()),
      }
    });
    if let Err(error) = spawn_result {
      self.capture_presentation_trace = None;
      PerformanceTimer::started_at(
        "capture.request.total",
        performance,
        PerformanceDetails::default().trigger(trigger.source),
        trigger.received_at,
      )
      .finish_error(&error);
      self.phase = Phase::Idle;
      self.capture_surfaces.hide_active();
      self.library_error = Some(format!("无法启动截图任务：{error}"));
      self.show_library_window(context);
      self.update_tray();
    }
  }

  fn start_open_document(&mut self, document_id: DocumentId, context: &egui::Context) {
    if self.phase != Phase::Idle {
      return;
    }
    let request_id = Uuid::new_v4();
    self.phase = Phase::Opening { request_id, document_id };
    let store = self.store.clone();
    let spawn_result = self.spawn_worker(context, move || WorkerEvent::Open {
      request_id,
      document_id,
      result: store.open_document(document_id).map_err(|error| error.to_string()),
    });
    if let Err(error) = spawn_result {
      self.phase = Phase::Idle;
      self.library_error = Some(format!("无法启动讲义打开任务：{error}"));
    }
    self.update_tray();
  }

  fn start_restore(&mut self, context: &egui::Context) {
    if self.phase != Phase::Idle || !self.draft_available {
      return;
    }
    let request_id = Uuid::new_v4();
    self.phase = Phase::Restoring { request_id };
    let store = self.store.clone();
    let spawn_result = self.spawn_worker(context, move || WorkerEvent::Restore {
      request_id,
      result: store.load_latest_draft().map_err(|error| error.to_string()),
    });
    if let Err(error) = spawn_result {
      self.phase = Phase::Idle;
      self.library_error = Some(format!("无法启动草稿恢复任务：{error}"));
    }
    self.update_tray();
  }

  fn start_save(&mut self, context: &egui::Context) {
    if self.phase != Phase::Editing {
      return;
    }
    let started_at = Instant::now();
    let Some(session) = self.session.as_mut() else {
      return;
    };
    let request_id = Uuid::new_v4();
    let performance = session.performance_context(request_id, None);
    let pixel_size =
      [session.document.canvas_size_px.width_px, session.document.canvas_size_px.height_px];
    let snapshot_timer = PerformanceTimer::start(
      "persistence.snapshot",
      performance,
      PerformanceDetails::default().workflow("save"),
    );
    let snapshot = match session.document.snapshot(session.document.revision) {
      Ok(snapshot) => {
        snapshot_timer.finish_ok();
        snapshot
      }
      Err(error) => {
        snapshot_timer.finish_error(&error);
        PerformanceTimer::started_at(
          "persistence.request_to_ui_complete",
          performance,
          PerformanceDetails::default().workflow("save").pixel_size(pixel_size),
          started_at,
        )
        .finish_error(&error);
        self.persistent_error = Some(error.to_string());
        return;
      }
    };
    if session.prepared_background.needs_retry() {
      let Some(image) = session.native_background.clone() else {
        self.persistent_error = Some("背景编码失败且没有可用于重试的内存图像".into());
        return;
      };
      session.prepared_background = self.background_encoder.submit(
        session.prepared_background.capture_sequence(),
        image,
        performance,
      );
    }
    let snapshot = Arc::new(snapshot);
    let prepared_background = session.prepared_background.clone();
    let persistence_context = session.persistence_context(request_id, None);
    let copy_to_clipboard = self.settings.copy_image_after_save;
    self.phase = Phase::Saving { request_id };
    self.persistence_ui_trace =
      Some(PersistenceUiTrace { performance, started_at, workflow: "save", pixel_size });
    self.persistent_error = None;
    self.retry_kind = None;
    let store = self.store.clone();
    let spawn_result = self.spawn_worker_traced(context, performance, "save", move || {
      let timer = PerformanceTimer::start(
        "persistence.worker",
        performance,
        PerformanceDetails::default().workflow("save").pixel_size(pixel_size),
      );
      let result = match prepared_background.wait() {
        Ok(background) => store
          .save_document(SaveRequest {
            context: persistence_context,
            snapshot: snapshot.as_ref().clone(),
            background,
          })
          .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
      };
      match &result {
        Ok(_) => timer.finish_ok(),
        Err(_) => timer.finish_error_code("persistence.save_failed"),
      }
      let completed_at = Instant::now();
      let post_save_job = result.as_ref().ok().map(|saved| PostSaveJob {
        document_id: saved.document_id,
        revision: saved.revision,
        snapshot,
        prepared_background,
        copy_to_clipboard,
        performance,
        requested_at: completed_at,
      });
      WorkerEvent::Save {
        request_id,
        context: persistence_context,
        completed_at,
        result,
        post_save_job,
      }
    });
    if let Err(error) = spawn_result {
      self.finish_persistence_trace_error(request_id);
      self.phase = Phase::Editing;
      self.persistent_error = Some(format!("无法启动保存任务：{error}"));
      self.retry_kind = Some(RetryKind::Save);
      self.quit_after_persist = false;
    }
    self.update_tray();
  }

  fn start_stash(&mut self, context: &egui::Context) {
    if self.phase != Phase::Editing {
      return;
    }
    let started_at = Instant::now();
    let stash_sequence = next_sequence(&mut self.next_stash_sequence);
    let Some(session) = self.session.as_ref() else {
      return;
    };
    if matches!(session.origin, SessionOrigin::ExistingDocument) {
      return;
    }
    let request_id = Uuid::new_v4();
    let generation_id = GenerationId::new();
    let performance = PerformanceContext {
      generation_id: Some(generation_id.as_uuid()),
      ..session.performance_context(request_id, Some(stash_sequence))
    };
    let pixel_size =
      [session.document.canvas_size_px.width_px, session.document.canvas_size_px.height_px];
    let snapshot_timer = PerformanceTimer::start(
      "persistence.snapshot",
      performance,
      PerformanceDetails::default().workflow("stash"),
    );
    let snapshot = match session.document.snapshot(session.document.revision) {
      Ok(snapshot) => {
        snapshot_timer.finish_ok();
        snapshot
      }
      Err(error) => {
        snapshot_timer.finish_error(&error);
        PerformanceTimer::started_at(
          "persistence.request_to_ui_complete",
          performance,
          PerformanceDetails::default().workflow("stash").pixel_size(pixel_size),
          started_at,
        )
        .finish_error(&error);
        self.persistent_error = Some(error.to_string());
        return;
      }
    };
    let persistence_context =
      session.persistence_context(request_id, Some(stash_sequence)).with_generation(generation_id);
    let job = StashJob {
      context: persistence_context,
      generation_id,
      snapshot,
      prepared_background: session.prepared_background.clone(),
      requested_at: started_at,
    };
    if let Err(error) = self.draft_coordinator.enqueue_commit(job) {
      PerformanceTimer::started_at(
        "persistence.request_to_ui_complete",
        performance,
        PerformanceDetails::default().workflow("stash").pixel_size(pixel_size),
        started_at,
      )
      .finish_error(&error);
      self.persistent_error = Some(error.to_string());
      return;
    }

    self.phase = Phase::Idle;
    self.persistent_error = None;
    self.retry_kind = None;
    self.hide_editor_window(context);
    self.remember_tool_and_release_session();
    PerformanceTimer::started_at(
      "persistence.request_to_ui_complete",
      performance,
      PerformanceDetails::default().workflow("stash").pixel_size(pixel_size),
      started_at,
    )
    .finish_ok();
    self.update_tray();
  }

  fn start_import(&mut self, path: PathBuf, context: &egui::Context) {
    if self.phase != Phase::Idle {
      self.set_toast("当前会话结束后才能导入讲义");
      return;
    }
    if path.extension().and_then(|extension| extension.to_str()) != Some("rsboard") {
      self.library_error = Some("只能导入 .rsboard 讲义".into());
      return;
    }
    let request = ImportRequest {
      context: PersistenceContext::new(Uuid::new_v4(), Uuid::new_v4()),
      manifest_path: path,
    };
    let store = self.store.clone();
    if let Err(error) = self.spawn_worker(context, move || {
      WorkerEvent::Import(store.import_document(request).map_err(|error| error.to_string()))
    }) {
      self.library_error = Some(format!("无法启动讲义导入任务：{error}"));
    }
  }

  fn session_from_capture(
    &self,
    frame: CaptureFrame,
    _context: &egui::Context,
  ) -> Result<WorkingSession, ApplicationError> {
    let performance = PerformanceContext::capture(frame.request_id, frame.capture_sequence);
    let [width_px, height_px] = frame.pixel_size;
    let canvas_size_px = SizePx::new(width_px, height_px);
    let bounds = frame.display_bounds_global;
    let document = BoardDocument::new_capture(
      DocumentId::new(),
      canvas_size_px,
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx {
          x_px: bounds[0],
          y_px: bounds[1],
          width_px: bounds[2].try_into().unwrap_or_default(),
          height_px: bounds[3].try_into().unwrap_or_default(),
        },
        scale_factor: frame.scale_factor,
      },
      frame.captured_at.with_timezone(&chrono::Local),
    )?;
    let prepared =
      self.background_encoder.submit(frame.capture_sequence, frame.image.clone(), performance);
    self.make_session(
      SessionOrigin::NewCapture,
      Some(frame.capture_sequence),
      document,
      SessionBackground { prepared, pixels: None, native: Some(frame.image) },
      performance,
      _context,
    )
  }

  fn session_from_loaded(
    &self,
    origin: SessionOrigin,
    capture_sequence: Option<u64>,
    loaded: LoadedDocument,
    performance: PerformanceContext,
    context: &egui::Context,
  ) -> Result<WorkingSession, ApplicationError> {
    let prepared =
      PreparedBackground::ready(capture_sequence.unwrap_or_default(), loaded.background.clone())?;
    #[cfg(target_os = "macos")]
    let (pixels, native) = {
      let encoded =
        loaded.background.encoded_png_bytes().ok_or(ApplicationError::InvalidTexture)?;
      (None, Some(NativeCaptureImage::from_encoded_png(&encoded)?))
    };
    #[cfg(not(target_os = "macos"))]
    let (pixels, native) = {
      let (_, _, pixels) = loaded.background.decode_rgba8()?;
      (Some(pixels), None)
    };
    self.make_session(
      origin,
      capture_sequence,
      loaded.document,
      SessionBackground { prepared, pixels, native },
      performance,
      context,
    )
  }

  fn make_session(
    &self,
    origin: SessionOrigin,
    capture_sequence: Option<u64>,
    document: BoardDocument,
    background: SessionBackground,
    performance: PerformanceContext,
    context: &egui::Context,
  ) -> Result<WorkingSession, ApplicationError> {
    document.validate()?;
    let texture = if let Some(pixels) = background.pixels.as_deref() {
      let texture_timer = PerformanceTimer::start(
        "capture.background_texture.register",
        PerformanceContext {
          document_id: Some(document.document_id.as_uuid()),
          revision: Some(document.revision),
          ..performance
        },
        PerformanceDetails::default()
          .pixel_size([document.canvas_size_px.width_px, document.canvas_size_px.height_px]),
      );
      let texture_result = load_rgba_texture(
        context,
        &format!("background-{}", document.document_id),
        document.canvas_size_px,
        pixels,
      );
      match &texture_result {
        Ok(_) => texture_timer.finish_ok(),
        Err(error) => texture_timer.finish_error(error),
      }
      Some(texture_result?)
    } else {
      None
    };
    let dirty_baseline = document.dirty_baseline();
    Ok(WorkingSession {
      session_id: Uuid::new_v4(),
      capture_sequence,
      origin,
      document,
      history: CommandHistory::new(),
      dirty_baseline,
      element_clipboard: None,
      prepared_background: background.prepared,
      native_background: background.native,
      background_texture: texture,
      editor: EditorController::new(self.last_tool),
    })
  }

  fn update_tray(&self) {
    if let Some(tray) = &self.tray {
      tray.set_availability(self.phase != Phase::Idle, self.draft_available);
    }
  }

  fn set_toast(&mut self, message: impl Into<String>) {
    self.toast = Some((message.into(), Instant::now()));
  }

  fn persistence_trace_for(&self, request_id: Uuid) -> Option<PersistenceUiTrace> {
    self.persistence_ui_trace.filter(|trace| trace.performance.request_id == Some(request_id))
  }

  fn finish_persistence_trace_ok(&mut self, request_id: Uuid) {
    let Some(trace) = self.persistence_trace_for(request_id) else {
      return;
    };
    self.persistence_ui_trace = None;
    PerformanceTimer::started_at(
      "persistence.request_to_ui_complete",
      trace.performance,
      PerformanceDetails::default().workflow(trace.workflow).pixel_size(trace.pixel_size),
      trace.started_at,
    )
    .finish_ok();
  }

  fn finish_persistence_trace_error(&mut self, request_id: Uuid) {
    let Some(trace) = self.persistence_trace_for(request_id) else {
      return;
    };
    self.persistence_ui_trace = None;
    PerformanceTimer::started_at(
      "persistence.request_to_ui_complete",
      trace.performance,
      PerformanceDetails::default().workflow(trace.workflow).pixel_size(trace.pixel_size),
      trace.started_at,
    )
    .finish_error_code("persistence.ui_failed");
  }

  fn finish_persistence_trace_stale(&mut self, request_id: Uuid) {
    let Some(trace) = self.persistence_trace_for(request_id) else {
      return;
    };
    self.persistence_ui_trace = None;
    PerformanceTimer::started_at(
      "persistence.request_to_ui_complete",
      trace.performance,
      PerformanceDetails::default().workflow(trace.workflow).pixel_size(trace.pixel_size),
      trace.started_at,
    )
    .finish_stale();
  }

  fn handle_draft_results(&mut self, _context: &egui::Context) {
    while let Some(event) = self.draft_coordinator.try_recv() {
      match event {
        DraftResult::Commit {
          context: expected,
          generation_id,
          completed_at,
          is_latest,
          result,
        } => {
          let performance = PerformanceContext {
            generation_id: Some(generation_id.as_uuid()),
            ..performance_from_persistence_context(expected)
          };
          PerformanceTimer::started_at(
            "persistence.worker_to_ui",
            performance,
            PerformanceDetails::default().workflow("stash"),
            completed_at,
          )
          .finish_ok();
          match *result {
            Ok(stored) if stored.context == expected && stored.generation_id == generation_id => {
              self.draft_available = true;
              record(
                "persistence.result",
                performance,
                PerformanceDetails::default().workflow("stash"),
                if is_latest { PerformanceOutcome::Ok } else { PerformanceOutcome::Stale },
              );
            }
            Ok(_) => {
              self.draft_available = true;
              self.report_background_failure(
                performance,
                is_latest,
                "draft_result_mismatch",
                "最新草稿暂存失败",
              );
            }
            Err(error) => {
              eprintln!(
                "draft_commit_failed capture_sequence={:?} stash_sequence={:?} generation_id={} latest={} error={error}",
                expected.capture_sequence, expected.stash_sequence, generation_id, is_latest,
              );
              self.report_background_failure(
                performance,
                is_latest,
                "draft_commit_failed",
                "最新草稿暂存失败",
              );
            }
          }
        }
        DraftResult::DeleteIfGeneration { generation_id, result } => match result {
          Ok(_) => {
            self.draft_available = self.store.paths().latest_draft().exists();
          }
          Err(error) => {
            eprintln!("draft_delete_generation_failed generation_id={generation_id} error={error}");
            self.report_background_failure(
              PerformanceContext {
                generation_id: Some(generation_id.as_uuid()),
                ..Default::default()
              },
              true,
              "draft_delete_failed",
              "草稿清理失败",
            );
          }
        },
        DraftResult::DeleteLatest { result } => match result {
          Ok(_) => {
            self.draft_available = self.store.paths().latest_draft().exists();
          }
          Err(error) => {
            eprintln!("draft_delete_latest_failed error={error}");
            self.report_background_failure(
              PerformanceContext::default(),
              true,
              "draft_delete_failed",
              "草稿删除失败",
            );
          }
        },
        DraftResult::ClearAll { result } => match result {
          Ok(()) => {
            self.draft_available = false;
            let _ = self.recent.refresh(&self.store);
            self.preview_textures.clear();
          }
          Err(error) => {
            eprintln!("clear_all_content_failed error={error}");
            self.report_background_failure(
              PerformanceContext::default(),
              true,
              "clear_all_failed",
              "内容清理失败",
            );
          }
        },
      }
    }
  }

  fn handle_post_save_results(&mut self) {
    while let Some(event) = self.post_save_coordinator.try_recv() {
      match event {
        PostSaveResult::ImageTasks { document_id, revision, preview_installed, warnings } => {
          if preview_installed {
            self.preview_textures.remove(&document_id);
          }
          if warnings.is_empty() {
            if self.has_visible_window() {
              self.set_toast("讲义已保存");
            }
          } else {
            let warning = warnings.join("；");
            eprintln!(
              "post_save_image_tasks_failed document_id={document_id} revision={revision} error={warning}"
            );
            if self.has_visible_window() {
              self.set_toast(warning);
            }
          }
        }
        PostSaveResult::RecentRefresh { performance, result } => match result {
          Ok(scan) => self.recent.apply_scan(scan),
          Err(error) => {
            eprintln!(
              "post_save_recent_refresh_failed document_id={:?} revision={:?} error={error}",
              performance.document_id, performance.revision
            );
            if self.has_visible_window() {
              self.set_toast("最近讲义刷新失败");
            }
          }
        },
      }
    }
  }

  fn report_background_failure(
    &mut self,
    performance: PerformanceContext,
    is_latest: bool,
    error_code: &'static str,
    visible_message: &'static str,
  ) {
    if !is_latest {
      return;
    }
    eprintln!("draft_background_failure code={error_code}");
    record(
      "draft.background_result",
      performance,
      PerformanceDetails::default().workflow("stash"),
      PerformanceOutcome::Error,
    );
    if self.has_visible_window() {
      self.set_toast(visible_message);
    }
  }

  fn has_visible_window(&self) -> bool {
    self.surface == WindowSurface::Library || self.capture_surfaces.active_display_id().is_some()
  }

  fn handle_worker_events(&mut self, context: &egui::Context) {
    let events: Vec<_> = self.worker_receiver.try_iter().collect();
    for event in events {
      match event {
        WorkerEvent::Capture { request_id, capture_sequence, completed_at, result } => {
          let performance = PerformanceContext::capture(request_id, capture_sequence);
          PerformanceTimer::started_at(
            "capture.worker_to_ui",
            performance,
            PerformanceDetails::default(),
            completed_at,
          )
          .finish_ok();
          let expected_display_id = match self.phase {
            Phase::Capturing {
              request_id: current_request,
              capture_sequence: current_sequence,
              display_id,
            } if current_request == request_id && current_sequence == capture_sequence => {
              display_id
            }
            _ => {
              if self
                .capture_presentation_trace
                .is_some_and(|trace| trace.performance == performance)
              {
                let trace = self.capture_presentation_trace.take().expect("trace checked above");
                PerformanceTimer::started_at(
                  "capture.request.total",
                  trace.performance,
                  PerformanceDetails::default().trigger(trace.trigger),
                  trace.started_at,
                )
                .finish_stale();
              }
              record(
                "capture.result",
                performance,
                PerformanceDetails::default(),
                PerformanceOutcome::Stale,
              );
              continue;
            }
          };
          let session_result = match result {
            Err(error) => Err(error),
            Ok(frame) => {
              let session_timer = PerformanceTimer::start(
                "capture.session.prepare",
                performance,
                PerformanceDetails::default().pixel_size(frame.pixel_size),
              );
              let result = if frame.request_id != request_id
                || frame.capture_sequence != capture_sequence
                || frame.display_id != expected_display_id
              {
                Err("capture result correlation did not match the active request".to_owned())
              } else {
                let bounds = frame.display_bounds_global;
                let pixel_size = frame.pixel_size;
                let display_id = frame.display_id;
                self
                  .capture_surfaces
                  .set_frozen_image(display_id, &frame.image)
                  .map_err(|error| error.to_string())
                  .and_then(|_| {
                    self
                      .session_from_capture(frame, context)
                      .map(|session| (session, display_id, bounds, pixel_size))
                      .map_err(|error| error.to_string())
                  })
              };
              match &result {
                Ok(_) => session_timer.finish_ok(),
                Err(_) => session_timer.finish_error_code("capture.session_prepare_failed"),
              }
              result
            }
          };
          match session_result {
            Ok((session, display_id, bounds, pixel_size)) => {
              if let Some(trace) = self.capture_presentation_trace.as_mut()
                && trace.performance == performance
              {
                trace.pixel_size = Some(pixel_size);
              }
              self.draft_coordinator.publish_capture(capture_sequence);
              self.session = Some(session);
              self.phase = Phase::Editing;
              self.show_editor_window(context, Some(display_id), Some(bounds));
            }
            Err(error) => {
              if let Some(trace) = self.capture_presentation_trace.take()
                && trace.performance == performance
              {
                PerformanceTimer::started_at(
                  "capture.request.total",
                  trace.performance,
                  PerformanceDetails::default().trigger(trace.trigger),
                  trace.started_at,
                )
                .finish_error_code("capture.request_failed");
              }
              self.phase = Phase::Idle;
              self.capture_surfaces.hide_active();
              self.library_error = Some(error);
              self.show_library_window(context);
            }
          }
        }
        WorkerEvent::Open { request_id, document_id, result }
          if self.phase == Phase::Opening { request_id, document_id } =>
        {
          match result.and_then(|loaded| {
            self
              .session_from_loaded(
                SessionOrigin::ExistingDocument,
                None,
                loaded,
                PerformanceContext { request_id: Some(request_id), ..Default::default() },
                context,
              )
              .map_err(|error| error.to_string())
          }) {
            Ok(session) => {
              self.session = Some(session);
              self.phase = Phase::Editing;
              self.show_editor_window(context, None, None);
            }
            Err(error) => {
              self.phase = Phase::Idle;
              self.library_error = Some(error);
            }
          }
        }
        WorkerEvent::Restore { request_id, result }
          if self.phase == Phase::Restoring { request_id } =>
        {
          match result {
            Ok(Some(draft)) => {
              let origin = SessionOrigin::LatestDraft { generation_id: draft.generation_id };
              let capture_sequence = next_sequence(&mut self.next_capture_sequence);
              let performance = PerformanceContext::capture(request_id, capture_sequence);
              match self.session_from_loaded(
                origin,
                Some(capture_sequence),
                draft.loaded,
                performance,
                context,
              ) {
                Ok(session) => {
                  self.draft_coordinator.publish_capture(capture_sequence);
                  self.session = Some(session);
                  self.phase = Phase::Editing;
                  self.show_editor_window(context, None, None);
                }
                Err(error) => {
                  self.phase = Phase::Idle;
                  self.library_error = Some(error.to_string());
                }
              }
            }
            Ok(None) => {
              self.phase = Phase::Idle;
              self.draft_available = false;
              self.library_error = Some("没有可恢复的草稿".into());
            }
            Err(error) => {
              self.phase = Phase::Idle;
              self.library_error = Some(error);
            }
          }
        }
        WorkerEvent::Save {
          request_id,
          context: expected_context,
          completed_at,
          result,
          post_save_job,
        } => {
          if self.phase != (Phase::Saving { request_id }) {
            let performance = self
              .persistence_trace_for(request_id)
              .map(|trace| trace.performance)
              .unwrap_or_else(|| performance_from_persistence_context(expected_context));
            record(
              "persistence.result",
              performance,
              PerformanceDetails::default().workflow("save"),
              PerformanceOutcome::Stale,
            );
            self.finish_persistence_trace_stale(request_id);
            continue;
          }
          if let Some(trace) = self.persistence_trace_for(request_id) {
            PerformanceTimer::started_at(
              "persistence.worker_to_ui",
              trace.performance,
              PerformanceDetails::default().workflow(trace.workflow).pixel_size(trace.pixel_size),
              completed_at,
            )
            .finish_ok();
          }
          match result {
            Ok(saved)
              if self.persistence_result_matches(
                saved.context,
                expected_context,
                saved.document_id,
                saved.revision,
              ) =>
            {
              let saved_draft_generation = self.session.as_ref().and_then(|session| {
                if let SessionOrigin::LatestDraft { generation_id } = session.origin {
                  Some(generation_id)
                } else {
                  None
                }
              });
              self.phase = Phase::Idle;
              self.preview_textures.remove(&saved.document_id);
              if self.quit_after_persist {
                self.allow_close = true;
                context.send_viewport_cmd(ViewportCommand::Close);
              } else {
                self.hide_editor_window(context);
              }
              self.remember_tool_and_release_session();
              self.finish_persistence_trace_ok(request_id);

              match post_save_job {
                Some(job) => match self.post_save_coordinator.enqueue(job) {
                  Ok(outcome) => {
                    if outcome.clipboard_dropped {
                      eprintln!(
                        "post_save_clipboard_queue_full document_id={} revision={}",
                        saved.document_id, saved.revision
                      );
                      if self.has_visible_window() {
                        self.set_toast("讲义已保存，但保存后复制队列已满");
                      }
                    }
                    if outcome.render_evicted {
                      eprintln!(
                        "post_save_render_queue_evicted document_id={} revision={}",
                        saved.document_id, saved.revision
                      );
                    }
                  }
                  Err(error) => {
                    eprintln!(
                      "post_save_enqueue_failed document_id={} revision={} error={error}",
                      saved.document_id, saved.revision
                    );
                    if self.has_visible_window() {
                      self.set_toast("讲义已保存，但保存后任务启动失败");
                    }
                  }
                },
                None => eprintln!(
                  "post_save_job_missing document_id={} revision={}",
                  saved.document_id, saved.revision
                ),
              }
              if let Some(generation_id) = saved_draft_generation
                && let Err(error) = self.draft_coordinator.delete_if_generation(generation_id)
              {
                eprintln!(
                  "draft_delete_enqueue_failed generation_id={generation_id} error={error}"
                );
                if self.has_visible_window() {
                  self.set_toast("草稿清理失败");
                }
              }
            }
            Ok(_) => {
              if let Some(trace) = self.persistence_trace_for(request_id) {
                record(
                  "persistence.result",
                  trace.performance,
                  PerformanceDetails::default().workflow(trace.workflow),
                  PerformanceOutcome::Stale,
                );
              }
              self.finish_persistence_trace_error(request_id);
              self.phase = Phase::Editing;
              self.persistent_error = Some("保存结果与当前请求不匹配".to_owned());
              self.retry_kind = Some(RetryKind::Save);
              self.quit_after_persist = false;
            }
            Err(error) => {
              self.finish_persistence_trace_error(request_id);
              self.phase = Phase::Editing;
              self.persistent_error = Some(error);
              self.retry_kind = Some(RetryKind::Save);
              self.quit_after_persist = false;
            }
          }
        }
        WorkerEvent::Import(result) => match result {
          Ok(imported) => {
            let document_id = imported.saved.document_id;
            let _ = self.recent.refresh(&self.store);
            self.recent.highlight(document_id);
            self.preview_textures.remove(&document_id);
            if self.phase == Phase::Idle {
              self.show_library_window(context);
            }
            self.set_toast("讲义已导入");
          }
          Err(error) => self.library_error = Some(error),
        },
        WorkerEvent::LibraryChanged(result) => match result {
          Ok(highlighted) => {
            let _ = self.recent.refresh(&self.store);
            self.recent.highlighted = highlighted;
            self.draft_available = self.store.paths().latest_draft().exists();
            self.preview_textures.retain(|id, _| {
              self.recent.documents.iter().any(|document| document.document_id == *id)
            });
          }
          Err(error) => self.library_error = Some(error),
        },
        WorkerEvent::Auxiliary(result) => match result {
          Ok(message) => {
            let _ = self.recent.refresh(&self.store);
            self.set_toast(message);
          }
          Err(error) => self.set_toast(error),
        },
        _ => {}
      }
    }
    self.update_tray();
  }

  fn persistence_result_matches(
    &self,
    persistence: PersistenceContext,
    expected: PersistenceContext,
    document_id: DocumentId,
    revision: u64,
  ) -> bool {
    self.session.as_ref().is_some_and(|session| {
      persistence == expected
        && persistence.session_id == session.session_id
        && persistence.capture_sequence == session.capture_sequence
        && document_id == session.document.document_id
        && revision == session.document.revision
    })
  }

  fn handle_editor_actions(&mut self, actions: Vec<EditorAction>, context: &egui::Context) {
    for action in actions {
      if self.phase != Phase::Editing {
        break;
      }
      match action {
        EditorAction::Command(batch) => {
          let Some(session) = self.session.as_mut() else {
            continue;
          };
          if let Err(error) = session.history.execute_batch(&mut session.document, batch) {
            self.persistent_error = Some(error.to_string());
          }
        }
        EditorAction::Undo => {
          if let Some(session) = self.session.as_mut()
            && let Err(error) = session.history.undo(&mut session.document)
          {
            self.persistent_error = Some(error.to_string());
          }
        }
        EditorAction::Redo => {
          if let Some(session) = self.session.as_mut()
            && let Err(error) = session.history.redo(&mut session.document)
          {
            self.persistent_error = Some(error.to_string());
          }
        }
        EditorAction::Copy => {
          if let Some(session) = self.session.as_mut()
            && let Some(element_id) = session.editor.selected_element_id()
          {
            session.element_clipboard = session.document.element(element_id).cloned();
          }
        }
        EditorAction::Paste { position } => {
          let Some(session) = self.session.as_mut() else {
            continue;
          };
          let Some(source) = session.element_clipboard.as_ref() else {
            continue;
          };
          match common::DocumentCommand::paste_copy(
            source,
            common::ElementId::new(),
            position,
            &session.document,
          ) {
            Ok(command) => match session.history.execute(&mut session.document, command) {
              Ok(_) => {
                session.editor.set_selected_element_id(
                  session.document.highest_element().map(|e| e.element_id),
                );
              }
              Err(error) => self.persistent_error = Some(error.to_string()),
            },
            Err(error) => self.persistent_error = Some(error.to_string()),
          }
        }
        EditorAction::Save => {
          self.start_save(context);
          break;
        }
        EditorAction::Close => {
          self.close_editor(context);
          break;
        }
      }
    }
  }

  fn close_editor(&mut self, context: &egui::Context) {
    let Some(session) = self.session.as_ref() else {
      return;
    };
    match session.origin {
      SessionOrigin::NewCapture | SessionOrigin::LatestDraft { .. } => self.start_stash(context),
      SessionOrigin::ExistingDocument if session.is_dirty() => {
        self.phase = Phase::ConfirmingDiscard;
      }
      SessionOrigin::ExistingDocument => {
        self.remember_tool_and_release_session();
        self.phase = Phase::Idle;
        self.hide_editor_window(context);
      }
    }
    self.update_tray();
  }

  fn remember_tool_and_release_session(&mut self) {
    if let Some(session) = self.session.take() {
      self.last_tool = Some(session.editor.active_tool());
    }
  }

  fn show_editor_window(
    &mut self,
    context: &egui::Context,
    preferred_display_id: Option<u32>,
    display_bounds: Option<[i32; 4]>,
  ) {
    let window_timer = self.capture_presentation_trace.map(|trace| {
      PerformanceTimer::start(
        "capture.editor_commands.enqueue",
        trace.performance,
        PerformanceDetails::default().trigger(trace.trigger),
      )
    });
    let display_id = preferred_display_id
      .or_else(|| {
        display_bounds.and_then(|bounds| self.capture_surfaces.display_for_bounds(bounds))
      })
      .or_else(|| {
        global_cursor_position().and_then(|point| self.capture_surfaces.display_for_point(point))
      })
      .or_else(|| self.capture_surfaces.active_display_id())
      .or_else(|| self.capture_surfaces.fallback_display());
    let Some(display_id) = display_id else {
      self.persistent_error = Some("没有可用的截图编辑窗口".into());
      return;
    };
    let Some(session_id) = self.session.as_ref().map(|session| session.session_id) else {
      return;
    };
    let request_id = self
      .capture_presentation_trace
      .and_then(|trace| trace.performance.request_id)
      .unwrap_or(session_id);
    if let Some(image) =
      self.session.as_ref().and_then(|session| session.native_background.as_ref())
      && let Err(error) = self.capture_surfaces.set_frozen_image(display_id, image)
    {
      self.persistent_error = Some(error.to_string());
      self.capture_surfaces.hide_active();
      return;
    }
    let result = self
      .capture_surfaces
      .present(display_id, request_id)
      .and_then(|_| self.capture_surfaces.begin_editing(display_id, session_id));
    if let Err(error) = result {
      self.persistent_error = Some(error.to_string());
      self.capture_surfaces.hide_active();
      return;
    }
    context.request_repaint();
    if let Some(timer) = window_timer {
      timer.finish_ok();
    }
  }

  fn show_library_window(&mut self, context: &egui::Context) {
    self.surface = WindowSurface::Library;
    send_platform_fullscreen_command(context, false);
    send_platform_window_level_command(context, WindowLevel::Normal);
    context.send_viewport_cmd(ViewportCommand::Decorations(true));
    context.send_viewport_cmd(ViewportCommand::Resizable(true));
    context.send_viewport_cmd(ViewportCommand::InnerSize(LIBRARY_SIZE));
    context.send_viewport_cmd(ViewportCommand::Visible(true));
    context.send_viewport_cmd(ViewportCommand::Focus);
  }

  fn hide_library_window(&mut self, context: &egui::Context) {
    self.surface = WindowSurface::Hidden;
    send_platform_fullscreen_command(context, false);
    send_platform_window_level_command(context, WindowLevel::Normal);
    context.send_viewport_cmd(ViewportCommand::Decorations(true));
    context.send_viewport_cmd(ViewportCommand::Resizable(true));
    context.send_viewport_cmd(ViewportCommand::Visible(false));
  }

  fn hide_editor_window(&mut self, context: &egui::Context) {
    self.capture_surfaces.hide_active();
    context.request_repaint();
  }

  fn poll_external_events(&mut self, context: &egui::Context) {
    let hotkey_received_at = self.hotkey.as_ref().and_then(GlobalF1Hotkey::poll_capture_requested);
    if let Some(received_at) = hotkey_received_at {
      self.start_capture(context, CaptureTrigger { source: "hotkey", received_at });
    }

    let tray_action = self.tray.as_ref().and_then(TrayController::poll_action);
    if let Some(action) = tray_action {
      match action {
        TrayAction::Capture => self.start_capture(context, CaptureTrigger::now("tray")),
        TrayAction::ShowRecent => {
          if matches!(self.phase, Phase::Saving { .. }) {
            self.set_toast("保存完成后再打开最近讲义");
          } else {
            self.show_library_window(context);
          }
        }
        TrayAction::RestoreDraft => self.start_restore(context),
        TrayAction::ShowSettings => {
          if matches!(self.phase, Phase::Saving { .. }) {
            self.set_toast("保存完成后再修改设置");
          } else {
            self.settings_draft = self.settings.clone();
            self.show_settings = true;
            self.show_library_window(context);
          }
        }
        TrayAction::Quit => self.request_quit(context),
      }
    }

    while let Some(paths) = self.instance.try_recv() {
      for path in paths {
        self.start_import(path, context);
      }
    }
    let mut opened_file_groups = Vec::new();
    if let Some(open_file_bridge) = &self.open_file_bridge {
      while let Some(paths) = open_file_bridge.try_recv() {
        opened_file_groups.push(paths);
      }
    }
    for paths in opened_file_groups {
      for path in paths {
        self.start_import(path, context);
      }
    }
    let dropped: Vec<_> = context.input(|input| {
      input.raw.dropped_files.iter().map(|file| file.path().to_path_buf()).collect()
    });
    for path in dropped {
      self.start_import(path, context);
    }
  }

  fn request_quit(&mut self, context: &egui::Context) {
    match self.phase {
      Phase::Saving { .. } => {
        self.quit_after_persist = true;
      }
      Phase::Editing | Phase::ConfirmingDiscard => {
        let Some(session) = self.session.as_ref() else {
          return;
        };
        match session.origin {
          SessionOrigin::NewCapture | SessionOrigin::LatestDraft { .. } => {
            self.phase = Phase::Editing;
            self.start_stash(context);
            if self.phase == Phase::Idle {
              self.allow_close = true;
              context.send_viewport_cmd(ViewportCommand::Close);
            }
          }
          SessionOrigin::ExistingDocument if session.is_dirty() => {
            self.phase = Phase::Editing;
            self.exit_dialog = true;
          }
          SessionOrigin::ExistingDocument => {
            self.allow_close = true;
            context.send_viewport_cmd(ViewportCommand::Close);
          }
        }
      }
      _ => {
        self.allow_close = true;
        context.send_viewport_cmd(ViewportCommand::Close);
      }
    }
  }

  fn show_editor_ui(&mut self, root_ui: &mut egui::Ui, context: &egui::Context) {
    if self.persistent_error.is_some() && self.phase == Phase::Editing {
      let message = self.persistent_error.clone().unwrap_or_default();
      egui::Panel::bottom("editor-error")
        .frame(egui::Frame::new().fill(Color32::from_rgb(92, 30, 28)).inner_margin(12.0))
        .show(root_ui, |ui| {
          ui.horizontal(|ui| {
            ui.label(message);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
              if ui.button("关闭").clicked() {
                self.persistent_error = None;
                self.retry_kind = None;
              }
              if self.retry_kind.is_some() && ui.button("重试").clicked() {
                match self.retry_kind {
                  Some(RetryKind::Save) => self.start_save(context),
                  None => {}
                }
              }
            });
          });
        });
    }

    let actions = if let Some(session) = self.session.as_mut() {
      let mut actions = Vec::new();
      let background_fill =
        if session.background_texture.is_some() { Color32::BLACK } else { Color32::TRANSPARENT };
      egui::CentralPanel::default().frame(egui::Frame::NONE.fill(background_fill)).show(
        root_ui,
        |ui| match self.phase {
          Phase::Editing => {
            actions = session.editor.show(
              ui,
              &session.document,
              &session.history,
              session.background_texture.as_ref(),
            );
          }
          Phase::Saving { .. } | Phase::ConfirmingDiscard => {
            session.editor.show_read_only(
              ui,
              &session.document,
              session.background_texture.as_ref(),
            );
          }
          _ => {
            session.editor.show_read_only(
              ui,
              &session.document,
              session.background_texture.as_ref(),
            );
          }
        },
      );
      actions
    } else {
      Vec::new()
    };
    if self.phase == Phase::Editing {
      self.handle_editor_actions(actions, context);
    }

    if matches!(self.phase, Phase::Saving { .. }) {
      egui::Area::new(egui::Id::new("persisting-overlay"))
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .order(egui::Order::Tooltip)
        .show(context, |ui| {
          egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
              ui.spinner();
              ui.label("正在保存...");
            });
          });
        });
    }
  }

  fn show_library_ui(&mut self, root_ui: &mut egui::Ui, context: &egui::Context) {
    let documents: Vec<_> = self.recent.visible_documents().cloned().collect();
    let mut action = None;
    egui::Panel::top("library-header")
      .frame(egui::Frame::new().fill(Color32::from_rgb(29, 29, 31)).inner_margin(16.0))
      .show(root_ui, |ui| {
        let row_height = library_header_row_height(ui);
        fixed_height_centered_row(ui, row_height, |ui| {
          ui.heading("RS Board");
          ui.separator();
          if self.phase.has_active_session() && ui.button("返回编辑器").clicked() {
            self.show_editor_window(context, None, None);
          }
          if ui
            .add_enabled(self.phase == Phase::Idle, egui::Button::new("新截图"))
            .on_hover_text("捕获鼠标所在屏幕 (F1)")
            .clicked()
          {
            self.start_capture(context, CaptureTrigger::now("library"));
          }
          if ui
            .add_enabled(
              self.draft_available && self.phase == Phase::Idle,
              egui::Button::new("恢复草稿"),
            )
            .clicked()
          {
            self.start_restore(context);
          }
          if ui.add_enabled(self.phase == Phase::Idle, egui::Button::new("导入")).clicked()
            && let Some(path) =
              rfd::FileDialog::new().add_filter("RS Board 讲义", &["rsboard"]).pick_file()
          {
            self.start_import(path, context);
          }
          ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("设置").clicked() {
              self.settings_draft = self.settings.clone();
              self.show_settings = true;
            }
            ui.add_sized([260.0, 30.0], library_search_field(&mut self.recent.query));
          });
        });
      });

    egui::CentralPanel::default()
      .frame(
        egui::Frame::NONE.fill(Color32::from_rgb(24, 24, 25)).inner_margin(LIBRARY_PANEL_MARGIN),
      )
      .show(root_ui, |ui| {
        if let Some(error) = self.library_error.clone() {
          egui::Frame::new().fill(Color32::from_rgb(82, 31, 29)).inner_margin(10.0).show(
            ui,
            |ui| {
              ui.horizontal(|ui| {
                ui.label(error);
                if ui.button("关闭").clicked() {
                  self.library_error = None;
                }
                if self.draft_available
                  && self.phase == Phase::Idle
                  && ui.button("删除草稿").clicked()
                {
                  self.delete_draft_dialog = true;
                }
              });
            },
          );
          ui.add_space(10.0);
        }

        match self.phase {
          Phase::Capturing { .. } => busy_line(ui, "正在捕获屏幕"),
          Phase::Opening { .. } => busy_line(ui, "正在打开讲义"),
          Phase::Restoring { .. } => busy_line(ui, "正在恢复草稿"),
          _ => {}
        }

        if self.phase.has_active_session() {
          egui::Frame::new().fill(Color32::from_rgb(45, 42, 34)).inner_margin(10.0).show(
            ui,
            |ui| {
              ui.label("当前编辑会话仍保留在内存中；打开讲义和恢复草稿已暂时禁用。");
            },
          );
          ui.add_space(10.0);
        }

        if documents.is_empty() {
          ui.centered_and_justified(|ui| {
            ui.label(if self.recent.query.trim().is_empty() {
              "暂无讲义"
            } else {
              "没有匹配的讲义"
            });
          });
          return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
          let card_width = library_document_card_width();
          egui::Grid::new("recent-grid")
            .num_columns(2)
            .spacing([LIBRARY_GRID_GAP, LIBRARY_GRID_GAP])
            .show(ui, |ui| {
              for (index, document) in documents.iter().enumerate() {
                let preview = self.preview_texture(document, context);
                let highlighted = self.recent.highlighted == Some(document.document_id);
                let frame = egui::Frame::new()
                  .fill(if highlighted {
                    Color32::from_rgb(58, 42, 41)
                  } else {
                    Color32::from_rgb(38, 38, 40)
                  })
                  .stroke(egui::Stroke::new(
                    1.0,
                    if highlighted {
                      Color32::from_rgb(230, 76, 70)
                    } else {
                      Color32::from_gray(64)
                    },
                  ))
                  .corner_radius(6.0)
                  .inner_margin(LIBRARY_CARD_INNER_MARGIN);
                let response = frame.show(ui, |ui| {
                  ui.set_width(card_width - 2.0 * LIBRARY_CARD_INNER_MARGIN);
                  let preview_size = LIBRARY_PREVIEW_SIZE;
                  fixed_height_centered_row(ui, preview_size.y, |ui| {
                    let (preview_rect, preview_response) =
                      ui.allocate_exact_size(preview_size, egui::Sense::click());
                    ui.painter().rect_filled(preview_rect, 3.0, Color32::BLACK);
                    if let Some(texture) = preview {
                      ui.painter().image(
                        texture.id(),
                        fit_image_rect(texture.size_vec2(), preview_rect),
                        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                      );
                    } else {
                      ui.painter().text(
                        preview_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "预览生成中",
                        egui::FontId::proportional(13.0),
                        Color32::from_gray(130),
                      );
                    }
                    ui.add_space(LIBRARY_CARD_CONTENT_GAP);
                    let description_width = library_document_description_width(ui);
                    let description_height = library_document_description_height(ui);
                    centered_vertical_slot(
                      ui,
                      egui::vec2(description_width, preview_size.y),
                      description_height,
                      |ui| {
                        ui.add(
                          egui::Label::new(egui::RichText::new(&document.title).strong())
                            .truncate(),
                        )
                        .on_hover_text(&document.title);
                        ui.label(
                          egui::RichText::new(
                            document
                              .updated_at
                              .with_timezone(&chrono::Local)
                              .format("%Y-%m-%d %H:%M")
                              .to_string(),
                          )
                          .small()
                          .color(Color32::from_gray(150)),
                        );
                      },
                    );
                    let action_height = ui.text_style_height(&egui::TextStyle::Button)
                      + 2.0 * ui.spacing().button_padding.y;
                    centered_vertical_slot(
                      ui,
                      egui::vec2(LIBRARY_CARD_ACTION_WIDTH, preview_size.y),
                      action_height,
                      |ui| {
                        ui.menu_button("⋯", |ui| {
                          if ui.button("重命名").clicked() {
                            action = Some(LibraryAction::Rename(document.document_id));
                            ui.close();
                          }
                          if ui.button("复制图片").clicked() {
                            action = Some(LibraryAction::CopyImage(document.document_id));
                            ui.close();
                          }
                          if ui.button("导出 PNG").clicked() {
                            action = Some(LibraryAction::ExportPng(document.document_id));
                            ui.close();
                          }
                          if ui.button("导出讲义").clicked() {
                            action = Some(LibraryAction::ExportBundle(document.document_id));
                            ui.close();
                          }
                          ui.separator();
                          if ui
                            .button(
                              egui::RichText::new("删除").color(Color32::from_rgb(255, 100, 95)),
                            )
                            .clicked()
                          {
                            action = Some(LibraryAction::Delete(document.document_id));
                            ui.close();
                          }
                        });
                      },
                    );
                    preview_response
                  })
                  .inner
                });
                if (response.inner.double_clicked() || response.response.double_clicked())
                  && self.phase == Phase::Idle
                {
                  action = Some(LibraryAction::Open(document.document_id));
                }
                if (index + 1) % 2 == 0 {
                  ui.end_row();
                }
              }
            });
        });
      });

    if let Some(action) = action {
      self.handle_library_action(action, context);
    }
  }

  fn preview_texture(
    &mut self,
    document: &crate::storage::DocumentSummary,
    context: &egui::Context,
  ) -> Option<TextureHandle> {
    let path = document.preview_path.as_ref()?;
    if let Some((cached_path, texture)) = self.preview_textures.get(&document.document_id)
      && cached_path == path
    {
      return Some(texture.clone());
    }
    let image = image::open(path).ok()?.into_rgba8();
    let texture = context.load_texture(
      format!("preview-{}-{}", document.document_id, document.revision),
      egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
      ),
      TextureOptions::LINEAR,
    );
    self.preview_textures.insert(document.document_id, (path.clone(), texture.clone()));
    Some(texture)
  }

  fn handle_library_action(&mut self, action: LibraryAction, context: &egui::Context) {
    match action {
      LibraryAction::Open(document_id) => self.start_open_document(document_id, context),
      LibraryAction::Rename(document_id) => {
        if let Some(document) =
          self.recent.documents.iter().find(|item| item.document_id == document_id)
        {
          self.rename_dialog = Some((document_id, document.title.clone()));
        }
      }
      LibraryAction::CopyImage(document_id) => {
        self.start_render_export(document_id, None, true, context)
      }
      LibraryAction::ExportPng(document_id) => {
        let title = self
          .recent
          .documents
          .iter()
          .find(|item| item.document_id == document_id)
          .map(|item| sanitized_file_stem(&item.title))
          .unwrap_or_else(|| "讲义".into());
        if let Some(path) = rfd::FileDialog::new()
          .add_filter("PNG 图片", &["png"])
          .set_file_name(format!("{title}.png"))
          .save_file()
        {
          self.start_render_export(document_id, Some(path), false, context);
        }
      }
      LibraryAction::ExportBundle(document_id) => {
        if let Some(directory) = rfd::FileDialog::new().pick_folder() {
          let store = self.store.clone();
          if let Err(error) = self.spawn_worker(context, move || {
            WorkerEvent::Auxiliary(
              store
                .export_document(document_id, &directory)
                .map(|bundle| format!("已导出 {}.rsboard", bundle.stem))
                .map_err(|error| error.to_string()),
            )
          }) {
            self.set_toast(format!("无法启动讲义导出任务：{error}"));
          }
        }
      }
      LibraryAction::Delete(document_id) => self.delete_document_dialog = Some(document_id),
    }
  }

  fn start_render_export(
    &mut self,
    document_id: DocumentId,
    destination: Option<PathBuf>,
    clipboard: bool,
    context: &egui::Context,
  ) {
    let store = self.store.clone();
    if let Err(error) = self.spawn_worker(context, move || {
      let result = (|| {
        let loaded = store.open_document(document_id).map_err(|error| error.to_string())?;
        let (_, _, pixels) = loaded.background.decode_rgba8().map_err(|error| error.to_string())?;
        let background = RgbaImage::from_raw(
          loaded.document.canvas_size_px.width_px,
          loaded.document.canvas_size_px.height_px,
          pixels.to_vec(),
        )
        .ok_or_else(|| "背景像素无效".to_owned())?;
        let output = render_document_to_image(&loaded.document, &background);
        if clipboard {
          copy_image(&output).map_err(|error| error.to_string())?;
          Ok("图片已复制".into())
        } else {
          let destination = destination.ok_or_else(|| "未选择导出路径".to_owned())?;
          write_png_atomically(&destination, &output).map_err(|error| error.to_string())?;
          Ok("PNG 已导出".into())
        }
      })();
      WorkerEvent::Auxiliary(result)
    }) {
      self.set_toast(format!("无法启动图片导出任务：{error}"));
    }
  }

  fn show_dialogs(&mut self, context: &egui::Context) {
    if self.phase == Phase::ConfirmingDiscard {
      egui::Window::new("放弃修改？")
        .id(egui::Id::new("discard-document-dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(context, |ui| {
          ui.label("这份正式讲义包含未保存修改。");
          ui.horizontal(|ui| {
            if ui.button("继续编辑").clicked() {
              self.phase = Phase::Editing;
            }
            if ui
              .button(egui::RichText::new("放弃修改").color(Color32::from_rgb(255, 105, 98)))
              .clicked()
            {
              self.remember_tool_and_release_session();
              self.phase = Phase::Idle;
              self.hide_editor_window(context);
            }
          });
        });
    }

    if self.exit_dialog {
      let mut choice = None;
      egui::Window::new("退出 RS Board？")
        .id(egui::Id::new("exit-document-dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(context, |ui| {
          ui.label("正式讲义还有未保存的修改。");
          ui.horizontal(|ui| {
            if ui.button("取消").clicked() {
              choice = Some(ExitChoice::Cancel);
            }
            if ui.button("不保存并退出").clicked() {
              choice = Some(ExitChoice::Discard);
            }
            if ui.button("保存并退出").clicked() {
              choice = Some(ExitChoice::Save);
            }
          });
        });
      match choice {
        Some(ExitChoice::Cancel) => self.exit_dialog = false,
        Some(ExitChoice::Discard) => {
          self.exit_dialog = false;
          self.allow_close = true;
          context.send_viewport_cmd(ViewportCommand::Close);
        }
        Some(ExitChoice::Save) => {
          self.exit_dialog = false;
          self.quit_after_persist = true;
          self.start_save(context);
        }
        None => {}
      }
    }

    if self.show_settings {
      let mut close = false;
      let mut save = false;
      egui::Window::new("设置")
        .id(egui::Id::new("settings-dialog"))
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(context, |ui| {
          egui::Grid::new("settings-grid").num_columns(2).spacing([18.0, 12.0]).show(ui, |ui| {
            ui.label("全局快捷键");
            ui.text_edit_singleline(&mut self.settings_draft.global_hotkey);
            ui.end_row();
            ui.label("截图包含光标");
            ui.checkbox(&mut self.settings_draft.include_cursor, "");
            ui.end_row();
            ui.label("登录时启动");
            ui.checkbox(&mut self.settings_draft.launch_at_login, "");
            ui.end_row();
            ui.label("保存后复制图片");
            ui.checkbox(&mut self.settings_draft.copy_image_after_save, "");
            ui.end_row();
          });
          ui.separator();
          if ui
            .add_enabled(
              self.phase == Phase::Idle,
              egui::Button::new(
                egui::RichText::new("清除所有讲义和草稿").color(Color32::from_rgb(255, 105, 98)),
              ),
            )
            .clicked()
          {
            self.clear_confirmation_stage = 1;
          }
          ui.separator();
          ui.horizontal(|ui| {
            if ui.button("取消").clicked() {
              close = true;
            }
            if ui.button("保存").clicked() {
              save = true;
            }
          });
        });
      if close {
        self.settings_draft = self.settings.clone();
        self.show_settings = false;
      }
      if save {
        self.apply_settings(context);
      }
    }

    if let Some((document_id, current_title)) = self.rename_dialog.as_mut() {
      let document_id = *document_id;
      let mut cancel = false;
      let mut commit = false;
      egui::Window::new("重命名讲义")
        .id(egui::Id::new("rename-dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(context, |ui| {
          ui.add_sized([340.0, 28.0], egui::TextEdit::singleline(current_title));
          ui.horizontal(|ui| {
            cancel = ui.button("取消").clicked();
            commit = ui
              .add_enabled(!current_title.trim().is_empty(), egui::Button::new("重命名"))
              .clicked();
          });
        });
      if cancel {
        self.rename_dialog = None;
      } else if commit {
        let title = current_title.clone();
        self.rename_dialog = None;
        let store = self.store.clone();
        if let Err(error) = self.spawn_worker(context, move || {
          WorkerEvent::LibraryChanged(
            store
              .rename_document(document_id, title)
              .map(|_| Some(document_id))
              .map_err(|error| error.to_string()),
          )
        }) {
          self.library_error = Some(format!("无法启动讲义重命名任务：{error}"));
        }
      }
    }

    if let Some(document_id) = self.delete_document_dialog {
      let mut cancel = false;
      let mut confirm = false;
      egui::Window::new("删除讲义？")
        .id(egui::Id::new("delete-document-dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(context, |ui| {
          ui.label("删除后无法恢复，最新草稿不受影响。");
          ui.horizontal(|ui| {
            cancel = ui.button("取消").clicked();
            confirm = ui
              .button(egui::RichText::new("永久删除").color(Color32::from_rgb(255, 105, 98)))
              .clicked();
          });
        });
      if cancel {
        self.delete_document_dialog = None;
      } else if confirm {
        self.delete_document_dialog = None;
        let store = self.store.clone();
        if let Err(error) = self.spawn_worker(context, move || {
          WorkerEvent::LibraryChanged(
            store.delete_document(document_id).map(|_| None).map_err(|error| error.to_string()),
          )
        }) {
          self.library_error = Some(format!("无法启动讲义删除任务：{error}"));
        }
      }
    }

    if self.delete_draft_dialog {
      let mut cancel = false;
      let mut confirm = false;
      egui::Window::new("删除最新草稿？")
        .id(egui::Id::new("delete-draft-dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(context, |ui| {
          ui.label("草稿删除后无法恢复，正式讲义不受影响。");
          ui.horizontal(|ui| {
            cancel = ui.button("取消").clicked();
            confirm = ui
              .button(egui::RichText::new("永久删除").color(Color32::from_rgb(255, 105, 98)))
              .clicked();
          });
        });
      if cancel {
        self.delete_draft_dialog = false;
      } else if confirm {
        self.delete_draft_dialog = false;
        if let Err(error) = self.draft_coordinator.delete_latest() {
          self.library_error = Some(format!("无法提交草稿删除任务：{error}"));
        }
      }
    }

    self.show_clear_confirmation(context);
  }

  fn show_clear_confirmation(&mut self, context: &egui::Context) {
    if self.clear_confirmation_stage == 0 {
      return;
    }
    let stage = self.clear_confirmation_stage;
    let mut cancel = false;
    let mut confirm = false;
    egui::Window::new(if stage == 1 { "清除全部内容？" } else { "最后确认" })
      .id(egui::Id::new("clear-all-dialog"))
      .collapsible(false)
      .resizable(false)
      .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
      .show(context, |ui| {
        ui.label(if stage == 1 {
          "将永久删除所有正式讲义和最新草稿。"
        } else {
          "此操作不可撤销。确认继续？"
        });
        ui.horizontal(|ui| {
          cancel = ui.button("取消").clicked();
          confirm = ui
            .button(
              egui::RichText::new(if stage == 1 { "继续" } else { "永久清除" })
                .color(Color32::from_rgb(255, 105, 98)),
            )
            .clicked();
        });
      });
    if cancel {
      self.clear_confirmation_stage = 0;
    } else if confirm && stage == 1 {
      self.clear_confirmation_stage = 2;
    } else if confirm {
      self.clear_confirmation_stage = 0;
      if let Err(error) = self.draft_coordinator.clear_all() {
        self.library_error = Some(format!("无法提交内容清理任务：{error}"));
      }
    }
  }

  fn apply_settings(&mut self, context: &egui::Context) {
    let old = self.settings.clone();
    let next = self.settings_draft.clone();
    if next.global_hotkey != old.global_hotkey {
      let hotkey_result = if let Some(hotkey) = self.hotkey.as_mut() {
        hotkey.update_shortcut(&next.global_hotkey)
      } else {
        GlobalF1Hotkey::from_shortcut_with_waker(&next.global_hotkey, {
          let context = context.clone();
          move || context.request_repaint()
        })
        .map(|hotkey| self.hotkey = Some(hotkey))
      };
      if let Err(error) = hotkey_result {
        self.library_error = Some(error.to_string());
        return;
      }
    }
    if next.launch_at_login != old.launch_at_login
      && let Err(error) = set_launch_at_login(next.launch_at_login)
    {
      if let Some(hotkey) = self.hotkey.as_mut()
        && next.global_hotkey != old.global_hotkey
      {
        let _ = hotkey.update_shortcut(&old.global_hotkey);
      }
      self.library_error = Some(error.to_string());
      return;
    }
    if let Err(error) = next.save(&self.settings_path) {
      if next.launch_at_login != old.launch_at_login {
        let _ = set_launch_at_login(old.launch_at_login);
      }
      if let Some(hotkey) = self.hotkey.as_mut()
        && next.global_hotkey != old.global_hotkey
      {
        let _ = hotkey.update_shortcut(&old.global_hotkey);
      }
      self.library_error = Some(error.to_string());
      return;
    }
    self.settings = next.clone();
    self.settings_draft = next;
    self.show_settings = false;
    self.set_toast("设置已保存");
  }

  fn show_toast(&mut self, context: &egui::Context) {
    let Some((message, created_at)) = self.toast.as_ref() else {
      return;
    };
    if created_at.elapsed() > Duration::from_secs(4) {
      self.toast = None;
      return;
    }
    egui::Area::new(egui::Id::new("app-toast"))
      .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 16.0))
      .order(egui::Order::Tooltip)
      .show(context, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
          ui.label(message);
        });
      });
    context.request_repaint_after(Duration::from_millis(250));
  }

  fn show_capture_viewports(&mut self, context: &egui::Context) {
    for viewport in self.capture_surfaces.viewport_specs() {
      let is_active = viewport.is_active();
      let lifecycle = viewport.lifecycle;
      context.show_viewport_immediate(
        viewport.viewport_id,
        viewport.builder,
        |ui, _viewport_class| {
          if !is_active {
            egui::CentralPanel::default()
              .frame(egui::Frame::NONE.fill(Color32::TRANSPARENT))
              .show(ui, |_| {});
            return;
          }

          let viewport_context = ui.ctx().clone();
          if viewport_context.input(|input| input.viewport().close_requested())
            && self.phase.has_active_session()
          {
            viewport_context.send_viewport_cmd(ViewportCommand::CancelClose);
            self.close_editor(&viewport_context);
          }
          if matches!(
            lifecycle,
            SurfaceLifecycle::Presenting { .. } | SurfaceLifecycle::Editing { .. }
          ) && self.session.is_some()
          {
            self.show_editor_ui(ui, &viewport_context);
            self.show_dialogs(&viewport_context);
            self.show_toast(&viewport_context);
            self.finish_capture_presentation_trace();
          }
        },
      );
    }
  }

  fn finish_capture_presentation_trace(&mut self) {
    let Some(trace) = self.capture_presentation_trace.take() else {
      return;
    };
    let mut details = PerformanceDetails::default().trigger(trace.trigger);
    if let Some(pixel_size) = trace.pixel_size {
      details = details.pixel_size(pixel_size);
    }
    let frame_timer = PerformanceTimer::started_at(
      "capture.editor_frame_submitted",
      trace.performance,
      details,
      trace.started_at,
    );
    let total_timer = PerformanceTimer::started_at(
      "capture.request.total",
      trace.performance,
      details,
      trace.started_at,
    );
    let matches_session = self
      .session
      .as_ref()
      .is_some_and(|session| session.capture_sequence == trace.performance.capture_sequence);
    if matches_session {
      frame_timer.finish_ok();
      total_timer.finish_ok();
    } else {
      frame_timer.finish_stale();
      total_timer.finish_stale();
    }
  }
}

impl eframe::App for RsBoardApp {
  fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
    self.handle_draft_results(context);
    self.handle_post_save_results();
    self.handle_worker_events(context);
    self.poll_external_events(context);
    if self.capture_surfaces.should_refresh() {
      match self.capture_surfaces.refresh_available_displays() {
        Ok(DisplayRefreshOutcome::ActiveDisplayRemoved(_)) => {
          invalidate_capture_backend_cache();
          self.capture_surfaces.hide_active();
          if self.phase == Phase::Editing {
            self.close_editor(context);
          }
        }
        Ok(DisplayRefreshOutcome::DisplaysChanged) => invalidate_capture_backend_cache(),
        Ok(DisplayRefreshOutcome::Unchanged) => {}
        Err(error) => eprintln!("capture_surface_refresh_failed error={error:?}"),
      }
    }
  }

  fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    let context = ui.ctx().clone();

    if context.input(|input| input.viewport().close_requested()) && !self.allow_close {
      context.send_viewport_cmd(ViewportCommand::CancelClose);
      if self.surface == WindowSurface::Library || self.phase == Phase::Idle {
        self.hide_library_window(&context);
      } else {
        self.request_quit(&context);
      }
    }

    match self.surface {
      WindowSurface::Library => self.show_library_ui(ui, &context),
      WindowSurface::Hidden => {
        egui::CentralPanel::default()
          .frame(egui::Frame::NONE.fill(Color32::TRANSPARENT))
          .show(ui, |_| {});
      }
    }
    if self.capture_surfaces.active_display_id().is_none() {
      self.show_dialogs(&context);
      self.show_toast(&context);
    }
    self.show_capture_viewports(&context);
  }

  fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
    Color32::TRANSPARENT.to_normalized_gamma_f32()
  }

  fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
    if !self.draft_coordinator.shutdown(Duration::from_secs(2)) {
      eprintln!("draft_coordinator_shutdown_timeout timeout_ms=2000");
    }
  }
}

#[derive(Debug, Error)]
pub enum ApplicationError {
  #[error(transparent)]
  Settings(#[from] SettingsError),
  #[error(transparent)]
  Storage(#[from] StorageError),
  #[error(transparent)]
  Capture(#[from] crate::capture::CaptureError),
  #[error(transparent)]
  BackgroundPrepare(#[from] BackgroundPrepareError),
  #[error(transparent)]
  DraftCoordinator(#[from] DraftCoordinatorError),
  #[error(transparent)]
  PostSaveCoordinator(#[from] PostSaveCoordinatorError),
  #[error(transparent)]
  Document(#[from] common::DocumentError),
  #[error("背景纹理无效")]
  InvalidTexture,
}

fn load_rgba_texture(
  context: &egui::Context,
  name: &str,
  size: SizePx,
  pixels: &[u8],
) -> Result<TextureHandle, ApplicationError> {
  let expected = size.width_px as usize * size.height_px as usize * 4;
  if pixels.len() != expected {
    return Err(ApplicationError::InvalidTexture);
  }
  Ok(context.load_texture(
    name,
    egui::ColorImage::from_rgba_unmultiplied(
      [size.width_px as usize, size.height_px as usize],
      pixels,
    ),
    TextureOptions::LINEAR,
  ))
}

fn next_sequence(sequence: &mut u64) -> u64 {
  *sequence = sequence.checked_add(1).expect("performance sequence exhausted");
  *sequence
}

fn performance_from_persistence_context(context: PersistenceContext) -> PerformanceContext {
  PerformanceContext {
    request_id: Some(context.request_id),
    session_id: Some(context.session_id),
    capture_sequence: context.capture_sequence,
    stash_sequence: context.stash_sequence,
    generation_id: context.generation_id.map(GenerationId::as_uuid),
    ..PerformanceContext::default()
  }
}

fn fixed_height_centered_row<R>(
  ui: &mut egui::Ui,
  height: f32,
  add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
  ui.allocate_ui_with_layout(
    egui::vec2(ui.available_width(), height),
    egui::Layout::left_to_right(egui::Align::Center),
    add_contents,
  )
}

fn centered_vertical_slot<R>(
  ui: &mut egui::Ui,
  slot_size: egui::Vec2,
  content_height: f32,
  add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> (R, egui::Rect) {
  let (slot_rect, _) = ui.allocate_exact_size(slot_size, egui::Sense::hover());
  let content_rect = egui::Rect::from_center_size(
    slot_rect.center(),
    egui::vec2(slot_size.x, content_height.min(slot_size.y)),
  );
  let mut content_ui = ui.new_child(
    egui::UiBuilder::new().max_rect(content_rect).layout(egui::Layout::top_down(egui::Align::Min)),
  );
  let inner = add_contents(&mut content_ui);
  (inner, content_ui.min_rect())
}

fn library_header_row_height(ui: &egui::Ui) -> f32 {
  let button_height =
    ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y;
  button_height.max(ui.text_style_height(&egui::TextStyle::Heading)).max(30.0)
}

fn library_document_card_width() -> f32 {
  LIBRARY_CARD_WIDTH
}

fn library_document_description_width(ui: &egui::Ui) -> f32 {
  LIBRARY_CARD_WIDTH
    - 2.0 * LIBRARY_CARD_INNER_MARGIN
    - LIBRARY_PREVIEW_SIZE.x
    - LIBRARY_CARD_CONTENT_GAP
    - 2.0 * ui.spacing().item_spacing.x
    - LIBRARY_CARD_ACTION_WIDTH
}

fn library_search_field(query: &mut String) -> egui::TextEdit<'_> {
  egui::TextEdit::singleline(query)
    .hint_text("搜索标题")
    .margin(egui::Margin::symmetric(10, 2))
    .vertical_align(egui::Align::Center)
}

fn library_document_description_height(ui: &egui::Ui) -> f32 {
  ui.text_style_height(&egui::TextStyle::Body)
    + ui.spacing().item_spacing.y
    + ui.text_style_height(&egui::TextStyle::Small)
}

#[cfg(not(target_os = "macos"))]
fn send_platform_fullscreen_command(context: &egui::Context, fullscreen: bool) {
  context.send_viewport_cmd(ViewportCommand::Fullscreen(fullscreen));
}

#[cfg(target_os = "macos")]
fn send_platform_fullscreen_command(_context: &egui::Context, _fullscreen: bool) {}

#[cfg(not(target_os = "macos"))]
fn send_platform_window_level_command(context: &egui::Context, level: WindowLevel) {
  context.send_viewport_cmd(ViewportCommand::WindowLevel(level));
}

#[cfg(target_os = "macos")]
fn send_platform_window_level_command(_context: &egui::Context, _level: WindowLevel) {}

fn configure_egui(context: &egui::Context) {
  let mut fonts = egui::FontDefinitions::default();
  fonts
    .font_data
    .insert("rs-board-cjk".into(), Arc::new(egui::FontData::from_owned(BUNDLED_CJK_FONT.to_vec())));
  for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
    fonts.families.entry(family).or_default().insert(0, "rs-board-cjk".into());
  }
  context.set_fonts(fonts);

  context.all_styles_mut(|style| {
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.visuals.panel_fill = Color32::from_rgb(24, 24, 25);
    style.visuals.window_fill = Color32::from_rgb(32, 32, 34);
    style.visuals.selection.bg_fill = Color32::from_rgb(210, 54, 52);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(184, 45, 44);
  });
  context.set_theme(egui::Theme::Dark);
}

fn busy_line(ui: &mut egui::Ui, message: &str) {
  ui.horizontal(|ui| {
    ui.spinner();
    ui.label(message);
  });
  ui.add_space(12.0);
}

fn fit_image_rect(image_size: egui::Vec2, container: egui::Rect) -> egui::Rect {
  if image_size.x <= 0.0 || image_size.y <= 0.0 {
    return container;
  }
  let scale = (container.width() / image_size.x).min(container.height() / image_size.y).max(0.0);
  egui::Rect::from_center_size(container.center(), image_size * scale)
}

fn sanitized_file_stem(title: &str) -> String {
  let mut output = String::with_capacity(title.len().min(120));
  let mut previous_was_space = false;
  for character in title.trim().chars() {
    let character = match character {
      '/' | '\\' | ':' => '_',
      value if value.is_control() => continue,
      value if value.is_whitespace() => ' ',
      value => value,
    };
    if character == ' ' {
      if previous_was_space {
        continue;
      }
      previous_was_space = true;
    } else {
      previous_was_space = false;
    }
    if output.len() + character.len_utf8() > 120 {
      break;
    }
    output.push(character);
  }
  let output = output.trim_matches([' ', '.']).to_owned();
  if output.is_empty() { "未命名讲义".to_owned() } else { output }
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;

  use super::*;

  #[test]
  fn library_header_and_document_row_center_contents_vertically() {
    let context = egui::Context::default();
    configure_egui(&context);
    let header_centers = Cell::new([0.0; 5]);
    let document_centers = Cell::new([0.0; 3]);
    let document_card_rects = Cell::new([egui::Rect::NOTHING; 2]);
    let mut query = String::new();

    context
      .run_ui(
        egui::RawInput {
          screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))),
          ..Default::default()
        },
        |ui| {
          let header_height = library_header_row_height(ui);
          fixed_height_centered_row(ui, header_height, |ui| {
            let title = ui.heading("RS Board");
            let separator = ui.separator();
            let capture = ui.button("新截图");
            let (settings_center, search_center) = ui
              .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let settings = ui.button("设置");
                let search = ui.add_sized([260.0, 30.0], library_search_field(&mut query));
                (settings.rect.center().y, search.rect.center().y)
              })
              .inner;
            header_centers.set([
              title.rect.center().y,
              separator.rect.center().y,
              capture.rect.center().y,
              settings_center,
              search_center,
            ]);
          });

          ui.set_width(LIBRARY_SIZE.x - 2.0 * LIBRARY_PANEL_MARGIN - LIBRARY_SCROLLBAR_RESERVE);
          egui::Grid::new("library-layout-test-grid")
            .num_columns(2)
            .spacing([LIBRARY_GRID_GAP, LIBRARY_GRID_GAP])
            .show(ui, |ui| {
              let mut card_rects = [egui::Rect::NOTHING; 2];
              for card_rect in &mut card_rects {
                let card =
                  egui::Frame::new().inner_margin(LIBRARY_CARD_INNER_MARGIN).show(ui, |ui| {
                    ui.set_width(LIBRARY_CARD_WIDTH - 2.0 * LIBRARY_CARD_INNER_MARGIN);
                    fixed_height_centered_row(ui, LIBRARY_PREVIEW_SIZE.y, |ui| {
                      let (_, preview) =
                        ui.allocate_exact_size(LIBRARY_PREVIEW_SIZE, egui::Sense::hover());
                      ui.add_space(LIBRARY_CARD_CONTENT_GAP);
                      let description_height = library_document_description_height(ui);
                      let (_, description_rect) = centered_vertical_slot(
                        ui,
                        egui::vec2(library_document_description_width(ui), LIBRARY_PREVIEW_SIZE.y),
                        description_height,
                        |ui| {
                          ui.add(
                            egui::Label::new(
                              egui::RichText::new("截图 2026-08-07 13:12:08").strong(),
                            )
                            .truncate(),
                          );
                          ui.label(egui::RichText::new("2026-08-07 13:21").small());
                        },
                      );
                      let action_height = ui.text_style_height(&egui::TextStyle::Button)
                        + 2.0 * ui.spacing().button_padding.y;
                      let (_, action_rect) = centered_vertical_slot(
                        ui,
                        egui::vec2(LIBRARY_CARD_ACTION_WIDTH, LIBRARY_PREVIEW_SIZE.y),
                        action_height,
                        |ui| ui.menu_button("⋯", |_| {}).response.rect,
                      );
                      document_centers.set([
                        preview.rect.center().y,
                        description_rect.center().y,
                        action_rect.center().y,
                      ]);
                    });
                  });
                *card_rect = card.response.rect;
              }
              document_card_rects.set(card_rects);
            });
        },
      )
      .drop_without_applying_deltas();

    assert_same_center(header_centers.get());
    assert_same_center(document_centers.get());
    let [first_card, second_card] = document_card_rects.get();
    assert!((first_card.width() - LIBRARY_CARD_WIDTH).abs() < 0.1);
    assert!((second_card.width() - LIBRARY_CARD_WIDTH).abs() < 0.1);
    assert!((second_card.left() - first_card.right() - LIBRARY_GRID_GAP).abs() < 0.1);
    assert!(
      second_card.right() <= LIBRARY_SIZE.x - LIBRARY_PANEL_MARGIN - LIBRARY_SCROLLBAR_RESERVE
    );
  }

  fn assert_same_center<const N: usize>(centers: [f32; N]) {
    for pair in centers.windows(2) {
      assert!((pair[0] - pair[1]).abs() < 0.1, "centers: {centers:?}");
    }
  }
}
