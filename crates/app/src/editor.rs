use common::{
  ArrowEndpoint, ArrowHead, ArrowPayload, BoardDocument, ColorRgba, CommandBatch, CommandHistory,
  DocumentCommand, Element, ElementId, ElementLabel, ElementPayload, PRESET_BRUSH_HARDNESSES,
  PRESET_FONT_SIZES_PX, PRESET_STROKE_WIDTHS_PX, PointPx, RectPx, RectangleLabelAnchor,
  RectangleLabelEdge, RectangleLabelScene, RectangleLabelSide, RectangleLabelSolution,
  RectanglePayload, SequenceMarkerPayload, SizePx, StrokePayload, StrokePoint, StrokeStyle,
  StyleChange, TextAlign, TextPayload, TextStyle, arrow_label_layout,
  arrow_minimum_length_for_label, choose_rectangle_label_anchor, layout_text,
  minimum_geometry_extent, rectangle_label_layout, snap_rectangle_label_layout,
  solve_rectangle_label_reflow,
};
use eframe::egui::{
  self, Align2, Color32, CursorIcon, Event, FocusDirection, FontId, Id, ImeEvent, Key,
  KeyboardShortcut, Modifiers, Pos2, Rect, Response, Sense, Stroke, StrokeKind, TextureHandle,
  TouchDeviceId, TouchId, TouchPhase,
};
use serde::{Deserialize, Serialize};

use crate::renderer::{
  layout_egui_text_with_document_wrapping, measured_arrow_label_bounds,
  measured_rectangle_label_bounds, paint_arrow_without_label_text, paint_document, paint_element,
  paint_raw_stroke_points, paint_rectangle_without_label_text,
};

const DEFAULT_RECTANGLE_LABEL: &str = "标题";
const EMPTY_LABEL_DRAFT: &str = "\u{200b}";
const ARROW_LABEL_TOO_SHORT_TOAST: &str = "箭头过短无法插入文字";
const COLOR_SWATCH_FONT_SIZE_PT: f32 = 20.0;
const FLOATING_CONTROL_HEIGHT_PT: f32 = 32.0;
const FLOATING_MENU_MAX_HEIGHT_PT: f32 = f32::INFINITY;
const FLOATING_PANEL_ORDER: egui::Order = egui::Order::Middle;
const FLOATING_PANEL_MARGIN_PT: i8 = 4;
const TOOLBAR_DRAG_HANDLE_WIDTH_PT: f32 = 18.0;
const TOOLBAR_DRAG_HANDLE_DOT_RADIUS_PT: f32 = 1.5;
const HANDLE_VISUAL_RADIUS_PT: f32 = 3.0;
const HANDLE_HIT_RADIUS_PT: f32 = 11.0;
const TEXT_MOVE_HANDLE_VISUAL_RADIUS_PT: f32 = 3.0;
const TEXT_MOVE_HANDLE_HIT_RADIUS_PT: f32 = 9.0;
const TEXT_MOVE_HANDLE_OFFSET_PT: f32 = 4.0;
const HIT_TOLERANCE_PT: f32 = 7.0;
const MINIMUM_DISTANCE_ROUNDING_FACTOR: f32 = 4.0;
const RELEASE_TAPER_WIDTH_FACTOR: f32 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorTool {
  Select,
  Rectangle,
  Arrow,
  Text,
  Stroke,
  Sequence,
}

impl EditorTool {
  pub const ALL: [Self; 6] =
    [Self::Select, Self::Rectangle, Self::Arrow, Self::Text, Self::Stroke, Self::Sequence];

  pub fn label(self) -> &'static str {
    match self {
      Self::Select => "选择",
      Self::Rectangle => "方框",
      Self::Arrow => "箭头",
      Self::Text => "文字",
      Self::Stroke => "画笔",
      Self::Sequence => "序号",
    }
  }

  fn tooltip(self) -> &'static str {
    match self {
      Self::Select => "选择工具 (1)",
      Self::Rectangle => "方框工具 (2)",
      Self::Arrow => "箭头工具 (3)",
      Self::Text => "文字工具 (4)",
      Self::Stroke => "画笔工具 (5)",
      Self::Sequence => "序号工具 (6)",
    }
  }

  fn index(self) -> usize {
    match self {
      Self::Select => 0,
      Self::Rectangle => 1,
      Self::Arrow => 2,
      Self::Text => 3,
      Self::Stroke => 4,
      Self::Sequence => 5,
    }
  }

  fn cursor(self) -> CursorIcon {
    match self {
      Self::Select => CursorIcon::Default,
      Self::Text => CursorIcon::Text,
      _ => CursorIcon::Crosshair,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolStyle {
  pub color_rgba: ColorRgba,
  pub width_px: f32,
  pub font_size_px: f32,
  pub hardness: f32,
}

impl Default for ToolStyle {
  fn default() -> Self {
    Self { color_rgba: ColorRgba::RED, width_px: 8.0, font_size_px: 24.0, hardness: 1.0 }
  }
}

impl ToolStyle {
  pub const fn new(color_rgba: ColorRgba, width_px: f32, font_size_px: f32, hardness: f32) -> Self {
    Self { color_rgba, width_px, font_size_px, hardness }
  }
}

pub fn default_tool_styles() -> [ToolStyle; 6] {
  [
    ToolStyle::default(),
    ToolStyle::new(ColorRgba::RED, 8.0, 36.0, 1.0),
    ToolStyle::new(ColorRgba::RED, 8.0, 24.0, 1.0),
    ToolStyle::new(ColorRgba::RED, 8.0, 36.0, 1.0),
    ToolStyle::new(ColorRgba::RED, 8.0, 24.0, 0.0),
    ToolStyle::new(ColorRgba::RED, 8.0, 24.0, 1.0),
  ]
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditorAction {
  Command(CommandBatch),
  Toast(String),
  GlobalColorChanged { color_rgba: ColorRgba },
  Undo,
  Redo,
  Save,
  Close,
  Copy,
  Paste { position: PointPx },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasTransform {
  document_size: SizePx,
  canvas_rect: Rect,
  scale: f32,
}

impl CanvasTransform {
  pub fn fit(document_size: SizePx, available_rect: Rect) -> Option<Self> {
    if document_size.width_px == 0
      || document_size.height_px == 0
      || !available_rect.is_finite()
      || available_rect.width() <= 0.0
      || available_rect.height() <= 0.0
    {
      return None;
    }
    let scale = (available_rect.width() / document_size.width_px as f32)
      .min(available_rect.height() / document_size.height_px as f32);
    if !scale.is_finite() || scale <= 0.0 {
      return None;
    }
    let canvas_size =
      egui::vec2(document_size.width_px as f32 * scale, document_size.height_px as f32 * scale);
    Some(Self {
      document_size,
      canvas_rect: Rect::from_center_size(available_rect.center(), canvas_size),
      scale,
    })
  }

  pub fn document_size(self) -> SizePx {
    self.document_size
  }

  pub fn canvas_rect(self) -> Rect {
    self.canvas_rect
  }

  pub fn scale(self) -> f32 {
    self.scale
  }

  pub fn document_to_egui(self, point: PointPx) -> Pos2 {
    self.canvas_rect.min + egui::vec2(point.x_px * self.scale, point.y_px * self.scale)
  }

  pub fn egui_to_document(self, position: Pos2) -> Option<PointPx> {
    if !self.canvas_rect.contains(position) {
      return None;
    }
    Some(self.egui_to_document_clamped(position))
  }

  fn egui_to_document_clamped(self, position: Pos2) -> PointPx {
    PointPx::new(
      ((position.x - self.canvas_rect.min.x) / self.scale)
        .clamp(0.0, self.document_size.width_px as f32),
      ((position.y - self.canvas_rect.min.y) / self.scale)
        .clamp(0.0, self.document_size.height_px as f32),
    )
  }

  pub fn document_rect_to_egui(self, rect: RectPx) -> Rect {
    Rect::from_min_max(self.document_to_egui(rect.min), self.document_to_egui(rect.max))
  }
}

#[derive(Debug, Clone)]
enum PointerInteraction {
  Draw {
    element_id: ElementId,
    tool: EditorTool,
    start: PointPx,
    current: PointPx,
    stroke_points: Vec<StrokePoint>,
  },
  Move {
    element_id: ElementId,
    start: PointPx,
    current: PointPx,
  },
  ResizeRectangle {
    element_id: ElementId,
    handle: RectangleHandle,
    original: RectPx,
    current: PointPx,
  },
  UpdateArrowEndpoint {
    element_id: ElementId,
    endpoint: ArrowEndpoint,
    current: PointPx,
  },
  DragRectangleLabel {
    element_id: ElementId,
    current: PointPx,
    grab_offset_px: PointPx,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StylusId {
  device_id: TouchDeviceId,
  touch_id: TouchId,
}

#[derive(Debug, Clone, Copy)]
struct StylusEvent {
  id: StylusId,
  phase: TouchPhase,
  position: Pos2,
  pressure: f32,
}

#[derive(Debug, Clone, Copy)]
struct StylusSample {
  phase: TouchPhase,
  point: PointPx,
  pressure: f32,
}

#[derive(Default)]
struct StylusFrame {
  samples: Vec<StylusSample>,
  ended: bool,
  cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RectangleHandle {
  TopLeft,
  Top,
  TopRight,
  Right,
  BottomRight,
  Bottom,
  BottomLeft,
  Left,
}

#[derive(Debug, Clone)]
enum TextTarget {
  NewText { anchor_px: PointPx },
  ExistingText { element_id: ElementId },
  ArrowLabel { element_id: ElementId },
  RectangleLabel { element_id: ElementId },
}

#[derive(Debug, Clone)]
struct TextEditing {
  target: TextTarget,
  buffer: String,
  text_style: TextStyle,
  ime: InlineImeState,
  request_focus: bool,
  select_all: bool,
  auto_place_rectangle: bool,
}

#[derive(Debug, Clone, Default)]
struct InlineImeState {
  preedit: Option<String>,
  confirming_preedit_with_enter: bool,
}

#[derive(Debug, Clone, Copy)]
struct InlineTextGeometry {
  origin_px: PointPx,
  wrap_width_px: f32,
  editor_width_px: f32,
}

#[derive(Debug, Clone, Default)]
struct ElementPreviewSet {
  elements: Vec<Element>,
}

impl ElementPreviewSet {
  fn single(element: Element) -> Self {
    Self { elements: vec![element] }
  }

  fn is_empty(&self) -> bool {
    self.elements.is_empty()
  }

  fn get(&self, element_id: ElementId) -> Option<&Element> {
    self.elements.iter().find(|element| element.element_id == element_id)
  }

  fn iter(&self) -> impl Iterator<Item = &Element> {
    self.elements.iter()
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShortcutAction {
  Tool(EditorTool),
  Undo,
  Redo,
  Save,
  Deselect,
  CloseOrCancel,
  Delete,
  Copy,
  Paste,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolSwitchDirection {
  Next,
  Previous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextEditorCompletion {
  None,
  FocusLost,
  CanvasPress,
  Enter,
}

#[derive(Debug, Clone)]
pub struct EditorController {
  tool: EditorTool,
  styles: [ToolStyle; 6],
  global_color: ColorRgba,
  selected_element_id: Option<ElementId>,
  interaction: Option<PointerInteraction>,
  released_preview_elements: ElementPreviewSet,
  text_editing: Option<TextEditing>,
  last_pointer_document: Option<PointPx>,
  option_panel_anchor: Option<Pos2>,
  toolbar_screen_rect: Option<Rect>,
  toolbar_panel_rect: Option<Rect>,
  tool_button_rects: [Option<Rect>; 6],
  active_tool_button_press_started: bool,
  toolbar_was_moved: bool,
  queued_stylus_events: Vec<StylusEvent>,
  active_stylus_id: Option<StylusId>,
  pending_stroke_points: Vec<StrokePoint>,
  queued_tool_switch: Option<ToolSwitchDirection>,
  tab_order: Vec<EditorTool>,
}

impl Default for EditorController {
  fn default() -> Self {
    Self::new(None)
  }
}

impl EditorController {
  pub fn new(restored_tool: impl Into<Option<EditorTool>>) -> Self {
    Self::with_styles(restored_tool, default_tool_styles(), ColorRgba::RED)
  }

  pub fn with_styles(
    restored_tool: impl Into<Option<EditorTool>>,
    styles: [ToolStyle; 6],
    global_color: ColorRgba,
  ) -> Self {
    Self {
      tool: restored_tool.into().unwrap_or(EditorTool::Rectangle),
      styles,
      global_color,
      selected_element_id: None,
      interaction: None,
      released_preview_elements: ElementPreviewSet::default(),
      text_editing: None,
      last_pointer_document: None,
      option_panel_anchor: None,
      toolbar_screen_rect: None,
      toolbar_panel_rect: None,
      tool_button_rects: [None; 6],
      active_tool_button_press_started: false,
      toolbar_was_moved: false,
      queued_stylus_events: Vec::new(),
      active_stylus_id: None,
      pending_stroke_points: Vec::new(),
      queued_tool_switch: None,
      tab_order: vec![EditorTool::Rectangle, EditorTool::Arrow],
    }
  }

  pub fn active_tool(&self) -> EditorTool {
    self.tool
  }

  pub fn set_tab_order(&mut self, tab_order: Vec<EditorTool>) {
    self.tab_order = tab_order;
  }

  pub fn set_active_tool(&mut self, tool: EditorTool) {
    if self.tool == tool {
      return;
    }
    self.tool = tool;
    self.selected_element_id = None;
    self.interaction = None;
    self.released_preview_elements = ElementPreviewSet::default();
    self.active_tool_button_press_started = false;
    self.reset_stylus_input();
  }

  pub(crate) fn capture_stylus_input(&mut self, raw_input: &mut egui::RawInput) {
    self.capture_stylus_events(&mut raw_input.events);
  }

  pub(crate) fn capture_stylus_input_state(&mut self, input: &mut egui::InputState) {
    self.capture_stylus_events(&mut input.events);
    input.raw.events.retain(|event| !is_stylus_event(event));
  }

  pub(crate) fn capture_tab_switch_input_state(&mut self, ctx: &egui::Context) {
    let direction = ctx.input_mut(|input| {
      let mut direction = None;
      input.events.retain(|event| {
        let Event::Key { key: Key::Tab, pressed: true, modifiers, .. } = event else {
          return true;
        };
        let switch_direction = if modifiers.is_none() {
          Some(ToolSwitchDirection::Next)
        } else if modifiers.shift_only() {
          Some(ToolSwitchDirection::Previous)
        } else {
          return true;
        };
        direction = switch_direction;
        false
      });
      input.raw.events.retain(|event| {
        !matches!(
          event,
          Event::Key { key: Key::Tab, pressed: true, modifiers, .. }
            if modifiers.is_none() || modifiers.shift_only()
        )
      });
      direction
    });
    if let Some(direction) = direction {
      self.queued_tool_switch = Some(direction);
      ctx.memory_mut(|memory| memory.move_focus(FocusDirection::None));
    }
  }

  fn capture_stylus_events(&mut self, events: &mut Vec<Event>) {
    if events.iter().any(|event| matches!(event, Event::WindowFocused(false))) {
      self.interaction = None;
      self.reset_stylus_input();
      events.retain(|event| !is_stylus_event(event));
      return;
    }
    events.retain(|event| {
      let Some(stylus_event) = stylus_event(event) else {
        return true;
      };
      self.queued_stylus_events.push(stylus_event);
      false
    });
  }

  pub fn selected_element_id(&self) -> Option<ElementId> {
    self.selected_element_id
  }

  pub fn set_selected_element_id(&mut self, element_id: Option<ElementId>) {
    self.selected_element_id = element_id;
  }

  pub fn tool_style(&self, tool: EditorTool) -> ToolStyle {
    ToolStyle { color_rgba: self.global_color, ..self.styles[tool.index()] }
  }

  pub fn global_color(&self) -> ColorRgba {
    self.global_color
  }

  pub fn show(
    &mut self,
    root_ui: &mut egui::Ui,
    document: &BoardDocument,
    history: &CommandHistory,
    background: Option<&TextureHandle>,
  ) -> Vec<EditorAction> {
    let ctx = root_ui.ctx().clone();
    self.released_preview_elements = ElementPreviewSet::default();
    if self.selected_element_id.is_some_and(|id| document.element(id).is_none()) {
      self.selected_element_id = None;
    }

    let mut actions = Vec::new();

    let mut transform = None;
    let mut canvas_response = None;
    let mut canvas_painter = None;
    let background_fill = if background.is_some() { Color32::BLACK } else { Color32::TRANSPARENT };
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(background_fill)).show(
      root_ui,
      |ui| {
        let Some(canvas_transform) = CanvasTransform::fit(document.canvas_size_px, ui.max_rect())
        else {
          return;
        };
        transform = Some(canvas_transform);
        let response = ui
          .interact(
            canvas_transform.canvas_rect(),
            Id::new("rs-board-canvas"),
            Sense::click_and_drag(),
          )
          .on_hover_cursor(self.tool.cursor());

        if let Some(position) = response.hover_pos()
          && let Some(document_position) = canvas_transform.egui_to_document(position)
        {
          self.last_pointer_document = Some(document_position);
        }
        canvas_response = Some(response);

        let painter = ui.painter().with_clip_rect(canvas_transform.canvas_rect());
        if let Some(background) = background {
          painter.image(
            background.id(),
            canvas_transform.canvas_rect(),
            Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
          );
        }
        canvas_painter = Some(painter);
      },
    );

    if let Some(transform) = transform {
      let editing_transform_pointer = canvas_response.as_ref().is_some_and(|response| {
        self.text_editing_transform_pointer_active(response, transform, document)
      });
      let preserve_text_focus = self.active_tool_button_clicked(&ctx) || editing_transform_pointer;
      let mut text_completion =
        self.show_text_editor(&ctx, transform, document, preserve_text_focus);
      if text_completion == TextEditorCompletion::None
        && self.text_editing.is_some()
        && !editing_transform_pointer
        && canvas_response.as_ref().is_some_and(|response| self.canvas_primary_pressed(response))
      {
        ctx.memory_mut(|memory| memory.surrender_focus(inline_text_editor_id()));
        text_completion = TextEditorCompletion::CanvasPress;
      }
      if text_completion != TextEditorCompletion::None {
        self.commit_text(document, &mut actions);
        if text_completion == TextEditorCompletion::Enter {
          self.selected_element_id = None;
        }
      }
      let handle_editing_transform = self.text_editing.is_some()
        && (editing_transform_pointer || self.text_editing_transform_interaction_active());
      if let Some(response) = canvas_response.as_ref()
        && (self.text_editing.is_none() || handle_editing_transform)
      {
        self.handle_pointer(response, transform, document, &mut actions, handle_editing_transform);
      }
      if let Some(painter) = canvas_painter {
        self.paint_document_for_editing(&painter, transform, document);
        self.paint_interaction(&painter, transform, document);
        if self.text_editing.is_some() {
          self.paint_text_editing_controls(&painter, transform, document);
        } else {
          self.paint_selection(&painter, transform, document);
        }
      }
      self.released_preview_elements = ElementPreviewSet::default();
      self.handle_keyboard(&ctx, document, &mut actions);
      self.show_option_panel(&ctx, transform, document, &mut actions);
    } else {
      self.handle_keyboard(&ctx, document, &mut actions);
    }
    self.show_toolbar(&ctx, document, history, &mut actions);
    actions
  }

  pub fn show_read_only(
    &mut self,
    root_ui: &mut egui::Ui,
    document: &BoardDocument,
    background: Option<&TextureHandle>,
  ) {
    let background_fill = if background.is_some() { Color32::BLACK } else { Color32::TRANSPARENT };
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(background_fill)).show(
      root_ui,
      |ui| {
        let Some(canvas_transform) = CanvasTransform::fit(document.canvas_size_px, ui.max_rect())
        else {
          return;
        };
        let painter = ui.painter().with_clip_rect(canvas_transform.canvas_rect());
        if let Some(background) = background {
          painter.image(
            background.id(),
            canvas_transform.canvas_rect(),
            Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
          );
        }
        paint_document(&painter, &canvas_transform, document);
        self.paint_selection(&painter, canvas_transform, document);
      },
    );
  }

  pub fn update(
    &mut self,
    ui: &mut egui::Ui,
    document: &BoardDocument,
    history: &CommandHistory,
    background: Option<&TextureHandle>,
  ) -> Vec<EditorAction> {
    self.show(ui, document, history, background)
  }

  pub(crate) fn commit_pending_text(&mut self, document: &BoardDocument) -> Vec<EditorAction> {
    let mut actions = Vec::new();
    self.commit_text(document, &mut actions);
    actions
  }

  fn handle_keyboard(
    &mut self,
    ctx: &egui::Context,
    document: &BoardDocument,
    actions: &mut Vec<EditorAction>,
  ) {
    if let Some(direction) = self.queued_tool_switch.take() {
      let tool = match direction {
        ToolSwitchDirection::Next => self.tab_order_next(),
        ToolSwitchDirection::Previous => self.tab_order_previous(),
      };
      if let Some(tool) = tool {
        self.switch_tool(tool, document, actions);
      }
    }
    let text_editing = self.text_editing.is_some();
    let shortcut = ctx.input_mut(|input| {
      let position = input.events.iter().position(|event| {
        let Event::Key { key, pressed: true, modifiers, .. } = event else {
          return false;
        };
        map_shortcut(*key, *modifiers, text_editing).is_some()
      });
      position.and_then(|position| {
        let Event::Key { key, modifiers, .. } = input.events.remove(position) else {
          unreachable!();
        };
        map_shortcut(key, modifiers, text_editing)
      })
    });
    let Some(shortcut) = shortcut else {
      return;
    };
    match shortcut {
      ShortcutAction::Tool(tool) => {
        self.switch_tool(tool, document, actions);
      }
      ShortcutAction::Undo => {
        self.interaction = None;
        self.reset_stylus_input();
        self.selected_element_id = None;
        actions.push(EditorAction::Undo);
      }
      ShortcutAction::Redo => {
        self.interaction = None;
        self.reset_stylus_input();
        self.selected_element_id = None;
        actions.push(EditorAction::Redo);
      }
      ShortcutAction::Save => {
        if text_editing {
          self.commit_text(document, actions);
        }
        actions.push(EditorAction::Save);
      }
      ShortcutAction::Deselect => {
        self.selected_element_id = None;
      }
      ShortcutAction::CloseOrCancel => {
        let cancelled_text = self.text_editing.take().is_some();
        let cancelled_interaction = self.interaction.take().is_some();
        if cancelled_text || cancelled_interaction {
          self.reset_stylus_input();
          return;
        }
        actions.push(EditorAction::Close);
      }
      ShortcutAction::Delete => {
        if let Some(element_id) = self.selected_element_id.take() {
          actions.push(rectangle_command_action(
            document,
            DocumentCommand::DeleteElement { element_id },
            element_id,
          ));
        }
      }
      ShortcutAction::Copy => {
        if self.selected_element_id.is_some() {
          actions.push(EditorAction::Copy);
        }
      }
      ShortcutAction::Paste => {
        let position =
          self.last_pointer_document.unwrap_or_else(|| document.canvas_size_px.bounds().center());
        actions.push(EditorAction::Paste { position });
      }
    }
  }

  fn tab_order_next(&self) -> Option<EditorTool> {
    match self.tab_order.iter().position(|tool| *tool == self.tool) {
      Some(index) => Some(self.tab_order[(index + 1) % self.tab_order.len()]),
      None => self.tab_order.first().copied(),
    }
  }

  fn tab_order_previous(&self) -> Option<EditorTool> {
    match self.tab_order.iter().position(|tool| *tool == self.tool) {
      Some(index) => {
        Some(self.tab_order[(index + self.tab_order.len() - 1) % self.tab_order.len()])
      }
      None => self.tab_order.last().copied(),
    }
  }

  fn switch_tool(
    &mut self,
    tool: EditorTool,
    document: &BoardDocument,
    actions: &mut Vec<EditorAction>,
  ) {
    if self.tool == tool {
      return;
    }
    self.commit_text(document, actions);
    self.set_active_tool(tool);
  }

  fn handle_pointer(
    &mut self,
    response: &Response,
    transform: CanvasTransform,
    document: &BoardDocument,
    actions: &mut Vec<EditorAction>,
    editing_transform_only: bool,
  ) {
    let pointer_position =
      response.interact_pointer_pos().and_then(|p| transform.egui_to_document(p));
    let was_drawing_stroke =
      matches!(self.interaction, Some(PointerInteraction::Draw { tool: EditorTool::Stroke, .. }));
    let stylus_frame = if self.tool == EditorTool::Stroke || was_drawing_stroke {
      self.take_stylus_frame(transform)
    } else {
      self.reset_stylus_input();
      StylusFrame::default()
    };

    if was_drawing_stroke {
      if let Some(PointerInteraction::Draw { current, stroke_points, .. }) = &mut self.interaction {
        for sample in &stylus_frame.samples {
          append_stylus_sample(stroke_points, *sample);
          *current = sample.point;
        }
        if stylus_frame.ended {
          apply_release_taper(stroke_points, self.styles[EditorTool::Stroke.index()].width_px);
        }
      }
    } else if self.tool == EditorTool::Stroke {
      for sample in &stylus_frame.samples {
        append_stylus_sample(&mut self.pending_stroke_points, *sample);
      }
      if stylus_frame.ended {
        apply_release_taper(
          &mut self.pending_stroke_points,
          self.styles[EditorTool::Stroke.index()].width_px,
        );
      }
    }

    if stylus_frame.cancelled {
      if was_drawing_stroke {
        self.interaction = None;
      }
      self.pending_stroke_points.clear();
      return;
    }

    if !editing_transform_only
      && response.double_clicked()
      && self.tool != EditorTool::Stroke
      && let Some(position) = pointer_position
      && let Some(element_id) = hit_test_document_for_tool(
        document,
        position,
        HIT_TOLERANCE_PT / transform.scale(),
        self.tool,
      )
    {
      self.selected_element_id = Some(element_id);
      if double_click_starts_editing(document, element_id, position) {
        self.start_editing_existing(document, element_id, true, actions);
      }
      return;
    }

    if !editing_transform_only
      && response.clicked()
      && let Some(position) = pointer_position
    {
      let matching_element = hit_test_document_for_tool(
        document,
        position,
        HIT_TOLERANCE_PT / transform.scale(),
        self.tool,
      );
      if let Some(element_id) = matching_element {
        self.selected_element_id = Some(element_id);
        if single_click_starts_editing(document, element_id, position) {
          self.start_editing_existing(document, element_id, false, actions);
        }
      } else {
        self.selected_element_id = None;
        match self.tool {
          EditorTool::Select => {}
          EditorTool::Text => {
            let style = self.tool_style(EditorTool::Text);
            self.text_editing = Some(TextEditing {
              target: TextTarget::NewText { anchor_px: position },
              buffer: String::new(),
              text_style: TextStyle::mvp(style.color_rgba, style.font_size_px)
                .expect("text tool style is valid"),
              ime: InlineImeState::default(),
              request_focus: true,
              select_all: false,
              auto_place_rectangle: false,
            });
          }
          EditorTool::Sequence => self.insert_sequence(document, position, actions),
          EditorTool::Stroke => {
            let pressure = self
              .pending_stroke_points
              .iter()
              .map(|point| point.pressure)
              .reduce(f32::max)
              .unwrap_or(1.0);
            let point = StrokePoint::with_pressure(position, pressure)
              .expect("captured stroke pressure is normalized");
            if let Some(element) = self.make_stroke_points(document, &[point]) {
              let element_id = element.element_id;
              self.push_pointer_command(
                document,
                actions,
                DocumentCommand::AddElement { element },
                element_id,
              );
            }
            self.pending_stroke_points.clear();
          }
          EditorTool::Rectangle | EditorTool::Arrow => {}
        }
      }
    }

    if response.drag_started() {
      let start = response
        .ctx
        .input(|input| input.pointer.press_origin())
        .and_then(|position| transform.egui_to_document(position))
        .or(pointer_position);
      if let Some(start) = start {
        let selected_existing = if editing_transform_only {
          self.begin_text_editing_transform_drag(start, transform, document)
        } else {
          self.tool != EditorTool::Stroke && self.begin_selection_drag(start, transform, document)
        };
        if !selected_existing
          && !editing_transform_only
          && matches!(self.tool, EditorTool::Rectangle | EditorTool::Arrow | EditorTool::Stroke)
        {
          self.selected_element_id = None;
          let stroke_points = if self.tool == EditorTool::Stroke {
            let mut points = std::mem::take(&mut self.pending_stroke_points);
            if points.first().is_none_or(|point| point.point() != start) {
              let pressure = points.first().map_or(1.0, |point| point.pressure);
              points.insert(
                0,
                StrokePoint::with_pressure(start, pressure)
                  .expect("captured stroke pressure is normalized"),
              );
            }
            points
          } else {
            Vec::new()
          };
          self.interaction = Some(PointerInteraction::Draw {
            element_id: ElementId::new(),
            tool: self.tool,
            start,
            current: pointer_position.unwrap_or(start),
            stroke_points,
          });
        }
      }
    }

    if response.dragged()
      && let Some(position) = pointer_position
    {
      match &mut self.interaction {
        Some(PointerInteraction::Draw { current, stroke_points, tool, .. }) => {
          if *tool == EditorTool::Text
            && let Some(TextEditing { target: TextTarget::NewText { anchor_px }, .. }) =
              self.text_editing.as_mut()
          {
            *anchor_px = *anchor_px + (position - *current);
          }
          *current = position;
          if *tool == EditorTool::Stroke
            && stylus_frame.samples.is_empty()
            && self.active_stylus_id.is_none()
          {
            append_stroke_point(stroke_points, StrokePoint::new(position));
          }
        }
        Some(PointerInteraction::Move { current, .. })
        | Some(PointerInteraction::ResizeRectangle { current, .. })
        | Some(PointerInteraction::UpdateArrowEndpoint { current, .. })
        | Some(PointerInteraction::DragRectangleLabel { current, .. }) => *current = position,
        None => {}
      }
    }

    if response.drag_stopped() {
      self.finish_pointer_interaction(document, actions);
      self.pending_stroke_points.clear();
    } else if stylus_frame.ended && !response.clicked() {
      self.pending_stroke_points.clear();
    }
  }

  fn take_stylus_frame(&mut self, transform: CanvasTransform) -> StylusFrame {
    let mut frame = StylusFrame::default();
    let mut last_point = match &self.interaction {
      Some(PointerInteraction::Draw { tool: EditorTool::Stroke, stroke_points, .. }) => {
        stroke_points.last().map(StrokePoint::point)
      }
      _ => self.pending_stroke_points.last().map(StrokePoint::point),
    };
    for event in std::mem::take(&mut self.queued_stylus_events) {
      let point = transform.egui_to_document(event.position);
      if self.active_stylus_id.is_none()
        && (event.phase == TouchPhase::Start
          || (event.phase == TouchPhase::Move && event.pressure > 0.0))
        && point.is_some()
      {
        self.active_stylus_id = Some(event.id);
      }
      if self.active_stylus_id != Some(event.id) {
        continue;
      }

      match event.phase {
        TouchPhase::End => {
          if let Some(point) = point.or(last_point) {
            frame.samples.push(StylusSample {
              phase: event.phase,
              point,
              pressure: event.pressure,
            });
          }
          frame.ended = true;
          self.active_stylus_id = None;
        }
        TouchPhase::Cancel => {
          frame.cancelled = true;
          self.active_stylus_id = None;
        }
        TouchPhase::Start | TouchPhase::Move => {
          if let Some(point) = point {
            frame.samples.push(StylusSample {
              phase: event.phase,
              point,
              pressure: event.pressure,
            });
            last_point = Some(point);
          }
        }
      }
    }
    frame
  }

  fn reset_stylus_input(&mut self) {
    self.queued_stylus_events.clear();
    self.active_stylus_id = None;
    self.pending_stroke_points.clear();
  }

  fn text_editing_transform_pointer_active(
    &self,
    response: &Response,
    transform: CanvasTransform,
    document: &BoardDocument,
  ) -> bool {
    if self.text_editing_transform_interaction_active() {
      return true;
    }
    let Some(press_origin) = response
      .ctx
      .input(|input| input.pointer.primary_down().then(|| input.pointer.press_origin()).flatten())
    else {
      return false;
    };
    if self.toolbar_panel_rect.is_some_and(|rect| rect.contains(press_origin))
      || !response.rect.contains(press_origin)
    {
      return false;
    }
    let Some(position) = transform.egui_to_document(press_origin) else {
      return false;
    };
    let inside_editor = response
      .ctx
      .read_response(inline_text_editor_id())
      .is_some_and(|editor| editor.rect.contains(press_origin));
    self.text_editing_transform_hit(position, transform, document, inside_editor)
  }

  fn text_editing_transform_interaction_active(&self) -> bool {
    let Some(editing) = &self.text_editing else {
      return false;
    };
    match (&editing.target, &self.interaction) {
      (
        TextTarget::NewText { .. },
        Some(PointerInteraction::Draw { tool: EditorTool::Text, .. }),
      ) => true,
      (target, Some(interaction)) => {
        text_target_element_id(target).is_some_and(|target_id| match interaction {
          PointerInteraction::Move { element_id, .. }
          | PointerInteraction::ResizeRectangle { element_id, .. }
          | PointerInteraction::UpdateArrowEndpoint { element_id, .. }
          | PointerInteraction::DragRectangleLabel { element_id, .. } => *element_id == target_id,
          PointerInteraction::Draw { .. } => false,
        })
      }
      _ => false,
    }
  }

  fn text_editing_transform_hit(
    &self,
    position: PointPx,
    transform: CanvasTransform,
    document: &BoardDocument,
    inside_editor: bool,
  ) -> bool {
    let Some(editing) = &self.text_editing else {
      return false;
    };
    let tolerance = HIT_TOLERANCE_PT / transform.scale();
    match editing.target {
      TextTarget::RectangleLabel { element_id } => {
        let Some(element) = document.element(element_id) else {
          return false;
        };
        let ElementPayload::Rectangle(payload) = &element.payload else {
          return false;
        };
        hit_rectangle_handle(element, position, transform).is_some()
          || (!inside_editor
            && contains(
              RectPx::from_points(payload.start_px, payload.end_px).expanded(tolerance),
              position,
            ))
      }
      TextTarget::ArrowLabel { element_id } => {
        let Some(element) = document.element(element_id) else {
          return false;
        };
        let ElementPayload::Arrow(payload) = &element.payload else {
          return false;
        };
        hit_arrow_handle(element, position, transform).is_some()
          || (!inside_editor && hit_arrow_body(payload, position, tolerance))
      }
      TextTarget::ExistingText { .. } | TextTarget::NewText { .. } => self
        .text_editing_bounds_px(document)
        .is_some_and(|bounds| hit_text_editing_frame(bounds, position, transform)),
    }
  }

  fn begin_text_editing_transform_drag(
    &mut self,
    position: PointPx,
    transform: CanvasTransform,
    document: &BoardDocument,
  ) -> bool {
    let Some(target) = self.text_editing.as_ref().map(|editing| editing.target.clone()) else {
      return false;
    };
    match target {
      TextTarget::RectangleLabel { element_id } => {
        let Some(element) = document.element(element_id) else {
          return false;
        };
        let ElementPayload::Rectangle(payload) = &element.payload else {
          return false;
        };
        self.selected_element_id = Some(element_id);
        if let Some(handle) = hit_rectangle_handle(element, position, transform) {
          self.interaction = Some(PointerInteraction::ResizeRectangle {
            element_id,
            handle,
            original: RectPx::from_points(payload.start_px, payload.end_px),
            current: position,
          });
        } else {
          self.interaction =
            Some(PointerInteraction::Move { element_id, start: position, current: position });
        }
        true
      }
      TextTarget::ArrowLabel { element_id } => {
        let Some(element) = document.element(element_id) else {
          return false;
        };
        self.selected_element_id = Some(element_id);
        if let Some(endpoint) = hit_arrow_handle(element, position, transform) {
          self.interaction = Some(PointerInteraction::UpdateArrowEndpoint {
            element_id,
            endpoint,
            current: position,
          });
        } else {
          self.interaction =
            Some(PointerInteraction::Move { element_id, start: position, current: position });
        }
        true
      }
      TextTarget::ExistingText { element_id } => {
        self.selected_element_id = Some(element_id);
        self.interaction =
          Some(PointerInteraction::Move { element_id, start: position, current: position });
        true
      }
      TextTarget::NewText { .. } => {
        self.interaction = Some(PointerInteraction::Draw {
          element_id: ElementId::new(),
          tool: EditorTool::Text,
          start: position,
          current: position,
          stroke_points: Vec::new(),
        });
        true
      }
    }
  }

  fn text_editing_preview_element(&self, document: &BoardDocument) -> Option<Element> {
    let target_id =
      self.text_editing.as_ref().and_then(|editing| text_target_element_id(&editing.target))?;
    self
      .interaction
      .as_ref()
      .and_then(|interaction| {
        interaction_preview_set(self, interaction, document).get(target_id).cloned()
      })
      .or_else(|| self.released_preview_elements.get(target_id).cloned())
  }

  fn text_editing_bounds_px(&self, document: &BoardDocument) -> Option<RectPx> {
    let editing = self.text_editing.as_ref()?;
    let preview = self.text_editing_preview_element(document);
    text_editing_bounds_px_with_preview(editing, document, preview.as_ref())
  }

  fn begin_selection_drag(
    &mut self,
    position: PointPx,
    transform: CanvasTransform,
    document: &BoardDocument,
  ) -> bool {
    if let Some(element_id) = self.selected_element_id
      && let Some(element) = document.element(element_id)
      && tool_selects_element(self.tool, element)
    {
      if let Some(handle) = hit_rectangle_handle(element, position, transform) {
        let ElementPayload::Rectangle(payload) = &element.payload else {
          unreachable!();
        };
        self.interaction = Some(PointerInteraction::ResizeRectangle {
          element_id,
          handle,
          original: RectPx::from_points(payload.start_px, payload.end_px),
          current: position,
        });
        return true;
      }
      if let Some(endpoint) = hit_arrow_handle(element, position, transform) {
        self.interaction =
          Some(PointerInteraction::UpdateArrowEndpoint { element_id, endpoint, current: position });
        return true;
      }
    }

    let Some(element_id) = hit_test_document_for_tool(
      document,
      position,
      HIT_TOLERANCE_PT / transform.scale(),
      self.tool,
    ) else {
      self.selected_element_id = None;
      return false;
    };
    self.selected_element_id = Some(element_id);
    if document
      .element(element_id)
      .and_then(|element| match &element.payload {
        ElementPayload::Rectangle(payload) => {
          rectangle_label_layout(payload, document.canvas_size_px).ok().flatten()
        }
        _ => None,
      })
      .is_some_and(|layout| contains(layout.bounds_px, position))
    {
      let grab_offset_px = document
        .element(element_id)
        .and_then(|element| match &element.payload {
          ElementPayload::Rectangle(payload) => {
            rectangle_label_layout(payload, document.canvas_size_px).ok().flatten()
          }
          _ => None,
        })
        .map_or(PointPx::ZERO, |layout| layout.bounds_px.center() - position);
      self.interaction = Some(PointerInteraction::DragRectangleLabel {
        element_id,
        current: position,
        grab_offset_px,
      });
    } else {
      self.interaction =
        Some(PointerInteraction::Move { element_id, start: position, current: position });
    }
    true
  }

  fn finish_pointer_interaction(
    &mut self,
    document: &BoardDocument,
    actions: &mut Vec<EditorAction>,
  ) {
    let Some(interaction) = self.interaction.take() else {
      return;
    };
    self.released_preview_elements = ElementPreviewSet::default();
    match interaction {
      PointerInteraction::Draw {
        element_id: draw_element_id,
        tool,
        start,
        current,
        mut stroke_points,
      } => match tool {
        EditorTool::Rectangle => {
          if let Some(element) = self.make_rectangle(document, draw_element_id, start, current) {
            let element_id = element.element_id;
            let ElementPayload::Rectangle(payload) = &element.payload else {
              unreachable!("rectangle tool created a non-rectangle element");
            };
            let text_style = payload.label.text_style.clone();
            self.push_pointer_command(
              document,
              actions,
              DocumentCommand::AddElement { element },
              element_id,
            );
            self.selected_element_id = Some(element_id);
            self.text_editing = Some(TextEditing {
              target: TextTarget::RectangleLabel { element_id },
              buffer: DEFAULT_RECTANGLE_LABEL.to_owned(),
              text_style,
              ime: InlineImeState::default(),
              request_focus: true,
              select_all: true,
              // The release-time reflow already chose the anchor for this new label.
              auto_place_rectangle: false,
            });
          }
        }
        EditorTool::Arrow => {
          if let Some(element) = self.make_arrow(document, start, current) {
            let element_id = element.element_id;
            let ElementPayload::Arrow(payload) = &element.payload else {
              unreachable!("arrow tool created a non-arrow element");
            };
            let can_edit_label = arrow_can_host_label(payload, document.canvas_size_px);
            let text_style = payload.label.text_style.clone();
            self.push_pointer_command(
              document,
              actions,
              DocumentCommand::AddElement { element },
              element_id,
            );
            self.selected_element_id = Some(element_id);
            if can_edit_label {
              self.text_editing = Some(TextEditing {
                target: TextTarget::ArrowLabel { element_id },
                buffer: String::new(),
                text_style,
                ime: InlineImeState::default(),
                request_focus: true,
                select_all: false,
                auto_place_rectangle: false,
              });
            } else {
              actions.push(EditorAction::Toast(ARROW_LABEL_TOO_SHORT_TOAST.to_owned()));
            }
          }
        }
        EditorTool::Stroke => {
          if stroke_points.last().is_none_or(|point| point.point() != current) {
            append_stroke_point(&mut stroke_points, StrokePoint::new(current));
          }
          if let Some(element) = self.make_stroke_points(document, &stroke_points) {
            let element_id = element.element_id;
            self.push_pointer_command(
              document,
              actions,
              DocumentCommand::AddElement { element },
              element_id,
            );
          }
        }
        EditorTool::Select | EditorTool::Text | EditorTool::Sequence => {}
      },
      PointerInteraction::Move { element_id, start, current } => {
        let delta_px = current - start;
        if delta_px.distance_to(PointPx::ZERO) > 0.01 {
          self.push_pointer_command(
            document,
            actions,
            DocumentCommand::MoveElement { element_id, delta_px },
            element_id,
          );
        }
      }
      PointerInteraction::ResizeRectangle { element_id, handle, original, current } => {
        if let Some(element) = document.element(element_id)
          && let ElementPayload::Rectangle(payload) = &element.payload
          && let Ok(minimum) = minimum_geometry_extent(payload.stroke_style.width_px)
        {
          let (start_px, end_px) = minimum_resized_rectangle(original, handle, current, minimum);
          self.push_pointer_command(
            document,
            actions,
            DocumentCommand::ResizeRectangle { element_id, start_px, end_px },
            element_id,
          );
        }
      }
      PointerInteraction::UpdateArrowEndpoint { element_id, endpoint, current } => {
        if let Some(element) = document.element(element_id)
          && let ElementPayload::Arrow(payload) = &element.payload
        {
          let (position_px, constrained_for_label) =
            clamped_arrow_endpoint(payload, endpoint, current);
          let original_position_px = match endpoint {
            ArrowEndpoint::Start => payload.start_px,
            ArrowEndpoint::End => payload.end_px,
          };
          if position_px.distance_to(original_position_px) > 0.01 {
            self.push_pointer_command(
              document,
              actions,
              DocumentCommand::UpdateArrowEndpoint { element_id, endpoint, position_px },
              element_id,
            );
          }
          if constrained_for_label {
            actions.push(EditorAction::Toast(ARROW_LABEL_TOO_SHORT_TOAST.to_owned()));
          }
        }
      }
      PointerInteraction::DragRectangleLabel { element_id, current, grab_offset_px } => {
        if let Some(element) = document.element(element_id)
          && let ElementPayload::Rectangle(payload) = &element.payload
          && let Ok(layout) =
            snap_rectangle_label_layout(payload, document.canvas_size_px, current + grab_offset_px)
          && (layout.anchor != payload.label_anchor
            || layout.anchor != payload.preferred_label_anchor)
        {
          let command = DocumentCommand::SetRectangleLabelPlacement {
            element_id,
            preferred_anchor: layout.anchor,
            actual_anchor: layout.anchor,
          };
          if let Some(batch) = rectangle_reflow_batch(document, vec![command], element_id, &[]) {
            self.push_pointer_batch(document, actions, batch, element_id);
          }
        }
      }
    }
  }

  fn push_pointer_batch(
    &mut self,
    document: &BoardDocument,
    actions: &mut Vec<EditorAction>,
    batch: CommandBatch,
    preview_element_id: ElementId,
  ) {
    let preview = preview_set_after_batch(document, &batch);
    if preview.is_empty() {
      self.released_preview_elements = ElementPreviewSet::default();
      return;
    }
    self.released_preview_elements = preview;
    if self.released_preview_elements.get(preview_element_id).is_none() {
      self.released_preview_elements = ElementPreviewSet::default();
    }
    actions.push(EditorAction::Command(batch));
  }

  fn push_pointer_command(
    &mut self,
    document: &BoardDocument,
    actions: &mut Vec<EditorAction>,
    command: DocumentCommand,
    preview_element_id: ElementId,
  ) {
    let batch = rectangle_reflow_batch(document, vec![command.clone()], preview_element_id, &[])
      .unwrap_or_else(|| CommandBatch::single(command));
    self.push_pointer_batch(document, actions, batch, preview_element_id);
  }

  fn make_rectangle(
    &self,
    document: &BoardDocument,
    element_id: ElementId,
    start_px: PointPx,
    end_px: PointPx,
  ) -> Option<Element> {
    let minimum = minimum_geometry_extent(self.tool_style(EditorTool::Rectangle).width_px).ok()?;
    let end_px = PointPx::new(
      coordinate_at_minimum(start_px.x_px, end_px.x_px, minimum, 1.0),
      coordinate_at_minimum(start_px.y_px, end_px.y_px, minimum, 1.0),
    );
    let payload = self.rectangle_payload(start_px, end_px)?;
    Element::new(
      element_id,
      document.elements.len() as i64,
      ElementPayload::Rectangle(payload),
      document.canvas_size_px,
    )
    .ok()
  }

  fn make_rectangle_preview(
    &self,
    document: &BoardDocument,
    element_id: ElementId,
    start_px: PointPx,
    end_px: PointPx,
  ) -> Option<Element> {
    let payload = self.rectangle_payload(start_px, end_px)?;
    let body = RectPx::from_points(start_px, end_px);
    let minimum = minimum_geometry_extent(payload.stroke_style.width_px).ok()?;
    let mut element = Element {
      element_id,
      z_index: document.elements.len() as i64,
      bounds_px: RectPx::from_min_max(PointPx::ZERO, PointPx::ZERO),
      payload: ElementPayload::Rectangle(payload),
    };
    element.refresh_bounds(document.canvas_size_px).ok()?;
    if body.width() >= minimum && body.height() >= minimum {
      element.constrain_to_canvas(document.canvas_size_px, true).ok()?;
    }
    Some(element)
  }

  fn rectangle_payload(&self, start_px: PointPx, end_px: PointPx) -> Option<RectanglePayload> {
    let style = self.tool_style(EditorTool::Rectangle);
    let stroke_style = StrokeStyle::mvp(style.color_rgba, style.width_px).ok()?;
    let text_style =
      TextStyle::mvp(style.color_rgba.contrasting_text(), style.font_size_px).ok()?;
    Some(RectanglePayload {
      start_px,
      end_px,
      stroke_style,
      fill_rgba: None,
      label: ElementLabel {
        text: Some(DEFAULT_RECTANGLE_LABEL.to_owned()),
        max_width_px: 840.0,
        padding_px: 8.0,
        anchor_offset_px: 8.0,
        text_style,
      },
      label_anchor: RectangleLabelAnchor::new(
        RectangleLabelEdge::Top,
        RectangleLabelSide::Outside,
        0.0,
      ),
      preferred_label_anchor: RectangleLabelAnchor::new(
        RectangleLabelEdge::Top,
        RectangleLabelSide::Outside,
        0.0,
      ),
    })
  }

  fn make_arrow(
    &self,
    document: &BoardDocument,
    start_px: PointPx,
    end_px: PointPx,
  ) -> Option<Element> {
    let style = self.tool_style(EditorTool::Arrow);
    let minimum = ArrowHead::for_stroke_width(style.width_px).ok()?.min_body_length_px;
    let end_px = point_at_minimum_distance(start_px, end_px, minimum, PointPx::new(1.0, 0.0));
    let payload = self.arrow_payload(start_px, end_px)?;
    Element::new(
      ElementId::new(),
      document.elements.len() as i64,
      ElementPayload::Arrow(payload),
      document.canvas_size_px,
    )
    .ok()
  }

  fn make_arrow_preview(
    &self,
    document: &BoardDocument,
    start_px: PointPx,
    end_px: PointPx,
  ) -> Option<Element> {
    let payload = self.arrow_payload(start_px, end_px)?;
    let meets_minimum = start_px.distance_to(end_px) >= payload.head.min_body_length_px;
    let mut element = Element {
      element_id: ElementId::new(),
      z_index: document.elements.len() as i64,
      bounds_px: RectPx::from_min_max(PointPx::ZERO, PointPx::ZERO),
      payload: ElementPayload::Arrow(payload),
    };
    element.refresh_bounds(document.canvas_size_px).ok()?;
    if meets_minimum {
      element.constrain_to_canvas(document.canvas_size_px, true).ok()?;
    }
    Some(element)
  }

  fn arrow_payload(&self, start_px: PointPx, end_px: PointPx) -> Option<ArrowPayload> {
    let style = self.tool_style(EditorTool::Arrow);
    let stroke_style = StrokeStyle::mvp(style.color_rgba, style.width_px).ok()?;
    let head = ArrowHead::for_stroke_width(style.width_px).ok()?;
    let text_style =
      TextStyle::mvp(style.color_rgba.contrasting_text(), style.font_size_px).ok()?;
    Some(ArrowPayload {
      start_px,
      end_px,
      stroke_style,
      head,
      label: ElementLabel {
        text: None,
        max_width_px: 420.0,
        padding_px: 8.0,
        anchor_offset_px: 8.0,
        text_style,
      },
    })
  }

  #[cfg(test)]
  fn make_stroke(&self, document: &BoardDocument, points: &[PointPx]) -> Option<Element> {
    let points = points.iter().copied().map(StrokePoint::new).collect::<Vec<_>>();
    self.make_stroke_points(document, &points)
  }

  fn make_stroke_points(
    &self,
    document: &BoardDocument,
    points: &[StrokePoint],
  ) -> Option<Element> {
    let style = self.tool_style(EditorTool::Stroke);
    let stroke_style = StrokeStyle::mvp(style.color_rgba, style.width_px).ok()?;
    let payload =
      StrokePayload::from_stroke_points_with_hardness(points, stroke_style, style.hardness).ok()?;
    Element::new(
      ElementId::new(),
      document.elements.len() as i64,
      ElementPayload::Stroke(payload),
      document.canvas_size_px,
    )
    .ok()
  }

  fn insert_sequence(
    &mut self,
    document: &BoardDocument,
    position: PointPx,
    actions: &mut Vec<EditorAction>,
  ) {
    let style = self.tool_style(EditorTool::Sequence);
    let stroke_style = match StrokeStyle::mvp(style.color_rgba, style.width_px) {
      Ok(style) => style,
      Err(_) => return,
    };
    let text_style = match TextStyle::mvp(style.color_rgba.contrasting_text(), style.font_size_px) {
      Ok(style) => style,
      Err(_) => return,
    };
    let (radius_px, pill_width_px) = match SequenceMarkerPayload::geometry_for(
      document.next_sequence_number,
      style.font_size_px,
    ) {
      Ok(geometry) => geometry,
      Err(_) => return,
    };
    let Ok(element) = Element::new(
      ElementId::new(),
      document.elements.len() as i64,
      ElementPayload::SequenceMarker(SequenceMarkerPayload {
        center_px: position,
        number: document.next_sequence_number,
        radius_px,
        pill_width_px,
        fill_rgba: style.color_rgba,
        stroke_style,
        text_style,
      }),
      document.canvas_size_px,
    ) else {
      return;
    };
    if let Ok(batch) = CommandBatch::sequence_marker(document, element) {
      actions.push(EditorAction::Command(batch));
    }
  }

  fn start_editing_existing(
    &mut self,
    document: &BoardDocument,
    element_id: ElementId,
    recreate_hidden_label: bool,
    actions: &mut Vec<EditorAction>,
  ) {
    let Some(element) = document.element(element_id) else {
      return;
    };
    match &element.payload {
      ElementPayload::Text(payload) => {
        self.text_editing = Some(TextEditing {
          target: TextTarget::ExistingText { element_id },
          buffer: payload.text.clone(),
          text_style: payload.text_style.clone(),
          ime: InlineImeState::default(),
          request_focus: true,
          select_all: false,
          auto_place_rectangle: false,
        });
      }
      ElementPayload::Arrow(payload) if payload.label.text.is_some() || recreate_hidden_label => {
        if payload.label.text.is_none() && !arrow_can_host_label(payload, document.canvas_size_px) {
          actions.push(EditorAction::Toast(ARROW_LABEL_TOO_SHORT_TOAST.to_owned()));
          return;
        }
        self.text_editing = Some(TextEditing {
          target: TextTarget::ArrowLabel { element_id },
          buffer: payload.label.text.clone().unwrap_or_default(),
          text_style: payload.label.text_style.clone(),
          ime: InlineImeState::default(),
          request_focus: true,
          select_all: false,
          auto_place_rectangle: false,
        });
      }
      ElementPayload::Rectangle(payload) => {
        if payload.label.text.is_none() && !recreate_hidden_label {
          return;
        }
        self.text_editing = Some(TextEditing {
          target: TextTarget::RectangleLabel { element_id },
          buffer: payload.label.text.clone().unwrap_or_default(),
          text_style: payload.label.text_style.clone(),
          ime: InlineImeState::default(),
          request_focus: true,
          select_all: false,
          auto_place_rectangle: payload.label.text.is_none(),
        });
      }
      _ => {}
    }
  }

  fn commit_text(&mut self, document: &BoardDocument, actions: &mut Vec<EditorAction>) {
    let Some(editing) = self.text_editing.take() else {
      return;
    };
    match editing.target {
      TextTarget::NewText { anchor_px } => {
        if editing.buffer.trim().is_empty() {
          return;
        }
        let box_width_px = text_width_to_canvas_edge(anchor_px, document.canvas_size_px);
        let Ok(element) = Element::new(
          ElementId::new(),
          document.elements.len() as i64,
          ElementPayload::Text(TextPayload {
            anchor_px,
            text: editing.buffer,
            box_width_px,
            text_style: editing.text_style,
          }),
          document.canvas_size_px,
        ) else {
          return;
        };
        actions.push(command_action(DocumentCommand::AddElement { element }));
      }
      TextTarget::ExistingText { element_id } => {
        if editing.buffer.trim().is_empty() {
          return;
        }
        let Some(element) = document.element(element_id) else {
          return;
        };
        let ElementPayload::Text(payload) = &element.payload else {
          return;
        };
        let box_width_px = text_width_to_canvas_edge(payload.anchor_px, document.canvas_size_px);
        if payload.text == editing.buffer && payload.box_width_px == box_width_px {
          return;
        }
        let mut payload = payload.clone();
        payload.text = editing.buffer;
        payload.box_width_px = box_width_px;
        payload.text_style = editing.text_style;
        actions.push(command_action(DocumentCommand::UpdateElement {
          element_id,
          payload: ElementPayload::Text(payload),
        }));
      }
      TextTarget::ArrowLabel { element_id } => {
        let Some(element) = document.element(element_id) else {
          return;
        };
        let ElementPayload::Arrow(payload) = &element.payload else {
          return;
        };
        let text = normalized_label_text(editing.buffer);
        if payload.label.text != text {
          actions.push(command_action(DocumentCommand::UpdateElementLabel { element_id, text }));
        }
      }
      TextTarget::RectangleLabel { element_id } => {
        let Some(element) = document.element(element_id) else {
          return;
        };
        let ElementPayload::Rectangle(payload) = &element.payload else {
          return;
        };
        let text = normalized_label_text(editing.buffer);
        let mut commands = Vec::with_capacity(1);
        if payload.label.text != text {
          commands.push(DocumentCommand::UpdateElementLabel { element_id, text });
        }
        if !commands.is_empty()
          && let Some(batch) = rectangle_reflow_batch(document, commands, element_id, &[])
        {
          actions.push(EditorAction::Command(batch));
        }
      }
    }
  }

  fn show_text_editor(
    &mut self,
    ctx: &egui::Context,
    transform: CanvasTransform,
    document: &BoardDocument,
    preserve_focus: bool,
  ) -> TextEditorCompletion {
    if self.text_editing.is_none() {
      return TextEditorCompletion::None;
    }
    let cancel = ctx.input_mut(|input| {
      input
        .events
        .iter()
        .position(|event| {
          matches!(event, Event::Key { key: Key::Escape, pressed: true, modifiers, .. } if !modifiers.command && !modifiers.ctrl && !modifiers.mac_cmd)
        })
        .map(|position| input.events.remove(position))
        .is_some()
    });
    if cancel {
      self.text_editing = None;
      return TextEditorCompletion::None;
    }
    let editor_id = inline_text_editor_id();
    let layer_id = egui::LayerId::new(egui::Order::Foreground, editor_id.with("layer"));
    ctx.set_transform_layer(layer_id, egui::emath::TSTransform::IDENTITY);
    ctx.move_to_top(layer_id);
    let layer_painter = ctx.layer_painter(layer_id);
    let editing = self.text_editing.as_ref().expect("text editing checked above");
    let preview = self.text_editing_preview_element(document);
    let Some(geometry) = inline_text_geometry_with_rendered_label_width(
      editing,
      document,
      preview.as_ref(),
      &layer_painter,
      transform.scale(),
    ) else {
      return TextEditorCompletion::None;
    };
    let screen_position = transform.document_to_egui(geometry.origin_px);
    let mut lost_focus = false;
    let editing = self.text_editing.as_mut().expect("text editor checked above");
    let submit_after_widget =
      ctx.input_mut(|input| normalize_inline_text_events(&mut input.events, &mut editing.ime));
    let font_id = FontId::proportional(editing.text_style.font_size_px * transform.scale());
    let text_color = color32(editing.text_style.color_rgba);
    let horizontal_align = match editing.text_style.align {
      TextAlign::Left => egui::Align::Min,
      TextAlign::Center => egui::Align::Center,
      TextAlign::Right => egui::Align::Max,
    };
    let desired_width = (geometry.editor_width_px * transform.scale()).max(1.0);
    let layout_wrap_width = (geometry.wrap_width_px * transform.scale()).max(1.0);
    let editor_rect = Rect::from_min_size(
      screen_position,
      egui::vec2(desired_width, transform.canvas_rect().height().max(1.0)),
    );
    let first_shape = ctx.graphics_mut(|graphics| graphics.entry(layer_id).next_idx());
    let mut ui = egui::Ui::new(
      ctx.clone(),
      editor_id.with("ui"),
      egui::UiBuilder::new().layer_id(layer_id).max_rect(editor_rect),
    );
    ui.set_clip_rect(transform.canvas_rect());
    ui.set_width(desired_width);
    let layout_text_style = editing.text_style.clone();
    let arrow_label = matches!(editing.target, TextTarget::ArrowLabel { .. });
    let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, _wrap_width: f32| {
      layout_egui_text_with_document_wrapping(
        ui.painter(),
        text.as_str(),
        &layout_text_style,
        layout_wrap_width,
        transform.scale(),
        1.0,
        arrow_label,
      )
    };
    let mut output = egui::TextEdit::multiline(&mut editing.buffer)
      .id(editor_id)
      .font(font_id)
      .text_color(text_color)
      .horizontal_align(horizontal_align)
      .desired_width(desired_width)
      .desired_rows(1)
      .frame(egui::Frame::NONE)
      .margin(egui::Margin::ZERO)
      .layouter(&mut layouter)
      .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
      .show(&mut ui);
    if editing.request_focus {
      use egui::text::{CCursor, CCursorRange};

      output.response.request_focus();
      output.state = Default::default();
      let character_count = editing.buffer.chars().count();
      let cursor_range = if editing.select_all {
        CCursorRange::two(CCursor::new(0), CCursor::new(character_count))
      } else {
        CCursorRange::one(CCursor::new(character_count))
      };
      output.state.cursor.set_char_range(Some(cursor_range));
      output.state.store(ctx, editor_id);
      editing.request_focus = false;
    } else {
      lost_focus = output.response.lost_focus();
    }
    if preserve_focus {
      output.response.request_focus();
      lost_focus = false;
    }
    if submit_after_widget {
      output.response.surrender_focus();
    }
    if let Some(updated_geometry) = inline_text_geometry_with_rendered_label_width(
      editing,
      document,
      preview.as_ref(),
      &layer_painter,
      transform.scale(),
    ) {
      let updated_position = transform.document_to_egui(updated_geometry.origin_px);
      translate_inline_text_layer(
        ctx,
        layer_id,
        first_shape,
        updated_position - screen_position,
        transform.canvas_rect(),
      );
    }
    if submit_after_widget {
      TextEditorCompletion::Enter
    } else if lost_focus {
      TextEditorCompletion::FocusLost
    } else {
      TextEditorCompletion::None
    }
  }

  fn show_toolbar(
    &mut self,
    ctx: &egui::Context,
    document: &BoardDocument,
    history: &CommandHistory,
    actions: &mut Vec<EditorAction>,
  ) {
    let screen = ctx.content_rect();
    let recenter = self.should_recenter_toolbar(screen);
    let area = toolbar_area(screen, recenter).show(ctx, |ui| {
      floating_panel_frame(ui.style()).show(ui, |ui| {
        set_floating_control_style(ui);
        ui.horizontal(|ui| {
          toolbar_drag_handle(ui);
          for tool in EditorTool::ALL {
            let response =
              ui.selectable_label(self.tool == tool, tool.label()).on_hover_text(tool.tooltip());
            self.tool_button_rects[tool.index()] = Some(response.rect);
            if response.clicked() {
              self.switch_tool(tool, document, actions);
            }
          }
          ui.separator();
          self.style_controls(ui, document, actions);
          ui.separator();
          if ui
            .add_enabled(history.can_undo(), egui::Button::new("↶"))
            .on_hover_text("撤销 (Cmd+Z)")
            .clicked()
          {
            actions.push(EditorAction::Undo);
          }
          if ui
            .add_enabled(history.can_redo(), egui::Button::new("↷"))
            .on_hover_text("重做 (Cmd+Shift+Z)")
            .clicked()
          {
            actions.push(EditorAction::Redo);
          }
          if ui.button("保存").on_hover_text("保存 (Cmd+S)").clicked() {
            if self.text_editing.is_some() {
              self.commit_text(document, actions);
            }
            actions.push(EditorAction::Save);
          }
          toolbar_drag_handle(ui);
        });
      });
    });
    self.toolbar_panel_rect = Some(area.response.rect);
    let toolbar_origin = area.response.rect.min.to_vec2();
    for rect in self.tool_button_rects.iter_mut().flatten() {
      *rect = rect.translate(toolbar_origin);
    }
    self.toolbar_was_moved |= area.response.dragged();
  }

  fn active_tool_button_clicked(&mut self, ctx: &egui::Context) -> bool {
    let Some(rect) = self.tool_button_rects[self.tool.index()] else {
      return false;
    };
    let clicked = ctx.input(|input| {
      if input.pointer.primary_pressed()
        && input.pointer.press_origin().is_some_and(|position| rect.contains(position))
      {
        self.active_tool_button_press_started = true;
      }
      self.active_tool_button_press_started && input.pointer.primary_released()
    });
    if ctx.input(|input| input.pointer.primary_released()) {
      self.active_tool_button_press_started = false;
    }
    clicked
  }

  fn canvas_primary_pressed(&self, response: &Response) -> bool {
    let Some(press_origin) = response.ctx.input(|input| {
      input.pointer.primary_pressed().then(|| input.pointer.press_origin()).flatten()
    }) else {
      return false;
    };
    if self.toolbar_panel_rect.is_some_and(|rect| rect.contains(press_origin)) {
      return false;
    }
    if response
      .ctx
      .read_response(inline_text_editor_id())
      .is_some_and(|editor| editor.rect.contains(press_origin))
    {
      return false;
    }
    response.rect.contains(press_origin)
  }

  fn should_recenter_toolbar(&mut self, screen: Rect) -> bool {
    let screen_changed = self.toolbar_screen_rect != Some(screen);
    self.toolbar_screen_rect = Some(screen);
    screen_changed && !self.toolbar_was_moved
  }

  fn style_controls(
    &mut self,
    ui: &mut egui::Ui,
    document: &BoardDocument,
    actions: &mut Vec<EditorAction>,
  ) {
    let displayed = self.displayed_style(document);
    let color = color32(displayed.color_rgba);
    egui::ComboBox::from_id_salt("rs-board-color")
      .selected_text(egui::RichText::new("●").color(color).size(COLOR_SWATCH_FONT_SIZE_PT))
      .width(38.0)
      .height(FLOATING_MENU_MAX_HEIGHT_PT)
      .show_ui(ui, |ui| {
        set_floating_control_style(ui);
        for choice in mvp_colors() {
          let text =
            egui::RichText::new("●").color(color32(choice)).size(COLOR_SWATCH_FONT_SIZE_PT);
          if ui.selectable_label(displayed.color_rgba == choice, text).clicked() {
            self.apply_style_change(
              document,
              StyleChange { color_rgba: Some(choice), ..StyleChange::default() },
              actions,
            );
          }
        }
      })
      .response
      .on_hover_text("颜色");

    egui::ComboBox::from_id_salt("rs-board-width")
      .selected_text(format!("{} px", displayed.width_px as i32))
      .width(62.0)
      .height(FLOATING_MENU_MAX_HEIGHT_PT)
      .show_ui(ui, |ui| {
        set_floating_control_style(ui);
        for width_px in PRESET_STROKE_WIDTHS_PX {
          if ui
            .selectable_label(displayed.width_px == width_px, format!("{} px", width_px as i32))
            .clicked()
          {
            self.apply_style_change(
              document,
              StyleChange { width_px: Some(width_px), ..StyleChange::default() },
              actions,
            );
          }
        }
      })
      .response
      .on_hover_text("线宽");

    if self.shows_brush_hardness(document) {
      egui::ComboBox::from_id_salt("rs-board-hardness")
        .selected_text(format!("硬度 {}%", hardness_percent(displayed.hardness)))
        .width(82.0)
        .height(FLOATING_MENU_MAX_HEIGHT_PT)
        .show_ui(ui, |ui| {
          set_floating_control_style(ui);
          for hardness in PRESET_BRUSH_HARDNESSES {
            if ui
              .selectable_label(
                displayed.hardness == hardness,
                format!("{}%", hardness_percent(hardness)),
              )
              .clicked()
            {
              self.apply_style_change(
                document,
                StyleChange { hardness: Some(hardness), ..StyleChange::default() },
                actions,
              );
            }
          }
        })
        .response
        .on_hover_text("画笔硬度");
    }

    egui::ComboBox::from_id_salt("rs-board-font-size")
      .selected_text(format!("{} pt", displayed.font_size_px as i32))
      .width(62.0)
      .height(FLOATING_MENU_MAX_HEIGHT_PT)
      .show_ui(ui, |ui| {
        set_floating_control_style(ui);
        for font_size_px in PRESET_FONT_SIZES_PX {
          if ui
            .selectable_label(
              displayed.font_size_px == font_size_px,
              format!("{} pt", font_size_px as i32),
            )
            .clicked()
          {
            self.apply_style_change(
              document,
              StyleChange { font_size_px: Some(font_size_px), ..StyleChange::default() },
              actions,
            );
          }
        }
      })
      .response
      .on_hover_text("字号");
  }

  fn show_option_panel(
    &mut self,
    ctx: &egui::Context,
    transform: CanvasTransform,
    document: &BoardDocument,
    actions: &mut Vec<EditorAction>,
  ) {
    let option_down = ctx.input(|input| input.modifiers.alt);
    if !option_down || self.text_editing.is_some() {
      self.option_panel_anchor = None;
      return;
    }
    if self.option_panel_anchor.is_none() {
      self.option_panel_anchor = ctx.input(|input| input.pointer.hover_pos());
    }
    let Some(anchor) = self.option_panel_anchor else {
      return;
    };
    egui::Area::new(Id::new("rs-board-option-panel"))
      .order(FLOATING_PANEL_ORDER)
      .fixed_pos(anchor + egui::vec2(14.0, 14.0))
      .constrain_to(ctx.content_rect())
      .show(ctx, |ui| {
        floating_panel_frame(ui.style()).show(ui, |ui| {
          set_floating_control_style(ui);
          ui.horizontal(|ui| self.style_controls(ui, document, actions));
          if let Some(element_id) = self.selected_element_id {
            ui.separator();
            ui.horizontal(|ui| {
              if ui.button("↑").on_hover_text("上移一层").clicked() {
                actions.push(rectangle_command_action(
                  document,
                  DocumentCommand::BringForward { element_id },
                  element_id,
                ));
              }
              if ui.button("↓").on_hover_text("下移一层").clicked() {
                actions.push(rectangle_command_action(
                  document,
                  DocumentCommand::SendBackward { element_id },
                  element_id,
                ));
              }
              if ui.button("⇈").on_hover_text("置于顶层").clicked() {
                actions.push(rectangle_command_action(
                  document,
                  DocumentCommand::BringToFront { element_id },
                  element_id,
                ));
              }
              if ui.button("⇊").on_hover_text("置于底层").clicked() {
                actions.push(rectangle_command_action(
                  document,
                  DocumentCommand::SendToBack { element_id },
                  element_id,
                ));
              }
            });
          }
          if self.tool == EditorTool::Sequence {
            ui.separator();
            if ui.button(format!("插入 {}", document.next_sequence_number)).clicked()
              && let Some(position) = transform.egui_to_document(anchor)
            {
              self.insert_sequence(document, position, actions);
            }
          }
        });
      });
  }

  fn displayed_style(&self, document: &BoardDocument) -> ToolStyle {
    self
      .selected_element_id
      .and_then(|element_id| document.element(element_id))
      .map(style_for_element)
      .unwrap_or_else(|| self.tool_style(self.tool))
  }

  fn shows_brush_hardness(&self, document: &BoardDocument) -> bool {
    self
      .selected_element_id
      .and_then(|element_id| document.element(element_id))
      .map(|element| matches!(element.payload, ElementPayload::Stroke(_)))
      .unwrap_or(self.tool == EditorTool::Stroke)
  }

  fn apply_style_change(
    &mut self,
    document: &BoardDocument,
    mut change: StyleChange,
    actions: &mut Vec<EditorAction>,
  ) {
    if let Some(color_rgba) = change.color_rgba {
      self.global_color = color_rgba;
      self.update_text_editing_color(color_rgba);
      actions.push(EditorAction::GlobalColorChanged { color_rgba });
    }

    if let Some(element_id) = self.selected_element_id
      && let Some(element) = document.element(element_id)
    {
      match element.payload {
        ElementPayload::Stroke(_) => change.font_size_px = None,
        ElementPayload::Arrow(_) => {
          change.hardness = None;
        }
        ElementPayload::Text(_) => {
          change.width_px = None;
          change.hardness = None;
        }
        ElementPayload::Rectangle(_) | ElementPayload::SequenceMarker(_) => {
          change.hardness = None;
        }
      }
      if change.color_rgba == Some(style_for_element(element).color_rgba) {
        change.color_rgba = None;
      }
      if change != StyleChange::default() {
        let pending_label_text = pending_element_label_text(actions, element_id);
        let effective_arrow_label_text = match &element.payload {
          ElementPayload::Arrow(payload) => {
            Some(pending_label_text.unwrap_or_else(|| payload.label.text.clone()))
          }
          _ => None,
        };
        if let Some(Some(label_text)) = effective_arrow_label_text {
          let mut staged = element.clone();
          let ElementPayload::Arrow(payload) = &mut staged.payload else {
            unreachable!("effective arrow label only exists for arrows");
          };
          payload.label.text = Some(label_text);
          if staged.set_style(&change, document.canvas_size_px).is_err() {
            actions.push(EditorAction::Toast(ARROW_LABEL_TOO_SHORT_TOAST.to_owned()));
            return;
          }
        }
        actions.push(rectangle_command_action(
          document,
          DocumentCommand::ChangeElementStyle { element_id, change },
          element_id,
        ));
      }
      return;
    }

    let style = &mut self.styles[self.tool.index()];
    if let Some(width) = change.width_px {
      style.width_px = width;
    }
    if let Some(font_size) = change.font_size_px {
      style.font_size_px = font_size;
    }
    if let Some(hardness) = change.hardness {
      style.hardness = hardness;
    }
  }

  fn update_text_editing_color(&mut self, color_rgba: ColorRgba) {
    let Some(editing) = self.text_editing.as_mut() else {
      return;
    };
    editing.text_style.color_rgba = match &editing.target {
      TextTarget::ArrowLabel { .. } | TextTarget::RectangleLabel { .. } => {
        color_rgba.contrasting_text()
      }
      TextTarget::NewText { .. } | TextTarget::ExistingText { .. } => color_rgba,
    };
  }

  fn paint_document_for_editing(
    &self,
    painter: &egui::Painter,
    transform: CanvasTransform,
    document: &BoardDocument,
  ) {
    let painter = painter.with_clip_rect(transform.canvas_rect());
    let interaction_preview = self
      .interaction
      .as_ref()
      .map(|interaction| interaction_preview_set(self, interaction, document))
      .unwrap_or_default();
    let preview_elements = if interaction_preview.is_empty() {
      &self.released_preview_elements
    } else {
      &interaction_preview
    };
    for element in &document.elements {
      let display_element = preview_elements.get(element.element_id).unwrap_or(element);
      match self.text_editing.as_ref().map(|editing| &editing.target) {
        Some(TextTarget::ExistingText { element_id }) if *element_id == element.element_id => {}
        Some(TextTarget::ArrowLabel { element_id }) if *element_id == element.element_id => {
          let ElementPayload::Arrow(payload) = &display_element.payload else {
            continue;
          };
          let editing = self.text_editing.as_ref().expect("text editing target checked above");
          let draft = arrow_label_draft(payload, editing);
          paint_arrow_without_label_text(&painter, &transform, &draft, 1.0);
        }
        Some(TextTarget::RectangleLabel { element_id }) if *element_id == element.element_id => {
          let ElementPayload::Rectangle(payload) = &display_element.payload else {
            continue;
          };
          let editing = self.text_editing.as_ref().expect("text editing target checked above");
          let draft = rectangle_label_draft(payload, editing, document, *element_id);
          paint_rectangle_without_label_text(&painter, &transform, &draft, 1.0);
        }
        _ => paint_element(&painter, &transform, display_element, 1.0),
      }
    }
    for preview in preview_elements.iter() {
      if document.element(preview.element_id).is_none() {
        paint_element(&painter, &transform, preview, 1.0);
      }
    }
  }

  fn paint_text_editing_controls(
    &self,
    painter: &egui::Painter,
    transform: CanvasTransform,
    document: &BoardDocument,
  ) {
    match self.text_editing.as_ref().map(|editing| &editing.target) {
      Some(TextTarget::ExistingText { .. }) | Some(TextTarget::NewText { .. }) => {
        if let Some(bounds) = self.text_editing_bounds_px(document) {
          paint_text_editing_frame(painter, transform, bounds);
        }
      }
      Some(TextTarget::ArrowLabel { .. }) | Some(TextTarget::RectangleLabel { .. }) => {
        self.paint_selection(painter, transform, document);
      }
      None => {}
    }
  }

  fn paint_interaction(
    &self,
    painter: &egui::Painter,
    transform: CanvasTransform,
    document: &BoardDocument,
  ) {
    let Some(interaction) = &self.interaction else {
      return;
    };
    match interaction {
      PointerInteraction::Draw { tool, start, current, stroke_points, .. } => match tool {
        EditorTool::Rectangle => {}
        EditorTool::Arrow => {
          if let Some(element) = self.make_arrow_preview(document, *start, *current) {
            paint_element(painter, &transform, &element, 0.72);
          }
        }
        EditorTool::Stroke => paint_raw_stroke_points(
          painter,
          &transform,
          stroke_points,
          self.tool_style(EditorTool::Stroke).color_rgba,
          self.tool_style(EditorTool::Stroke).width_px,
          self.tool_style(EditorTool::Stroke).hardness,
        ),
        EditorTool::Select | EditorTool::Text | EditorTool::Sequence => {}
      },
      PointerInteraction::Move { .. }
      | PointerInteraction::ResizeRectangle { .. }
      | PointerInteraction::UpdateArrowEndpoint { .. }
      | PointerInteraction::DragRectangleLabel { .. } => {}
    }
  }

  fn paint_selection(
    &self,
    painter: &egui::Painter,
    transform: CanvasTransform,
    document: &BoardDocument,
  ) {
    let Some(element_id) = self.selected_element_id else {
      return;
    };
    let interaction_preview = self
      .interaction
      .as_ref()
      .map(|interaction| interaction_preview_set(self, interaction, document))
      .unwrap_or_default();
    let preview_elements = if interaction_preview.is_empty() {
      &self.released_preview_elements
    } else {
      &interaction_preview
    };
    let Some(element) = preview_elements.get(element_id).or_else(|| document.element(element_id))
    else {
      return;
    };
    match &element.payload {
      ElementPayload::Rectangle(payload) => {
        for (_, point) in rectangle_handles(RectPx::from_points(payload.start_px, payload.end_px)) {
          paint_handle(painter, transform.document_to_egui(point));
        }
      }
      ElementPayload::Arrow(payload) => {
        paint_handle(painter, transform.document_to_egui(payload.start_px));
        paint_handle(painter, transform.document_to_egui(payload.end_px));
      }
      _ => {
        let rect = transform.document_rect_to_egui(element.bounds_px).expand(3.0);
        painter.rect_stroke(
          rect,
          egui::CornerRadius::ZERO,
          Stroke::new(1.0, Color32::WHITE),
          StrokeKind::Outside,
        );
      }
    }
  }
}

pub fn hit_test_document(
  document: &BoardDocument,
  position_px: PointPx,
  tolerance_px: f32,
) -> Option<ElementId> {
  hit_test_document_where(document, position_px, tolerance_px, |_| true)
}

fn hit_test_document_for_tool(
  document: &BoardDocument,
  position_px: PointPx,
  tolerance_px: f32,
  tool: EditorTool,
) -> Option<ElementId> {
  hit_test_document_where(document, position_px, tolerance_px, |element| {
    tool_selects_element(tool, element)
  })
}

fn hit_test_document_where(
  document: &BoardDocument,
  position_px: PointPx,
  tolerance_px: f32,
  predicate: impl Fn(&Element) -> bool,
) -> Option<ElementId> {
  document
    .elements
    .iter()
    .filter(|element| predicate(element))
    .filter(|element| hit_test_element(element, position_px, tolerance_px, document.canvas_size_px))
    .max_by_key(|element| element.z_index)
    .map(|element| element.element_id)
}

fn tool_selects_element(tool: EditorTool, element: &Element) -> bool {
  match tool {
    EditorTool::Select => true,
    EditorTool::Rectangle => matches!(element.payload, ElementPayload::Rectangle(_)),
    EditorTool::Arrow => matches!(element.payload, ElementPayload::Arrow(_)),
    EditorTool::Text => matches!(element.payload, ElementPayload::Text(_)),
    EditorTool::Sequence => matches!(element.payload, ElementPayload::SequenceMarker(_)),
    EditorTool::Stroke => false,
  }
}

fn single_click_starts_editing(
  document: &BoardDocument,
  element_id: ElementId,
  position_px: PointPx,
) -> bool {
  let Some(element) = document.element(element_id) else {
    return false;
  };
  match &element.payload {
    ElementPayload::Text(_) => true,
    ElementPayload::Arrow(payload) => arrow_label_layout(payload, document.canvas_size_px)
      .ok()
      .flatten()
      .is_some_and(|layout| contains(layout.bounds_px, position_px)),
    ElementPayload::Rectangle(payload) => rectangle_label_layout(payload, document.canvas_size_px)
      .ok()
      .flatten()
      .is_some_and(|layout| contains(layout.bounds_px, position_px)),
    ElementPayload::Stroke(_) | ElementPayload::SequenceMarker(_) => false,
  }
}

fn double_click_starts_editing(
  document: &BoardDocument,
  element_id: ElementId,
  position_px: PointPx,
) -> bool {
  let Some(element) = document.element(element_id) else {
    return false;
  };
  match &element.payload {
    ElementPayload::Text(_) => true,
    ElementPayload::Arrow(payload) => {
      payload.label.text.is_none()
        || arrow_label_layout(payload, document.canvas_size_px)
          .ok()
          .flatten()
          .is_some_and(|layout| contains(layout.bounds_px, position_px))
    }
    ElementPayload::Rectangle(payload) => {
      payload.label.text.is_none()
        || rectangle_label_layout(payload, document.canvas_size_px)
          .ok()
          .flatten()
          .is_some_and(|layout| contains(layout.bounds_px, position_px))
    }
    _ => false,
  }
}

fn rectangle_label_obstacles(
  document: &BoardDocument,
  current_element_id: Option<ElementId>,
) -> Vec<RectPx> {
  let mut obstacles = Vec::new();
  for element in &document.elements {
    if Some(element.element_id) == current_element_id {
      continue;
    }
    let ElementPayload::Rectangle(payload) = &element.payload else {
      continue;
    };
    obstacles.push(RectPx::from_points(payload.start_px, payload.end_px));
    if let Ok(Some(layout)) = rectangle_label_layout(payload, document.canvas_size_px) {
      obstacles.push(layout.bounds_px);
    }
  }
  obstacles
}

fn hit_test_element(
  element: &Element,
  point: PointPx,
  tolerance: f32,
  canvas_size: SizePx,
) -> bool {
  if !contains(element.bounds_px.expanded(tolerance), point) {
    return false;
  }
  match &element.payload {
    ElementPayload::Stroke(payload) => match payload.points.as_slice() {
      [stroke_point] => {
        point.distance_to(stroke_point.point()) <= payload.stroke_style.width_px / 2.0 + tolerance
      }
      points => points.windows(2).any(|points| {
        distance_to_segment(point, points[0].point(), points[1].point())
          <= payload.stroke_style.width_px / 2.0 + tolerance
      }),
    },
    ElementPayload::Arrow(payload) => {
      hit_arrow_body(payload, point, tolerance)
        || arrow_label_layout(payload, canvas_size)
          .ok()
          .flatten()
          .is_some_and(|layout| contains(layout.bounds_px.expanded(tolerance), point))
    }
    ElementPayload::Rectangle(payload) => {
      contains(RectPx::from_points(payload.start_px, payload.end_px).expanded(tolerance), point)
        || rectangle_label_layout(payload, canvas_size)
          .ok()
          .flatten()
          .is_some_and(|layout| contains(layout.bounds_px.expanded(tolerance), point))
    }
    ElementPayload::Text(_) | ElementPayload::SequenceMarker(_) => {
      contains(element.bounds_px.expanded(tolerance), point)
    }
  }
}

fn hit_arrow_body(payload: &ArrowPayload, point: PointPx, tolerance: f32) -> bool {
  if distance_to_segment(point, payload.start_px, payload.end_px)
    <= payload.stroke_style.width_px / 2.0 + tolerance
  {
    return true;
  }
  let direction = payload.end_px - payload.start_px;
  let length = payload.start_px.distance_to(payload.end_px);
  if length <= f32::EPSILON {
    return false;
  }
  let unit = PointPx::new(direction.x_px / length, direction.y_px / length);
  let perpendicular = PointPx::new(-unit.y_px, unit.x_px);
  let base = PointPx::new(
    payload.end_px.x_px - unit.x_px * payload.head.length_px,
    payload.end_px.y_px - unit.y_px * payload.head.length_px,
  );
  let half_width = payload.head.width_px / 2.0 + tolerance;
  let left = PointPx::new(
    base.x_px + perpendicular.x_px * half_width,
    base.y_px + perpendicular.y_px * half_width,
  );
  let right = PointPx::new(
    base.x_px - perpendicular.x_px * half_width,
    base.y_px - perpendicular.y_px * half_width,
  );
  point_in_triangle(point, payload.end_px, left, right)
}

fn point_in_triangle(point: PointPx, first: PointPx, second: PointPx, third: PointPx) -> bool {
  fn sign(point: PointPx, first: PointPx, second: PointPx) -> f32 {
    (point.x_px - second.x_px) * (first.y_px - second.y_px)
      - (first.x_px - second.x_px) * (point.y_px - second.y_px)
  }
  let first_sign = sign(point, first, second);
  let second_sign = sign(point, second, third);
  let third_sign = sign(point, third, first);
  let has_negative = first_sign < 0.0 || second_sign < 0.0 || third_sign < 0.0;
  let has_positive = first_sign > 0.0 || second_sign > 0.0 || third_sign > 0.0;
  !(has_negative && has_positive)
}

fn append_stroke_point(points: &mut Vec<StrokePoint>, point: StrokePoint) {
  if let Some(last) = points.last_mut()
    && last.point() == point.point()
  {
    last.pressure = point.pressure;
  } else {
    points.push(point);
  }
}

fn is_stylus_event(event: &Event) -> bool {
  matches!(event, Event::Touch { force: Some(_), .. })
}

fn stylus_event(event: &Event) -> Option<StylusEvent> {
  let Event::Touch { device_id, id, phase, pos, force: Some(force) } = event else {
    return None;
  };
  let pressure = match phase {
    TouchPhase::End | TouchPhase::Cancel => 0.0,
    TouchPhase::Start | TouchPhase::Move => {
      if force.is_finite() {
        force.clamp(0.0, 1.0)
      } else {
        1.0
      }
    }
  };
  Some(StylusEvent {
    id: StylusId { device_id: *device_id, touch_id: *id },
    phase: *phase,
    position: *pos,
    pressure,
  })
}

fn append_stylus_sample(points: &mut Vec<StrokePoint>, sample: StylusSample) {
  match sample.phase {
    TouchPhase::Cancel => {}
    TouchPhase::End => {
      if points.last().is_some_and(|point| point.point() == sample.point) && points.len() == 1 {
        return;
      }
      append_stroke_point(
        points,
        StrokePoint::with_pressure(sample.point, 0.0)
          .expect("captured stroke pressure is normalized"),
      );
    }
    TouchPhase::Start | TouchPhase::Move => append_stroke_point(
      points,
      StrokePoint::with_pressure(sample.point, sample.pressure)
        .expect("captured stroke pressure is normalized"),
    ),
  }
}

fn apply_release_taper(points: &mut [StrokePoint], width_px: f32) {
  if points.len() < 2 || !width_px.is_finite() || width_px <= 0.0 {
    return;
  }
  let taper_distance = width_px * RELEASE_TAPER_WIDTH_FACTOR;
  let mut distance_from_tip = 0.0;
  for index in (0..points.len() - 1).rev() {
    distance_from_tip += points[index].point().distance_to(points[index + 1].point());
    if distance_from_tip >= taper_distance {
      break;
    }
    let pressure_cap = (distance_from_tip / taper_distance).clamp(0.0, 1.0);
    points[index].pressure = points[index].pressure.min(pressure_cap);
  }
}

fn hit_rectangle_handle(
  element: &Element,
  point: PointPx,
  transform: CanvasTransform,
) -> Option<RectangleHandle> {
  let ElementPayload::Rectangle(payload) = &element.payload else {
    return None;
  };
  rectangle_handles(RectPx::from_points(payload.start_px, payload.end_px))
    .into_iter()
    .find(|(_, handle)| {
      transform.document_to_egui(*handle).distance(transform.document_to_egui(point))
        <= HANDLE_HIT_RADIUS_PT
    })
    .map(|(handle, _)| handle)
}

fn hit_arrow_handle(
  element: &Element,
  point: PointPx,
  transform: CanvasTransform,
) -> Option<ArrowEndpoint> {
  let ElementPayload::Arrow(payload) = &element.payload else {
    return None;
  };
  let screen_point = transform.document_to_egui(point);
  if screen_point.distance(transform.document_to_egui(payload.start_px)) <= HANDLE_HIT_RADIUS_PT {
    Some(ArrowEndpoint::Start)
  } else if screen_point.distance(transform.document_to_egui(payload.end_px))
    <= HANDLE_HIT_RADIUS_PT
  {
    Some(ArrowEndpoint::End)
  } else {
    None
  }
}

fn rectangle_handles(rect: RectPx) -> [(RectangleHandle, PointPx); 8] {
  let center = rect.center();
  [
    (RectangleHandle::TopLeft, rect.min),
    (RectangleHandle::Top, PointPx::new(center.x_px, rect.min.y_px)),
    (RectangleHandle::TopRight, PointPx::new(rect.max.x_px, rect.min.y_px)),
    (RectangleHandle::Right, PointPx::new(rect.max.x_px, center.y_px)),
    (RectangleHandle::BottomRight, rect.max),
    (RectangleHandle::Bottom, PointPx::new(center.x_px, rect.max.y_px)),
    (RectangleHandle::BottomLeft, PointPx::new(rect.min.x_px, rect.max.y_px)),
    (RectangleHandle::Left, PointPx::new(rect.min.x_px, center.y_px)),
  ]
}

fn resized_rectangle(
  original: RectPx,
  handle: RectangleHandle,
  current: PointPx,
) -> (PointPx, PointPx) {
  let mut minimum = original.min;
  let mut maximum = original.max;
  match handle {
    RectangleHandle::TopLeft => minimum = current,
    RectangleHandle::Top => minimum.y_px = current.y_px,
    RectangleHandle::TopRight => {
      minimum.y_px = current.y_px;
      maximum.x_px = current.x_px;
    }
    RectangleHandle::Right => maximum.x_px = current.x_px,
    RectangleHandle::BottomRight => maximum = current,
    RectangleHandle::Bottom => maximum.y_px = current.y_px,
    RectangleHandle::BottomLeft => {
      minimum.x_px = current.x_px;
      maximum.y_px = current.y_px;
    }
    RectangleHandle::Left => minimum.x_px = current.x_px,
  }
  (minimum, maximum)
}

fn interaction_preview_element(
  interaction: &PointerInteraction,
  document: &BoardDocument,
) -> Option<Element> {
  match interaction {
    PointerInteraction::Draw { .. } => None,
    PointerInteraction::Move { element_id, start, current } => {
      let mut preview = document.element(*element_id)?.clone();
      preview.move_by(*current - *start, document.canvas_size_px).ok()?;
      Some(preview)
    }
    PointerInteraction::ResizeRectangle { element_id, handle, original, current } => {
      let mut preview = document.element(*element_id)?.clone();
      let meets_minimum = {
        let ElementPayload::Rectangle(payload) = &mut preview.payload else {
          return None;
        };
        (payload.start_px, payload.end_px) = resized_rectangle(*original, *handle, *current);
        let body = RectPx::from_points(payload.start_px, payload.end_px);
        minimum_geometry_extent(payload.stroke_style.width_px)
          .is_ok_and(|minimum| body.width() >= minimum && body.height() >= minimum)
      };
      preview.refresh_bounds(document.canvas_size_px).ok()?;
      if meets_minimum {
        preview.constrain_to_canvas(document.canvas_size_px, true).ok()?;
      }
      Some(preview)
    }
    PointerInteraction::UpdateArrowEndpoint { element_id, endpoint, current } => {
      let mut preview = document.element(*element_id)?.clone();
      let meets_minimum = {
        let ElementPayload::Arrow(payload) = &mut preview.payload else {
          return None;
        };
        let current = if payload.label.text.is_some() {
          clamped_arrow_endpoint(payload, *endpoint, *current).0
        } else {
          *current
        };
        match endpoint {
          ArrowEndpoint::Start => payload.start_px = current,
          ArrowEndpoint::End => payload.end_px = current,
        }
        payload.start_px.distance_to(payload.end_px) >= payload.head.min_body_length_px
      };
      preview.refresh_bounds(document.canvas_size_px).ok()?;
      if meets_minimum {
        preview.constrain_to_canvas(document.canvas_size_px, true).ok()?;
      }
      Some(preview)
    }
    PointerInteraction::DragRectangleLabel { element_id, current, grab_offset_px } => {
      let mut preview = document.element(*element_id)?.clone();
      let ElementPayload::Rectangle(payload) = &mut preview.payload else {
        return None;
      };
      payload.label_anchor =
        snap_rectangle_label_layout(payload, document.canvas_size_px, *current + *grab_offset_px)
          .ok()
          .map(|layout| layout.anchor)
          .unwrap_or(payload.label_anchor);
      payload.preferred_label_anchor = payload.label_anchor;
      preview.refresh_bounds(document.canvas_size_px).ok()?;
      Some(preview)
    }
  }
}

fn interaction_preview_set(
  controller: &EditorController,
  interaction: &PointerInteraction,
  document: &BoardDocument,
) -> ElementPreviewSet {
  match interaction {
    PointerInteraction::Draw {
      element_id, tool: EditorTool::Rectangle, start, current, ..
    } => {
      let Some(preview) =
        controller.make_rectangle_preview(document, *element_id, *start, *current)
      else {
        return ElementPreviewSet::default();
      };
      if preview.validate(document.canvas_size_px).is_err() {
        return ElementPreviewSet::single(preview);
      }
      rectangle_reflow_batch(
        document,
        vec![DocumentCommand::AddElement { element: preview.clone() }],
        *element_id,
        &[],
      )
      .map(|batch| preview_set_after_batch(document, &batch))
      .filter(|set| !set.is_empty())
      .unwrap_or_else(|| ElementPreviewSet::single(preview))
    }
    PointerInteraction::Move { element_id, start, current } => {
      let Some(element) = document.element(*element_id) else {
        return ElementPreviewSet::default();
      };
      if !matches!(element.payload, ElementPayload::Rectangle(_)) {
        return interaction_preview_element(interaction, document)
          .map(ElementPreviewSet::single)
          .unwrap_or_default();
      }
      let delta_px = *current - *start;
      if delta_px.distance_to(PointPx::ZERO) <= 0.01 {
        return ElementPreviewSet::default();
      }
      rectangle_reflow_batch(
        document,
        vec![DocumentCommand::MoveElement { element_id: *element_id, delta_px }],
        *element_id,
        &[],
      )
      .map(|batch| preview_set_after_batch(document, &batch))
      .filter(|set| !set.is_empty())
      .unwrap_or_else(|| {
        interaction_preview_element(interaction, document)
          .map(ElementPreviewSet::single)
          .unwrap_or_default()
      })
    }
    PointerInteraction::ResizeRectangle { element_id, handle, original, current } => {
      let Some(preview) = interaction_preview_element(interaction, document) else {
        return ElementPreviewSet::default();
      };
      let ElementPayload::Rectangle(payload) = &preview.payload else {
        return ElementPreviewSet::single(preview);
      };
      let Ok(minimum) = minimum_geometry_extent(payload.stroke_style.width_px) else {
        return ElementPreviewSet::single(preview);
      };
      let (raw_start_px, raw_end_px) = resized_rectangle(*original, *handle, *current);
      let raw_body = RectPx::from_points(raw_start_px, raw_end_px);
      if raw_body.width() < minimum || raw_body.height() < minimum {
        return ElementPreviewSet::single(preview);
      }
      let (start_px, end_px) = minimum_resized_rectangle(*original, *handle, *current, minimum);
      rectangle_reflow_batch(
        document,
        vec![DocumentCommand::ResizeRectangle { element_id: *element_id, start_px, end_px }],
        *element_id,
        &[],
      )
      .map(|batch| preview_set_after_batch(document, &batch))
      .filter(|set| !set.is_empty())
      .unwrap_or_else(|| ElementPreviewSet::single(preview))
    }
    PointerInteraction::DragRectangleLabel { element_id, current, grab_offset_px } => {
      let Some(element) = document.element(*element_id) else {
        return ElementPreviewSet::default();
      };
      let ElementPayload::Rectangle(payload) = &element.payload else {
        return ElementPreviewSet::default();
      };
      let Some(layout) =
        snap_rectangle_label_layout(payload, document.canvas_size_px, *current + *grab_offset_px)
          .ok()
      else {
        return ElementPreviewSet::default();
      };
      if layout.anchor == payload.label_anchor && layout.anchor == payload.preferred_label_anchor {
        return ElementPreviewSet::default();
      }
      rectangle_reflow_batch(
        document,
        vec![DocumentCommand::SetRectangleLabelPlacement {
          element_id: *element_id,
          preferred_anchor: layout.anchor,
          actual_anchor: layout.anchor,
        }],
        *element_id,
        &[],
      )
      .map(|batch| preview_set_after_batch(document, &batch))
      .filter(|set| !set.is_empty())
      .unwrap_or_default()
    }
    PointerInteraction::Draw { .. } | PointerInteraction::UpdateArrowEndpoint { .. } => {
      interaction_preview_element(interaction, document)
        .map(ElementPreviewSet::single)
        .unwrap_or_default()
    }
  }
}

fn minimum_resized_rectangle(
  original: RectPx,
  handle: RectangleHandle,
  current: PointPx,
  minimum: f32,
) -> (PointPx, PointPx) {
  let (mut start_px, mut end_px) = resized_rectangle(original, handle, current);
  match handle {
    RectangleHandle::TopLeft => {
      start_px.x_px = coordinate_at_minimum(end_px.x_px, start_px.x_px, minimum, -1.0);
      start_px.y_px = coordinate_at_minimum(end_px.y_px, start_px.y_px, minimum, -1.0);
    }
    RectangleHandle::Top => {
      start_px.y_px = coordinate_at_minimum(end_px.y_px, start_px.y_px, minimum, -1.0);
    }
    RectangleHandle::TopRight => {
      end_px.x_px = coordinate_at_minimum(start_px.x_px, end_px.x_px, minimum, 1.0);
      start_px.y_px = coordinate_at_minimum(end_px.y_px, start_px.y_px, minimum, -1.0);
    }
    RectangleHandle::Right => {
      end_px.x_px = coordinate_at_minimum(start_px.x_px, end_px.x_px, minimum, 1.0);
    }
    RectangleHandle::BottomRight => {
      end_px.x_px = coordinate_at_minimum(start_px.x_px, end_px.x_px, minimum, 1.0);
      end_px.y_px = coordinate_at_minimum(start_px.y_px, end_px.y_px, minimum, 1.0);
    }
    RectangleHandle::Bottom => {
      end_px.y_px = coordinate_at_minimum(start_px.y_px, end_px.y_px, minimum, 1.0);
    }
    RectangleHandle::BottomLeft => {
      start_px.x_px = coordinate_at_minimum(end_px.x_px, start_px.x_px, minimum, -1.0);
      end_px.y_px = coordinate_at_minimum(start_px.y_px, end_px.y_px, minimum, 1.0);
    }
    RectangleHandle::Left => {
      start_px.x_px = coordinate_at_minimum(end_px.x_px, start_px.x_px, minimum, -1.0);
    }
  }
  (start_px, end_px)
}

fn coordinate_at_minimum(fixed: f32, requested: f32, minimum: f32, fallback_direction: f32) -> f32 {
  let offset = requested - fixed;
  if offset.abs() >= minimum {
    requested
  } else if offset.abs() > f32::EPSILON {
    fixed + offset.signum() * minimum
  } else {
    fixed + fallback_direction * minimum
  }
}

fn point_at_minimum_distance(
  fixed_px: PointPx,
  requested_px: PointPx,
  minimum: f32,
  fallback_direction: PointPx,
) -> PointPx {
  let requested_direction = requested_px - fixed_px;
  let requested_length = requested_px.distance_to(fixed_px);
  if requested_length >= minimum {
    return requested_px;
  }

  let fallback_length = fallback_direction.distance_to(PointPx::ZERO);
  let (direction, direction_length) = if requested_length > f32::EPSILON {
    (requested_direction, requested_length)
  } else if fallback_length > f32::EPSILON {
    (fallback_direction, fallback_length)
  } else {
    (PointPx::new(1.0, 0.0), 1.0)
  };
  let coordinate_scale = fixed_px
    .x_px
    .abs()
    .max(fixed_px.y_px.abs())
    .max(requested_px.x_px.abs())
    .max(requested_px.y_px.abs())
    .max(minimum)
    .max(1.0);
  let target_distance =
    minimum + coordinate_scale * f32::EPSILON * MINIMUM_DISTANCE_ROUNDING_FACTOR;
  let scale = target_distance / direction_length;
  PointPx::new(fixed_px.x_px + direction.x_px * scale, fixed_px.y_px + direction.y_px * scale)
}

fn clamped_arrow_endpoint(
  payload: &ArrowPayload,
  endpoint: ArrowEndpoint,
  requested_px: PointPx,
) -> (PointPx, bool) {
  let (original_px, fixed_px) = match endpoint {
    ArrowEndpoint::Start => (payload.start_px, payload.end_px),
    ArrowEndpoint::End => (payload.end_px, payload.start_px),
  };
  let label_minimum =
    payload.label.text.as_ref().and_then(|_| arrow_minimum_length_for_label(payload).ok());
  let minimum = label_minimum.unwrap_or(payload.head.min_body_length_px);
  let constrained_for_label =
    label_minimum.is_some_and(|minimum| requested_px.distance_to(fixed_px) < minimum);
  (
    point_at_minimum_distance(fixed_px, requested_px, minimum, original_px - fixed_px),
    constrained_for_label,
  )
}

fn arrow_can_host_label(payload: &ArrowPayload, canvas_size_px: SizePx) -> bool {
  let mut draft = payload.clone();
  draft.label.text = Some(EMPTY_LABEL_DRAFT.to_owned());
  arrow_label_layout(&draft, canvas_size_px).is_ok_and(|layout| layout.is_some())
}

fn map_shortcut(key: Key, modifiers: Modifiers, text_editing: bool) -> Option<ShortcutAction> {
  let command = modifiers.command || modifiers.mac_cmd || modifiers.ctrl;
  if text_editing {
    if key == Key::Escape {
      return Some(ShortcutAction::CloseOrCancel);
    }
    if command {
      if let Some(tool) = tool_for_key(key) {
        return Some(ShortcutAction::Tool(tool));
      }
      if key == Key::S {
        return Some(ShortcutAction::Save);
      }
    }
    return None;
  }

  if let Some(tool) = tool_for_key(key)
    && (!modifiers.shift && !modifiers.alt)
  {
    return Some(ShortcutAction::Tool(tool));
  }
  if command && key == Key::Z {
    return Some(if modifiers.shift { ShortcutAction::Redo } else { ShortcutAction::Undo });
  }
  if command && key == Key::S {
    return Some(ShortcutAction::Save);
  }
  if command && key == Key::C {
    return Some(ShortcutAction::Copy);
  }
  if command && key == Key::V {
    return Some(ShortcutAction::Paste);
  }
  if key == Key::Enter && modifiers.is_none() {
    return Some(ShortcutAction::Deselect);
  }
  if key == Key::Escape {
    return Some(ShortcutAction::CloseOrCancel);
  }
  if key == Key::Delete || key == Key::Backspace {
    return Some(ShortcutAction::Delete);
  }
  None
}

fn text_width_to_canvas_edge(anchor_px: PointPx, canvas_size_px: SizePx) -> f32 {
  let canvas_width_px = canvas_size_px.width_px as f32;
  (canvas_width_px - anchor_px.x_px).clamp(1.0, canvas_width_px)
}
#[allow(dead_code)]
fn inline_text_geometry(
  editing: &TextEditing,
  document: &BoardDocument,
) -> Option<InlineTextGeometry> {
  inline_text_geometry_with_preview(editing, document, None)
}

fn inline_text_geometry_with_preview(
  editing: &TextEditing,
  document: &BoardDocument,
  preview: Option<&Element>,
) -> Option<InlineTextGeometry> {
  match editing.target {
    TextTarget::NewText { anchor_px } => Some(InlineTextGeometry {
      origin_px: anchor_px,
      wrap_width_px: text_width_to_canvas_edge(anchor_px, document.canvas_size_px),
      editor_width_px: text_width_to_canvas_edge(anchor_px, document.canvas_size_px),
    }),
    TextTarget::ExistingText { element_id } => {
      let ElementPayload::Text(payload) = &target_element(document, preview, element_id)?.payload
      else {
        return None;
      };
      Some(InlineTextGeometry {
        origin_px: payload.anchor_px,
        wrap_width_px: text_width_to_canvas_edge(payload.anchor_px, document.canvas_size_px),
        editor_width_px: text_width_to_canvas_edge(payload.anchor_px, document.canvas_size_px),
      })
    }
    TextTarget::ArrowLabel { element_id } => {
      let ElementPayload::Arrow(payload) = &target_element(document, preview, element_id)?.payload
      else {
        return None;
      };
      let draft = arrow_label_draft(payload, editing);
      let layout = arrow_label_layout(&draft, document.canvas_size_px).ok()??;
      Some(inline_label_geometry(
        layout.bounds_px,
        layout.text_wrap_width_px,
        draft.label.padding_px,
        editing.text_style.align,
      ))
    }
    TextTarget::RectangleLabel { element_id } => {
      // A newly-created rectangle is added after this frame. Waiting for it to
      // enter the document avoids briefly placing its editor at the origin.
      let ElementPayload::Rectangle(payload) =
        &target_element(document, preview, element_id)?.payload
      else {
        return None;
      };
      let draft = rectangle_label_draft(payload, editing, document, element_id);
      let layout = rectangle_label_layout(&draft, document.canvas_size_px).ok()??;
      Some(inline_label_geometry(
        layout.bounds_px,
        layout.text_wrap_width_px,
        draft.label.padding_px,
        editing.text_style.align,
      ))
    }
  }
}

fn inline_text_geometry_with_rendered_label_width(
  editing: &TextEditing,
  document: &BoardDocument,
  preview: Option<&Element>,
  painter: &egui::Painter,
  scale: f32,
) -> Option<InlineTextGeometry> {
  let (bounds_px, text_wrap_width_px, padding_px) = match editing.target {
    TextTarget::RectangleLabel { element_id } => {
      let ElementPayload::Rectangle(payload) =
        &target_element(document, preview, element_id)?.payload
      else {
        return None;
      };
      let draft = rectangle_label_draft(payload, editing, document, element_id);
      let layout = rectangle_label_layout(&draft, document.canvas_size_px).ok()??;
      let bounds_px = measured_rectangle_label_bounds(
        painter,
        &layout,
        draft.label.visible_text().unwrap_or_default(),
        &draft.label.text_style,
        draft.label.padding_px,
        scale,
      );
      (bounds_px, layout.text_wrap_width_px, draft.label.padding_px)
    }
    TextTarget::ArrowLabel { element_id } => {
      let ElementPayload::Arrow(payload) = &target_element(document, preview, element_id)?.payload
      else {
        return None;
      };
      let draft = arrow_label_draft(payload, editing);
      let layout = arrow_label_layout(&draft, document.canvas_size_px).ok()??;
      let bounds_px = measured_arrow_label_bounds(
        painter,
        &layout,
        draft.label.visible_text().unwrap_or_default(),
        &draft.label.text_style,
        draft.label.padding_px,
        scale,
      );
      (bounds_px, layout.text_wrap_width_px, draft.label.padding_px)
    }
    _ => return inline_text_geometry_with_preview(editing, document, preview),
  };
  Some(inline_label_geometry(bounds_px, text_wrap_width_px, padding_px, editing.text_style.align))
}

fn target_element<'a>(
  document: &'a BoardDocument,
  preview: Option<&'a Element>,
  element_id: ElementId,
) -> Option<&'a Element> {
  preview
    .filter(|element| element.element_id == element_id)
    .or_else(|| document.element(element_id))
}

fn text_target_element_id(target: &TextTarget) -> Option<ElementId> {
  match target {
    TextTarget::NewText { .. } => None,
    TextTarget::ExistingText { element_id }
    | TextTarget::ArrowLabel { element_id }
    | TextTarget::RectangleLabel { element_id } => Some(*element_id),
  }
}

fn text_editing_bounds_px_with_preview(
  editing: &TextEditing,
  document: &BoardDocument,
  preview: Option<&Element>,
) -> Option<RectPx> {
  match editing.target {
    TextTarget::NewText { anchor_px } => {
      text_draft_bounds_px(anchor_px, &editing.buffer, &editing.text_style, document.canvas_size_px)
    }
    TextTarget::ExistingText { element_id } => {
      let ElementPayload::Text(payload) = &target_element(document, preview, element_id)?.payload
      else {
        return None;
      };
      Some(payload_text_bounds_px(payload, document.canvas_size_px))
    }
    TextTarget::ArrowLabel { .. } | TextTarget::RectangleLabel { .. } => None,
  }
}

fn text_draft_bounds_px(
  anchor_px: PointPx,
  text: &str,
  text_style: &TextStyle,
  canvas_size_px: SizePx,
) -> Option<RectPx> {
  let box_width_px = text_width_to_canvas_edge(anchor_px, canvas_size_px);
  let text = if text.trim().is_empty() { EMPTY_LABEL_DRAFT } else { text };
  let layout = layout_text(text, text_style, box_width_px).ok()?;
  Some(RectPx::from_min_max(
    anchor_px,
    PointPx::new(anchor_px.x_px + layout.width_px, anchor_px.y_px + layout.height_px),
  ))
}

fn payload_text_bounds_px(payload: &TextPayload, canvas_size_px: SizePx) -> RectPx {
  text_draft_bounds_px(payload.anchor_px, &payload.text, &payload.text_style, canvas_size_px)
    .unwrap_or(RectPx::from_min_max(payload.anchor_px, payload.anchor_px))
}

fn inline_label_geometry(
  bounds_px: RectPx,
  text_wrap_width_px: f32,
  padding_px: f32,
  _align: TextAlign,
) -> InlineTextGeometry {
  let editor_width_px = (bounds_px.width() - padding_px * 2.0).max(1.0);
  InlineTextGeometry {
    origin_px: bounds_px.min + PointPx::new(padding_px, padding_px),
    wrap_width_px: text_wrap_width_px,
    editor_width_px,
  }
}

fn arrow_label_draft(payload: &ArrowPayload, editing: &TextEditing) -> ArrowPayload {
  let mut draft = payload.clone();
  draft.label.text = Some(if editing.buffer.trim().is_empty() {
    EMPTY_LABEL_DRAFT.to_owned()
  } else {
    editing.buffer.clone()
  });
  draft.label.text_style = editing.text_style.clone();
  draft
}

fn rectangle_label_draft(
  payload: &RectanglePayload,
  editing: &TextEditing,
  document: &BoardDocument,
  element_id: ElementId,
) -> RectanglePayload {
  let mut draft = payload.clone();
  draft.label.text = Some(if editing.buffer.trim().is_empty() {
    EMPTY_LABEL_DRAFT.to_owned()
  } else {
    editing.buffer.clone()
  });
  draft.label.text_style = editing.text_style.clone();
  if editing.auto_place_rectangle {
    let obstacles = rectangle_label_obstacles(document, Some(element_id));
    if let Ok(anchor) = choose_rectangle_label_anchor(&draft, document.canvas_size_px, &obstacles) {
      draft.label_anchor = anchor;
    }
  }
  draft
}

fn normalized_label_text(text: String) -> Option<String> {
  (!text.trim().is_empty()).then_some(text)
}

fn inline_text_editor_id() -> Id {
  Id::new("rs-board-inline-text-editor")
}

fn translate_inline_text_layer(
  ctx: &egui::Context,
  layer_id: egui::LayerId,
  first_shape: egui::layers::ShapeIdx,
  delta: egui::Vec2,
  canvas_clip_rect: Rect,
) {
  if delta == egui::Vec2::ZERO {
    return;
  }
  ctx.graphics_mut(|graphics| {
    let paint_list = graphics.entry(layer_id);
    let end_shape = paint_list.next_idx();
    for index in first_shape.0..end_shape.0 {
      paint_list.mutate_shape(egui::layers::ShapeIdx(index), |clipped_shape| {
        clipped_shape.clip_rect = canvas_clip_rect.translate(-delta);
      });
    }
  });
  ctx.set_transform_layer(layer_id, egui::emath::TSTransform::from_translation(delta));
  ctx.output_mut(|output| {
    if let Some(ime) = &mut output.ime {
      ime.rect = ime.rect.translate(delta);
      ime.cursor_rect = ime.cursor_rect.translate(delta);
    }
  });
}

fn strip_one_logical_trailing_newline(text: &mut String) -> bool {
  if text.ends_with("\r\n") {
    text.truncate(text.len() - 2);
    true
  } else if text.ends_with(['\r', '\n']) {
    text.truncate(text.len() - 1);
    true
  } else {
    false
  }
}

fn is_single_logical_newline(text: &str) -> bool {
  matches!(text, "\n" | "\r" | "\r\n")
}

fn is_unmodified_enter(modifiers: &Modifiers) -> bool {
  modifiers.is_none()
}

fn synthetic_shift_enter_event() -> Event {
  Event::Key {
    key: Key::Enter,
    physical_key: Some(Key::Enter),
    pressed: true,
    repeat: false,
    modifiers: Modifiers::SHIFT,
  }
}

fn normalize_inline_text_events(events: &mut Vec<Event>, ime: &mut InlineImeState) -> bool {
  let mut active_preedit = ime.preedit.clone();
  let mut confirmation_preedit = active_preedit.clone();
  let mut confirming_preedit_with_enter = ime.confirming_preedit_with_enter;
  let mut consume_following_confirm_enter = false;
  let mut submit_after_widget = false;
  events.retain_mut(|event| match event {
    Event::Ime(ImeEvent::Preedit { text, .. }) => {
      if text.is_empty() {
        active_preedit = None;
        confirming_preedit_with_enter = false;
      } else {
        active_preedit = Some(text.clone());
        confirmation_preedit = Some(text.clone());
        confirming_preedit_with_enter = false;
      }
      true
    }
    Event::Ime(ImeEvent::Commit(text)) => {
      let preedit_before_commit = active_preedit
        .clone()
        .or_else(|| confirmation_preedit.clone())
        .filter(|preedit| !preedit.is_empty());
      let is_candidate_confirmation =
        preedit_before_commit.is_some() && strip_one_logical_trailing_newline(text);

      if is_candidate_confirmation {
        if text.is_empty()
          && let Some(preedit) = preedit_before_commit
        {
          *text = preedit;
        }
        active_preedit = None;
        confirmation_preedit = None;
        confirming_preedit_with_enter = false;
        consume_following_confirm_enter = true;
        true
      } else if is_single_logical_newline(text) {
        *event = synthetic_shift_enter_event();
        active_preedit = None;
        confirmation_preedit = None;
        confirming_preedit_with_enter = false;
        true
      } else {
        active_preedit = None;
        confirmation_preedit = None;
        confirming_preedit_with_enter = false;
        true
      }
    }
    Event::Key { key: Key::Enter, pressed: true, modifiers, .. } => {
      if is_unmodified_enter(modifiers) && consume_following_confirm_enter {
        consume_following_confirm_enter = false;
        false
      } else if is_unmodified_enter(modifiers)
        && active_preedit.as_ref().is_some_and(|text| !text.is_empty())
      {
        confirming_preedit_with_enter = true;
        false
      } else if modifiers.shift || modifiers.mac_cmd || modifiers.command {
        *modifiers = Modifiers::SHIFT;
        true
      } else {
        submit_after_widget = true;
        false
      }
    }
    Event::WindowFocused(false) => {
      active_preedit = None;
      confirmation_preedit = None;
      confirming_preedit_with_enter = false;
      consume_following_confirm_enter = false;
      true
    }
    _ => true,
  });

  ime.preedit = active_preedit;
  ime.confirming_preedit_with_enter =
    confirming_preedit_with_enter && ime.preedit.as_ref().is_some_and(|text| !text.is_empty());
  submit_after_widget
}

fn tool_for_key(key: Key) -> Option<EditorTool> {
  match key {
    Key::Num1 => Some(EditorTool::Select),
    Key::Num2 => Some(EditorTool::Rectangle),
    Key::Num3 => Some(EditorTool::Arrow),
    Key::Num4 => Some(EditorTool::Text),
    Key::Num5 => Some(EditorTool::Stroke),
    Key::Num6 => Some(EditorTool::Sequence),
    _ => None,
  }
}

fn style_for_element(element: &Element) -> ToolStyle {
  match &element.payload {
    ElementPayload::Stroke(payload) => ToolStyle {
      color_rgba: payload.stroke_style.color_rgba,
      width_px: payload.stroke_style.width_px,
      font_size_px: 24.0,
      hardness: payload.hardness,
    },
    ElementPayload::Arrow(payload) => ToolStyle {
      color_rgba: payload.stroke_style.color_rgba,
      width_px: payload.stroke_style.width_px,
      font_size_px: payload.label.text_style.font_size_px,
      hardness: 1.0,
    },
    ElementPayload::Rectangle(payload) => ToolStyle {
      color_rgba: payload.stroke_style.color_rgba,
      width_px: payload.stroke_style.width_px,
      font_size_px: payload.label.text_style.font_size_px,
      hardness: 1.0,
    },
    ElementPayload::Text(payload) => ToolStyle {
      color_rgba: payload.text_style.color_rgba,
      width_px: 8.0,
      font_size_px: payload.text_style.font_size_px,
      hardness: 1.0,
    },
    ElementPayload::SequenceMarker(payload) => ToolStyle {
      color_rgba: payload.fill_rgba,
      width_px: payload.stroke_style.width_px,
      font_size_px: payload.text_style.font_size_px,
      hardness: 1.0,
    },
  }
}

fn hardness_percent(hardness: f32) -> i32 {
  (hardness * 100.0).round() as i32
}

fn command_action(command: DocumentCommand) -> EditorAction {
  EditorAction::Command(CommandBatch::single(command))
}

fn rectangle_command_action(
  document: &BoardDocument,
  command: DocumentCommand,
  primary_id: ElementId,
) -> EditorAction {
  let batch = rectangle_reflow_batch(document, vec![command.clone()], primary_id, &[])
    .unwrap_or_else(|| CommandBatch::single(command));
  EditorAction::Command(batch)
}

pub(crate) fn rectangle_reflow_batch(
  document: &BoardDocument,
  commands: Vec<DocumentCommand>,
  primary_id: ElementId,
  seed_ids: &[ElementId],
) -> Option<CommandBatch> {
  let base_batch = CommandBatch::new(commands.clone()).ok()?;
  let mut staged = document.clone();
  base_batch.clone().apply(&mut staged).ok()?;

  let before = RectangleLabelScene::from_document(document);
  let after = RectangleLabelScene::from_document(&staged);
  let mut solutions = solve_rectangle_label_reflow(&before, &after, primary_id, seed_ids).ok()?;
  solutions.sort_by(|left, right| {
    let left_element = staged.element(left.element_id);
    let right_element = staged.element(right.element_id);
    right_element
      .map(|element| element.z_index)
      .cmp(&left_element.map(|element| element.z_index))
      .then_with(|| left.element_id.as_uuid().as_u128().cmp(&right.element_id.as_uuid().as_u128()))
  });

  let mut reflowed_commands = commands;
  for solution in solutions {
    let Some(element) = staged.element(solution.element_id) else {
      continue;
    };
    let ElementPayload::Rectangle(payload) = &element.payload else {
      continue;
    };
    if payload.preferred_label_anchor == solution.preferred_anchor
      && payload.label_anchor == solution.actual_anchor
    {
      continue;
    }
    if document.element(solution.element_id).is_none()
      && update_added_rectangle_solution(&mut reflowed_commands, solution, document.canvas_size_px)
    {
      continue;
    }
    reflowed_commands.push(DocumentCommand::SetRectangleLabelPlacement {
      element_id: solution.element_id,
      preferred_anchor: solution.preferred_anchor,
      actual_anchor: solution.actual_anchor,
    });
  }

  CommandBatch::new(reflowed_commands).ok()
}

fn update_added_rectangle_solution(
  commands: &mut [DocumentCommand],
  solution: RectangleLabelSolution,
  canvas_size_px: SizePx,
) -> bool {
  for command in commands {
    let DocumentCommand::AddElement { element } = command else {
      continue;
    };
    if element.element_id != solution.element_id {
      continue;
    }
    let ElementPayload::Rectangle(payload) = &mut element.payload else {
      return false;
    };
    payload.preferred_label_anchor = solution.preferred_anchor;
    payload.label_anchor = solution.actual_anchor;
    return element.refresh_bounds(canvas_size_px).is_ok();
  }
  false
}

fn pending_element_label_text(
  actions: &[EditorAction],
  element_id: ElementId,
) -> Option<Option<String>> {
  actions.iter().rev().find_map(|action| {
    let EditorAction::Command(batch) = action else {
      return None;
    };
    batch.commands().iter().rev().find_map(|command| match command {
      DocumentCommand::UpdateElementLabel { element_id: updated_id, text }
        if *updated_id == element_id =>
      {
        Some(text.clone())
      }
      _ => None,
    })
  })
}

fn preview_set_after_batch(document: &BoardDocument, batch: &CommandBatch) -> ElementPreviewSet {
  let mut staged = document.clone();
  if batch.clone().apply(&mut staged).is_err() {
    return ElementPreviewSet::default();
  }
  ElementPreviewSet {
    elements: staged
      .elements
      .iter()
      .filter(|element| {
        document
          .element(element.element_id)
          .is_none_or(|before| !elements_equivalent_for_preview(before, element))
      })
      .cloned()
      .collect(),
  }
}

fn elements_equivalent_for_preview(before: &Element, after: &Element) -> bool {
  if before == after {
    return true;
  }
  if before.element_id != after.element_id || before.z_index != after.z_index {
    return false;
  }
  let (ElementPayload::Rectangle(before_payload), ElementPayload::Rectangle(after_payload)) =
    (&before.payload, &after.payload)
  else {
    return false;
  };
  before_payload.start_px == after_payload.start_px
    && before_payload.end_px == after_payload.end_px
    && before_payload.stroke_style == after_payload.stroke_style
    && before_payload.fill_rgba == after_payload.fill_rgba
    && before_payload.label == after_payload.label
    && rectangle_label_anchor_equivalent_for_preview(
      before_payload.preferred_label_anchor,
      after_payload.preferred_label_anchor,
    )
    && rectangle_label_anchor_equivalent_for_preview(
      before_payload.label_anchor,
      after_payload.label_anchor,
    )
    && rect_equivalent_for_preview(before.bounds_px, after.bounds_px)
}

fn rectangle_label_anchor_equivalent_for_preview(
  before: RectangleLabelAnchor,
  after: RectangleLabelAnchor,
) -> bool {
  before.edge == after.edge
    && before.side == after.side
    && (before.position - after.position).abs() <= 0.001
}

fn rect_equivalent_for_preview(before: RectPx, after: RectPx) -> bool {
  before.min.distance_to(after.min) <= 0.001 && before.max.distance_to(after.max) <= 0.001
}

fn contains(rect: RectPx, point: PointPx) -> bool {
  point.x_px >= rect.min.x_px
    && point.y_px >= rect.min.y_px
    && point.x_px <= rect.max.x_px
    && point.y_px <= rect.max.y_px
}

fn distance_to_segment(point: PointPx, start: PointPx, end: PointPx) -> f32 {
  let dx = end.x_px - start.x_px;
  let dy = end.y_px - start.y_px;
  let length_squared = dx * dx + dy * dy;
  if length_squared <= f32::EPSILON {
    return point.distance_to(start);
  }
  let t = (((point.x_px - start.x_px) * dx + (point.y_px - start.y_px) * dy) / length_squared)
    .clamp(0.0, 1.0);
  point.distance_to(PointPx::new(start.x_px + t * dx, start.y_px + t * dy))
}

fn paint_handle(painter: &egui::Painter, position: Pos2) {
  painter.circle_stroke(position, HANDLE_VISUAL_RADIUS_PT + 1.0, Stroke::new(1.5, Color32::BLACK));
  painter.circle_stroke(position, HANDLE_VISUAL_RADIUS_PT, Stroke::new(1.5, Color32::WHITE));
}

fn paint_text_editing_frame(
  painter: &egui::Painter,
  transform: CanvasTransform,
  bounds_px: RectPx,
) {
  let rect = transform.document_rect_to_egui(bounds_px).expand(3.0);
  painter.rect_stroke(
    rect,
    egui::CornerRadius::ZERO,
    Stroke::new(1.0, Color32::WHITE),
    StrokeKind::Outside,
  );
  paint_text_move_handle(painter, text_move_handle_position(rect));
}

fn hit_text_editing_frame(bounds_px: RectPx, point: PointPx, transform: CanvasTransform) -> bool {
  let screen_point = transform.document_to_egui(point);
  let rect = transform.document_rect_to_egui(bounds_px).expand(3.0);
  if text_move_handle_position(rect).distance(screen_point) <= TEXT_MOVE_HANDLE_HIT_RADIUS_PT {
    return true;
  }
  rect.expand(HANDLE_HIT_RADIUS_PT * 0.5).contains(screen_point)
    && !rect.shrink(HANDLE_HIT_RADIUS_PT * 0.5).contains(screen_point)
}

fn text_move_handle_position(rect: Rect) -> Pos2 {
  rect.min - egui::vec2(TEXT_MOVE_HANDLE_OFFSET_PT, TEXT_MOVE_HANDLE_OFFSET_PT)
}

fn paint_text_move_handle(painter: &egui::Painter, position: Pos2) {
  painter.circle_stroke(
    position,
    TEXT_MOVE_HANDLE_VISUAL_RADIUS_PT + 1.0,
    Stroke::new(1.5, Color32::BLACK),
  );
  painter.circle_stroke(
    position,
    TEXT_MOVE_HANDLE_VISUAL_RADIUS_PT,
    Stroke::new(1.5, Color32::WHITE),
  );
}

fn color32(color: ColorRgba) -> Color32 {
  Color32::from_rgba_unmultiplied(color.red, color.green, color.blue, color.alpha)
}

fn mvp_colors() -> [ColorRgba; 6] {
  [
    ColorRgba::RED,
    ColorRgba::YELLOW,
    ColorRgba::GREEN,
    ColorRgba::BLUE,
    ColorRgba::WHITE,
    ColorRgba::BLACK,
  ]
}

fn floating_panel_frame(style: &egui::Style) -> egui::Frame {
  egui::Frame::popup(style).inner_margin(egui::Margin::same(FLOATING_PANEL_MARGIN_PT))
}

fn toolbar_area(screen: Rect, recenter: bool) -> egui::Area {
  let area = egui::Area::new(Id::new("rs-board-toolbar"))
    .order(FLOATING_PANEL_ORDER)
    .pivot(Align2::CENTER_BOTTOM)
    .default_pos(screen.center_bottom())
    .movable(true)
    .constrain_to(screen);
  if recenter { area.current_pos(screen.center_bottom()) } else { area }
}

fn toolbar_drag_handle(ui: &mut egui::Ui) -> Rect {
  let (rect, response) = ui.allocate_exact_size(
    egui::vec2(TOOLBAR_DRAG_HANDLE_WIDTH_PT, FLOATING_CONTROL_HEIGHT_PT),
    Sense::hover(),
  );
  let response = response.on_hover_cursor(CursorIcon::Grab).on_hover_text("拖动工具栏");
  let color = ui.style().interact(&response).fg_stroke.color;
  let center = rect.center();
  for x in [-3.0, 3.0] {
    for y in [-6.0, 0.0, 6.0] {
      ui.painter().circle_filled(
        center + egui::vec2(x, y),
        TOOLBAR_DRAG_HANDLE_DOT_RADIUS_PT,
        color,
      );
    }
  }
  rect
}

fn set_floating_control_style(ui: &mut egui::Ui) {
  let swatch_height =
    ui.ctx().fonts_mut(|fonts| fonts.row_height(&FontId::proportional(COLOR_SWATCH_FONT_SIZE_PT)));
  let vertical_padding = ((FLOATING_CONTROL_HEIGHT_PT - swatch_height) / 2.0).max(0.0);
  let spacing = ui.spacing_mut();
  spacing.button_padding.y = spacing.button_padding.y.min(vertical_padding);
  spacing.interact_size.y = FLOATING_CONTROL_HEIGHT_PT;
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;

  use chrono::{TimeZone, Utc};
  use common::{CapturedDisplay, DocumentId, GlobalBoundsPx};
  use uuid::Uuid;

  use super::*;

  fn document() -> BoardDocument {
    document_with_size(SizePx::new(400, 200))
  }

  fn document_with_size(canvas_size_px: SizePx) -> BoardDocument {
    BoardDocument::new_capture(
      DocumentId::from_uuid(Uuid::nil()),
      canvas_size_px,
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx {
          x_px: 0,
          y_px: 0,
          width_px: canvas_size_px.width_px,
          height_px: canvas_size_px.height_px,
        },
        scale_factor: 1.0,
      },
      Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).unwrap(),
    )
    .unwrap()
  }

  #[test]
  fn brush_hardness_is_available_in_the_toolbar_and_option_panel() {
    let context = egui::Context::default();
    let document = document();
    let history = CommandHistory::new();
    let mut controller = EditorController::default();
    assert!(!controller.shows_brush_hardness(&document));
    controller.set_active_tool(EditorTool::Stroke);
    assert!(controller.shows_brush_hardness(&document));
    let anchor = Pos2::new(300.0, 160.0);
    let input = raw_input(
      vec![Event::ModifiersChanged(Modifiers::ALT), Event::PointerMoved(anchor)],
      egui::vec2(1000.0, 500.0),
    );

    let output = context.run_ui(input, |ui| {
      assert!(controller.show(ui, &document, &history, None).is_empty());
    });
    output.drop_without_applying_deltas();
    assert_eq!(controller.option_panel_anchor, Some(anchor));
  }

  #[test]
  fn brush_hardness_selection_is_used_for_new_strokes_and_selected_strokes() {
    let mut document = document();
    let mut controller = EditorController::default();
    controller.set_active_tool(EditorTool::Stroke);
    let mut actions = Vec::new();
    controller.apply_style_change(
      &document,
      StyleChange { hardness: Some(0.5), ..StyleChange::default() },
      &mut actions,
    );
    assert!(actions.is_empty());
    let new_stroke = controller.make_stroke(&document, &[PointPx::new(80.0, 60.0)]).unwrap();
    let ElementPayload::Stroke(payload) = &new_stroke.payload else {
      unreachable!();
    };
    assert_eq!(payload.hardness, 0.5);

    let element_id = new_stroke.element_id;
    document.elements.push(new_stroke);
    controller.set_selected_element_id(Some(element_id));
    controller.apply_style_change(
      &document,
      StyleChange { hardness: Some(0.0), ..StyleChange::default() },
      &mut actions,
    );
    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected a style command, got {actions:?}");
    };
    batch.clone().apply(&mut document).unwrap();
    let ElementPayload::Stroke(payload) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(payload.hardness, 0.0);
  }

  #[test]
  fn default_tool_styles_match_the_settings_plan() {
    let controller = EditorController::default();

    assert_eq!(
      controller.tool_style(EditorTool::Rectangle),
      ToolStyle::new(ColorRgba::RED, 8.0, 36.0, 1.0)
    );
    assert_eq!(
      controller.tool_style(EditorTool::Arrow),
      ToolStyle::new(ColorRgba::RED, 8.0, 24.0, 1.0)
    );
    assert_eq!(
      controller.tool_style(EditorTool::Text),
      ToolStyle::new(ColorRgba::RED, 8.0, 36.0, 1.0)
    );
    assert_eq!(
      controller.tool_style(EditorTool::Stroke),
      ToolStyle::new(ColorRgba::RED, 8.0, 24.0, 0.0)
    );
    assert_eq!(
      controller.tool_style(EditorTool::Sequence),
      ToolStyle::new(ColorRgba::RED, 8.0, 24.0, 1.0)
    );
  }

  #[test]
  fn injected_tool_styles_are_used_for_new_elements() {
    let document = document();
    let mut styles = default_tool_styles();
    styles[EditorTool::Rectangle.index()] = ToolStyle::new(ColorRgba::BLUE, 12.0, 48.0, 1.0);
    styles[EditorTool::Stroke.index()] = ToolStyle::new(ColorRgba::GREEN, 4.0, 24.0, 0.5);
    let controller =
      EditorController::with_styles(EditorTool::Rectangle, styles, ColorRgba::YELLOW);

    let rectangle =
      controller.rectangle_payload(PointPx::new(20.0, 20.0), PointPx::new(180.0, 120.0)).unwrap();
    assert_eq!(rectangle.stroke_style.color_rgba, ColorRgba::YELLOW);
    assert_eq!(rectangle.stroke_style.width_px, 12.0);
    assert_eq!(rectangle.label.text_style.font_size_px, 48.0);

    let arrow =
      controller.arrow_payload(PointPx::new(20.0, 20.0), PointPx::new(180.0, 120.0)).unwrap();
    assert_eq!(arrow.stroke_style.color_rgba, ColorRgba::YELLOW);

    let stroke = controller.make_stroke(&document, &[PointPx::new(80.0, 60.0)]).unwrap();
    let ElementPayload::Stroke(payload) = stroke.payload else {
      unreachable!();
    };
    assert_eq!(payload.stroke_style.color_rgba, ColorRgba::YELLOW);
    assert_eq!(payload.stroke_style.width_px, 4.0);
    assert_eq!(payload.hardness, 0.5);

    for tool in [
      EditorTool::Rectangle,
      EditorTool::Arrow,
      EditorTool::Text,
      EditorTool::Stroke,
      EditorTool::Sequence,
    ] {
      assert_eq!(controller.tool_style(tool).color_rgba, ColorRgba::YELLOW);
    }

    let text_style = controller.tool_style(EditorTool::Text);
    assert_eq!(text_style.color_rgba, ColorRgba::YELLOW);

    let mut sequence_actions = Vec::new();
    let mut sequence_controller = controller.clone();
    sequence_controller.insert_sequence(
      &document,
      PointPx::new(200.0, 100.0),
      &mut sequence_actions,
    );
    let [EditorAction::Command(batch)] = sequence_actions.as_slice() else {
      panic!("expected one sequence command, got {sequence_actions:?}");
    };
    assert!(matches!(
      batch.commands(),
      [
        DocumentCommand::AddElement {
          element: Element {
            payload: ElementPayload::SequenceMarker(payload),
            ..
          },
        },
        DocumentCommand::SetNextSequenceNumber { .. },
      ] if payload.fill_rgba == ColorRgba::YELLOW
    ));
  }

  #[test]
  fn active_tool_color_selection_updates_global_color_and_emits_persistence_action() {
    let document = document();
    let mut controller = EditorController::new(EditorTool::Rectangle);
    let mut actions = Vec::new();

    controller.apply_style_change(
      &document,
      StyleChange { color_rgba: Some(ColorRgba::BLUE), ..StyleChange::default() },
      &mut actions,
    );

    assert_eq!(actions, vec![EditorAction::GlobalColorChanged { color_rgba: ColorRgba::BLUE }]);
    assert_eq!(controller.global_color(), ColorRgba::BLUE);
    assert_eq!(controller.tool_style(EditorTool::Rectangle).color_rgba, ColorRgba::BLUE);
    assert_eq!(controller.tool_style(EditorTool::Stroke).color_rgba, ColorRgba::BLUE);
  }

  #[test]
  fn selected_element_color_selection_changes_element_and_global_color() {
    let mut document = document();
    let element = text_element(&document, PointPx::new(40.0, 40.0), "text", 120.0);
    let element_id = element.element_id;
    document.elements.push(element);
    document.validate().unwrap();
    let mut controller = EditorController::new(EditorTool::Select);
    controller.selected_element_id = Some(element_id);
    let mut actions = Vec::new();

    controller.apply_style_change(
      &document,
      StyleChange { color_rgba: Some(ColorRgba::GREEN), ..StyleChange::default() },
      &mut actions,
    );

    assert!(matches!(
      actions.as_slice(),
      [
        EditorAction::GlobalColorChanged { color_rgba: ColorRgba::GREEN },
        EditorAction::Command(batch),
      ] if matches!(
        batch.commands(),
        [DocumentCommand::ChangeElementStyle { element_id: changed_id, change }]
          if *changed_id == element_id && change.color_rgba == Some(ColorRgba::GREEN)
      )
    ));
    assert_eq!(controller.global_color(), ColorRgba::GREEN);
    assert_eq!(controller.tool_style(EditorTool::Text).color_rgba, ColorRgba::GREEN);
  }

  #[test]
  fn selecting_the_elements_existing_color_only_updates_global_color() {
    let mut document = document();
    let element = text_element(&document, PointPx::new(40.0, 40.0), "text", 120.0);
    let element_id = element.element_id;
    document.elements.push(element);
    let mut controller = EditorController::new(EditorTool::Select);
    controller.selected_element_id = Some(element_id);
    let mut actions = Vec::new();

    controller.apply_style_change(
      &document,
      StyleChange { color_rgba: Some(ColorRgba::WHITE), ..StyleChange::default() },
      &mut actions,
    );

    assert_eq!(actions, vec![EditorAction::GlobalColorChanged { color_rgba: ColorRgba::WHITE }]);
    assert_eq!(controller.global_color(), ColorRgba::WHITE);
  }

  #[test]
  fn select_tool_without_an_element_can_update_global_color() {
    let document = document();
    let mut controller = EditorController::new(EditorTool::Select);
    let mut actions = Vec::new();

    controller.apply_style_change(
      &document,
      StyleChange { color_rgba: Some(ColorRgba::BLUE), ..StyleChange::default() },
      &mut actions,
    );

    assert_eq!(actions, vec![EditorAction::GlobalColorChanged { color_rgba: ColorRgba::BLUE }]);
    controller.set_active_tool(EditorTool::Arrow);
    assert_eq!(controller.tool_style(EditorTool::Arrow).color_rgba, ColorRgba::BLUE);
  }

  #[test]
  fn selecting_the_current_global_color_still_emits_a_persistence_action() {
    let document = document();
    let mut controller = EditorController::new(EditorTool::Rectangle);
    let mut actions = Vec::new();

    controller.apply_style_change(
      &document,
      StyleChange { color_rgba: Some(ColorRgba::RED), ..StyleChange::default() },
      &mut actions,
    );

    assert_eq!(actions, vec![EditorAction::GlobalColorChanged { color_rgba: ColorRgba::RED }]);
    assert_eq!(controller.global_color(), ColorRgba::RED);
  }

  #[test]
  fn selected_element_color_is_only_a_temporary_display_override() {
    let mut document = document();
    let element = text_element(&document, PointPx::new(40.0, 40.0), "text", 120.0);
    let element_id = element.element_id;
    document.elements.push(element);
    let mut controller =
      EditorController::with_styles(EditorTool::Text, default_tool_styles(), ColorRgba::GREEN);

    controller.set_selected_element_id(Some(element_id));
    assert_eq!(controller.displayed_style(&document).color_rgba, ColorRgba::WHITE);
    assert_eq!(controller.global_color(), ColorRgba::GREEN);

    controller.set_selected_element_id(None);
    assert_eq!(controller.displayed_style(&document).color_rgba, ColorRgba::GREEN);
    assert_eq!(controller.global_color(), ColorRgba::GREEN);
  }

  #[test]
  fn undoing_an_element_color_change_does_not_restore_global_color() {
    let mut document = document();
    let element = text_element(&document, PointPx::new(40.0, 40.0), "text", 120.0);
    let element_id = element.element_id;
    document.elements.push(element);
    let mut controller = EditorController::new(EditorTool::Select);
    controller.set_selected_element_id(Some(element_id));
    let mut actions = Vec::new();

    controller.apply_style_change(
      &document,
      StyleChange { color_rgba: Some(ColorRgba::BLUE), ..StyleChange::default() },
      &mut actions,
    );
    let [EditorAction::GlobalColorChanged { .. }, EditorAction::Command(batch)] =
      actions.as_slice()
    else {
      panic!("expected a global color update followed by an element command");
    };
    let mut history = CommandHistory::new();
    history.execute_batch(&mut document, batch.clone()).unwrap();
    assert!(history.undo(&mut document).unwrap());

    assert_eq!(
      style_for_element(document.element(element_id).unwrap()).color_rgba,
      ColorRgba::WHITE
    );
    assert_eq!(controller.global_color(), ColorRgba::BLUE);
  }

  #[test]
  fn committing_existing_text_after_color_change_keeps_the_new_color() {
    let mut document = document();
    let element = text_element(&document, PointPx::new(40.0, 40.0), "old", 120.0);
    let element_id = element.element_id;
    document.elements.push(element);
    let mut controller = EditorController::new(EditorTool::Select);
    controller.set_selected_element_id(Some(element_id));
    controller.start_editing_existing(&document, element_id, false, &mut Vec::new());
    controller.text_editing.as_mut().unwrap().buffer = "new".to_owned();
    let mut actions = Vec::new();

    controller.apply_style_change(
      &document,
      StyleChange { color_rgba: Some(ColorRgba::BLUE), ..StyleChange::default() },
      &mut actions,
    );
    controller.commit_text(&document, &mut actions);

    assert!(matches!(
      actions.first(),
      Some(EditorAction::GlobalColorChanged { color_rgba: ColorRgba::BLUE })
    ));
    let mut history = CommandHistory::new();
    for action in actions {
      if let EditorAction::Command(batch) = action {
        history.execute_batch(&mut document, batch).unwrap();
      }
    }
    let ElementPayload::Text(payload) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(payload.text, "new");
    assert_eq!(payload.text_style.color_rgba, ColorRgba::BLUE);
    assert_eq!(controller.global_color(), ColorRgba::BLUE);
  }

  fn text_element(
    document: &BoardDocument,
    anchor_px: PointPx,
    text: impl Into<String>,
    box_width_px: f32,
  ) -> Element {
    Element::new(
      ElementId::new(),
      document.elements.len() as i64,
      ElementPayload::Text(TextPayload {
        anchor_px,
        text: text.into(),
        box_width_px,
        text_style: TextStyle::mvp(ColorRgba::WHITE, 24.0).unwrap(),
      }),
      document.canvas_size_px,
    )
    .unwrap()
  }

  fn raw_input(events: Vec<Event>, screen_size: egui::Vec2) -> egui::RawInput {
    egui::RawInput {
      screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen_size)),
      events,
      ..Default::default()
    }
  }

  fn run_editor_frame(
    context: &egui::Context,
    controller: &mut EditorController,
    document: &BoardDocument,
    history: &CommandHistory,
    events: Vec<Event>,
  ) -> Vec<EditorAction> {
    let mut actions = None;
    context
      .run_ui(raw_input(events, egui::vec2(800.0, 400.0)), |ui| {
        actions = Some(controller.show(ui, document, history, None));
      })
      .drop_without_applying_deltas();
    actions.expect("editor frame ran")
  }

  fn run_stylus_editor_frame(
    context: &egui::Context,
    controller: &mut EditorController,
    document: &BoardDocument,
    history: &CommandHistory,
    events: Vec<Event>,
  ) -> Vec<EditorAction> {
    let mut input = raw_input(events, egui::vec2(800.0, 400.0));
    controller.capture_stylus_input(&mut input);
    let mut actions = None;
    context
      .run_ui(input, |ui| {
        actions = Some(controller.show(ui, document, history, None));
      })
      .drop_without_applying_deltas();
    actions.expect("editor frame ran")
  }

  fn tab_event(modifiers: Modifiers) -> Event {
    Event::Key {
      key: Key::Tab,
      physical_key: Some(Key::Tab),
      pressed: true,
      repeat: false,
      modifiers,
    }
  }

  fn run_tab_editor_frame(
    context: &egui::Context,
    controller: &mut EditorController,
    document: &BoardDocument,
    history: &CommandHistory,
    events: Vec<Event>,
  ) -> Vec<EditorAction> {
    let mut actions = None;
    context
      .run_ui(raw_input(events, egui::vec2(800.0, 400.0)), |ui| {
        controller.capture_tab_switch_input_state(ui.ctx());
        actions = Some(controller.show(ui, document, history, None));
      })
      .drop_without_applying_deltas();
    actions.expect("editor frame ran")
  }

  fn touch_event(phase: TouchPhase, position: Pos2, pressure: f32) -> Event {
    Event::Touch {
      device_id: TouchDeviceId(7),
      id: TouchId(11),
      phase,
      pos: position,
      force: Some(pressure),
    }
  }

  fn primary_button(position: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
      pos: position,
      button: egui::PointerButton::Primary,
      pressed,
      modifiers: Modifiers::NONE,
    }
  }

  fn resize_rectangle_command(
    commands: &[DocumentCommand],
    expected_id: ElementId,
  ) -> Option<(PointPx, PointPx)> {
    commands.iter().find_map(|command| match command {
      DocumentCommand::ResizeRectangle { element_id, start_px, end_px }
        if *element_id == expected_id =>
      {
        Some((*start_px, *end_px))
      }
      _ => None,
    })
  }

  fn rectangle_label_placement_command(
    commands: &[DocumentCommand],
    expected_id: ElementId,
  ) -> Option<(RectangleLabelAnchor, RectangleLabelAnchor)> {
    commands.iter().find_map(|command| match command {
      DocumentCommand::SetRectangleLabelPlacement {
        element_id,
        preferred_anchor,
        actual_anchor,
      } if *element_id == expected_id => Some((*preferred_anchor, *actual_anchor)),
      _ => None,
    })
  }

  fn rectangle(document: &BoardDocument, z_index: i64, start: PointPx, end: PointPx) -> Element {
    let style = StrokeStyle::default();
    Element::new(
      ElementId::new(),
      z_index,
      ElementPayload::Rectangle(RectanglePayload {
        start_px: start,
        end_px: end,
        stroke_style: style.clone(),
        fill_rgba: None,
        label: ElementLabel {
          text: Some(DEFAULT_RECTANGLE_LABEL.to_owned()),
          max_width_px: 200.0,
          padding_px: 8.0,
          anchor_offset_px: 8.0,
          text_style: TextStyle::mvp(style.color_rgba.contrasting_text(), 24.0).unwrap(),
        },
        label_anchor: RectangleLabelAnchor::new(
          RectangleLabelEdge::Top,
          RectangleLabelSide::Outside,
          0.0,
        ),
        preferred_label_anchor: RectangleLabelAnchor::new(
          RectangleLabelEdge::Top,
          RectangleLabelSide::Outside,
          0.0,
        ),
      }),
      document.canvas_size_px,
    )
    .unwrap()
  }

  fn arrow(document: &BoardDocument, z_index: i64, start: PointPx, end: PointPx) -> Element {
    let stroke_style = StrokeStyle::default();
    let head = ArrowHead::for_stroke_width(stroke_style.width_px).unwrap();
    Element::new(
      ElementId::new(),
      z_index,
      ElementPayload::Arrow(ArrowPayload {
        start_px: start,
        end_px: end,
        label: ElementLabel {
          text: None,
          max_width_px: 420.0,
          padding_px: 8.0,
          anchor_offset_px: 8.0,
          text_style: TextStyle::mvp(stroke_style.color_rgba.contrasting_text(), 24.0).unwrap(),
        },
        stroke_style,
        head,
      }),
      document.canvas_size_px,
    )
    .unwrap()
  }

  fn stroke(document: &BoardDocument, z_index: i64) -> Element {
    let payload = StrokePayload::from_raw_points(
      &[PointPx::new(60.0, 70.0), PointPx::new(150.0, 95.0)],
      StrokeStyle::default(),
    )
    .unwrap();
    Element::new(
      ElementId::new(),
      z_index,
      ElementPayload::Stroke(payload),
      document.canvas_size_px,
    )
    .unwrap()
  }

  fn sequence_marker(document: &BoardDocument, z_index: i64) -> Element {
    let fill_rgba = ColorRgba::BLUE;
    Element::new(
      ElementId::new(),
      z_index,
      ElementPayload::SequenceMarker(SequenceMarkerPayload {
        center_px: PointPx::new(150.0, 100.0),
        number: 1,
        radius_px: 18.0,
        pill_width_px: 44.0,
        fill_rgba,
        stroke_style: StrokeStyle::mvp(fill_rgba, 8.0).unwrap(),
        text_style: TextStyle::mvp(fill_rgba.contrasting_text(), 24.0).unwrap(),
      }),
      document.canvas_size_px,
    )
    .unwrap()
  }

  fn paint_canvas_output(
    controller: &EditorController,
    document: &BoardDocument,
  ) -> egui::FullOutput {
    paint_canvas_output_with_context(controller, document).1
  }

  fn paint_canvas_output_with_context(
    controller: &EditorController,
    document: &BoardDocument,
  ) -> (egui::Context, egui::FullOutput) {
    let context = egui::Context::default();
    let viewport = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 400.0));
    let transform = CanvasTransform::fit(document.canvas_size_px, viewport).unwrap();
    let output = context.run_ui(raw_input(Vec::new(), viewport.size()), |ui| {
      controller.paint_document_for_editing(ui.painter(), transform, document);
      controller.paint_interaction(ui.painter(), transform, document);
    });
    (context, output)
  }

  fn paint_selection_output(
    controller: &EditorController,
    document: &BoardDocument,
  ) -> egui::FullOutput {
    let context = egui::Context::default();
    let viewport = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 400.0));
    let transform = CanvasTransform::fit(document.canvas_size_px, viewport).unwrap();
    context.run_ui(raw_input(Vec::new(), viewport.size()), |ui| {
      controller.paint_selection(ui.painter(), transform, document);
    })
  }

  fn painted_shape_rects(output: &egui::FullOutput) -> Vec<Rect> {
    output.shapes.iter().map(|shape| shape.shape.visual_bounding_rect()).collect()
  }

  fn assert_point_close(actual: PointPx, expected: PointPx) {
    assert!(
      (actual.x_px - expected.x_px).abs() < 0.001,
      "actual={actual:?}, expected={expected:?}"
    );
    assert!(
      (actual.y_px - expected.y_px).abs() < 0.001,
      "actual={actual:?}, expected={expected:?}"
    );
  }

  fn assert_paint_shapes_are_finite(output: &egui::FullOutput) {
    assert!(!output.shapes.is_empty(), "expected the interaction preview to paint");
    for shape in &output.shapes {
      let bounds = shape.shape.visual_bounding_rect();
      assert!(
        bounds.min.x.is_finite()
          && bounds.min.y.is_finite()
          && bounds.max.x.is_finite()
          && bounds.max.y.is_finite(),
        "non-finite painted bounds: {bounds:?}"
      );
    }
  }

  fn assert_tessellated_meshes_are_finite(context: &egui::Context, output: &egui::FullOutput) {
    for clipped in context.tessellate(output.shapes.clone(), output.pixels_per_point) {
      let egui::epaint::Primitive::Mesh(mesh) = clipped.primitive else {
        continue;
      };
      for vertex in mesh.vertices {
        assert!(
          vertex.pos.x.is_finite()
            && vertex.pos.y.is_finite()
            && vertex.uv.x.is_finite()
            && vertex.uv.y.is_finite(),
          "non-finite tessellated vertex: {vertex:?}"
        );
      }
    }
  }

  #[test]
  fn stroke_release_preserves_the_preview_points_and_opacity() {
    let document = document();
    let points = vec![
      PointPx::new(40.0, 40.0),
      PointPx::new(40.2, 40.1),
      PointPx::new(52.0, 47.0),
      PointPx::new(61.0, 43.0),
    ];
    let interaction = PointerInteraction::Draw {
      element_id: ElementId::new(),
      tool: EditorTool::Stroke,
      start: points[0],
      current: *points.last().unwrap(),
      stroke_points: points.iter().copied().map(StrokePoint::new).collect(),
    };
    let mut controller = EditorController {
      tool: EditorTool::Stroke,
      interaction: Some(interaction),
      ..Default::default()
    };
    controller.styles[EditorTool::Stroke.index()].hardness = 1.0;

    let preview = paint_canvas_output(&controller, &document);
    let preview_strokes = preview
      .shapes
      .iter()
      .filter_map(|shape| match &shape.shape {
        egui::Shape::LineSegment { stroke, .. } => Some(stroke),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(preview_strokes.len(), points.len() - 1);
    assert!(preview_strokes.iter().all(|stroke| stroke.color.a() == u8::MAX));
    preview.drop_without_applying_deltas();

    let mut actions = Vec::new();
    controller.finish_pointer_interaction(&document, &mut actions);
    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one stroke command, got {actions:?}");
    };
    let [DocumentCommand::AddElement { element }] = batch.commands() else {
      panic!("expected AddElement");
    };
    let ElementPayload::Stroke(payload) = &element.payload else {
      panic!("expected a stroke element");
    };
    assert_eq!(payload.points.iter().map(|point| point.point()).collect::<Vec<_>>(), points);
  }

  #[test]
  fn clicking_with_the_stroke_tool_adds_a_dot() {
    let context = egui::Context::default();
    let document = document();
    let history = CommandHistory::new();
    let mut controller = EditorController::default();
    controller.set_active_tool(EditorTool::Stroke);
    let pointer = Pos2::new(100.0, 100.0);

    let warmup = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![Event::PointerMoved(pointer)],
    );
    assert!(warmup.is_empty());

    let pressed = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![Event::PointerButton {
        pos: pointer,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::default(),
      }],
    );
    assert!(pressed.is_empty());

    let actions = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![Event::PointerButton {
        pos: pointer,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::default(),
      }],
    );
    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one dot command, got {actions:?}");
    };
    let [DocumentCommand::AddElement { element }] = batch.commands() else {
      panic!("expected AddElement");
    };
    let ElementPayload::Stroke(payload) = &element.payload else {
      panic!("expected a stroke element");
    };
    assert_eq!(payload.points.len(), 1);
    let point = payload.points[0].point();
    let element_id = element.element_id;
    let mut changed = document.clone();
    batch.clone().apply(&mut changed).unwrap();
    assert_eq!(hit_test_document(&changed, point, 0.0), Some(element_id));
  }

  #[test]
  fn stylus_release_tapers_the_saved_stroke_even_when_the_end_position_is_unchanged() {
    let context = egui::Context::default();
    let document = document();
    let history = CommandHistory::new();
    let mut controller = EditorController::default();
    controller.set_active_tool(EditorTool::Stroke);
    let start = Pos2::new(100.0, 100.0);
    let middle = Pos2::new(150.0, 100.0);
    let end = Pos2::new(200.0, 100.0);

    assert!(
      run_stylus_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![Event::PointerMoved(start)],
      )
      .is_empty()
    );
    assert!(
      run_stylus_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![
          touch_event(TouchPhase::Start, start, 0.9),
          Event::PointerMoved(start),
          Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
          },
        ],
      )
      .is_empty()
    );
    assert!(
      run_stylus_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![touch_event(TouchPhase::Move, middle, 0.8), Event::PointerMoved(middle)],
      )
      .is_empty()
    );
    assert!(
      matches!(
        controller.interaction,
        Some(PointerInteraction::Draw { tool: EditorTool::Stroke, .. })
      ),
      "stylus movement should start a stroke drag"
    );
    assert!(
      run_stylus_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![touch_event(TouchPhase::Move, end, 0.7), Event::PointerMoved(end)],
      )
      .is_empty()
    );
    let actions = run_stylus_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![
        touch_event(TouchPhase::End, end, 0.0),
        Event::PointerButton {
          pos: end,
          button: egui::PointerButton::Primary,
          pressed: false,
          modifiers: Modifiers::NONE,
        },
        Event::PointerGone,
      ],
    );

    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one pressure stroke command, got {actions:?}");
    };
    let [DocumentCommand::AddElement { element }] = batch.commands() else {
      panic!("expected AddElement");
    };
    let ElementPayload::Stroke(payload) = &element.payload else {
      panic!("expected a stroke element");
    };
    assert!(payload.points.len() >= 3);
    assert_eq!(payload.points.last().unwrap().pressure, 0.0);
    let tail = &payload.points[payload.points.len() - 3..];
    assert!(tail.windows(2).all(|points| points[0].pressure >= points[1].pressure));
    assert!(tail[0].pressure > tail[1].pressure);
  }

  #[test]
  fn stylus_drag_without_a_new_sample_preserves_the_last_pressure() {
    let context = egui::Context::default();
    let document = document();
    let history = CommandHistory::new();
    let mut controller = EditorController::default();
    controller.set_active_tool(EditorTool::Stroke);
    let start = Pos2::new(100.0, 100.0);
    let middle = Pos2::new(150.0, 100.0);

    assert!(
      run_stylus_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![Event::PointerMoved(start)],
      )
      .is_empty()
    );
    assert!(
      run_stylus_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![
          touch_event(TouchPhase::Start, start, 0.4),
          Event::PointerMoved(start),
          Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
          },
        ],
      )
      .is_empty()
    );
    assert!(
      run_stylus_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![touch_event(TouchPhase::Move, middle, 0.4), Event::PointerMoved(middle)],
      )
      .is_empty()
    );

    let point_count = match &controller.interaction {
      Some(PointerInteraction::Draw { tool: EditorTool::Stroke, stroke_points, .. }) => {
        assert_eq!(stroke_points.last().unwrap().pressure, 0.4);
        stroke_points.len()
      }
      interaction => panic!("expected an active pressure stroke, got {interaction:?}"),
    };

    assert!(
      run_stylus_editor_frame(&context, &mut controller, &document, &history, Vec::new())
        .is_empty()
    );

    let Some(PointerInteraction::Draw { tool: EditorTool::Stroke, stroke_points, .. }) =
      &controller.interaction
    else {
      panic!("expected the pressure stroke to remain active");
    };
    assert_eq!(stroke_points.len(), point_count);
    assert_eq!(stroke_points.last().unwrap().pressure, 0.4);
  }

  #[test]
  fn stylus_tap_keeps_contact_pressure_instead_of_disappearing_on_release() {
    let point = PointPx::new(20.0, 20.0);
    let mut points = vec![StrokePoint::with_pressure(point, 0.35).unwrap()];
    append_stylus_sample(
      &mut points,
      StylusSample { phase: TouchPhase::End, point, pressure: 0.0 },
    );
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].pressure, 0.35);
  }

  #[test]
  fn input_state_capture_routes_only_pressure_touch_events() {
    let stylus = touch_event(TouchPhase::Move, Pos2::new(20.0, 30.0), 0.45);
    let finger = Event::Touch {
      device_id: TouchDeviceId(8),
      id: TouchId(12),
      phase: TouchPhase::Move,
      pos: Pos2::new(40.0, 50.0),
      force: None,
    };
    let mut input = egui::InputState::default();
    input.events = vec![stylus.clone(), finger.clone()];
    input.raw.events = vec![stylus, finger.clone()];
    let mut controller = EditorController::default();

    controller.capture_stylus_input_state(&mut input);

    assert_eq!(controller.queued_stylus_events.len(), 1);
    assert_eq!(controller.queued_stylus_events[0].pressure, 0.45);
    assert_eq!(input.events, vec![finger.clone()]);
    assert_eq!(input.raw.events, vec![finger]);
  }

  #[test]
  fn focus_loss_discards_pressure_events_from_the_same_frame() {
    let mut controller = EditorController::default();
    let mut input = raw_input(
      vec![
        touch_event(TouchPhase::Start, Pos2::new(20.0, 30.0), 0.45),
        Event::WindowFocused(false),
      ],
      egui::vec2(100.0, 100.0),
    );

    controller.capture_stylus_input(&mut input);

    assert!(controller.queued_stylus_events.is_empty());
    assert_eq!(input.events, vec![Event::WindowFocused(false)]);
  }

  #[test]
  fn positive_move_can_start_stylus_capture_without_drawing_clamped_outside_moves() {
    let transform = CanvasTransform::fit(
      SizePx::new(100, 100),
      Rect::from_min_size(Pos2::ZERO, egui::vec2(100.0, 100.0)),
    )
    .unwrap();
    let id = StylusId { device_id: TouchDeviceId(7), touch_id: TouchId(11) };
    let inside = Pos2::new(80.0, 40.0);
    let outside = Pos2::new(130.0, 70.0);
    let mut controller = EditorController {
      queued_stylus_events: vec![
        StylusEvent { id, phase: TouchPhase::Move, position: inside, pressure: 0.6 },
        StylusEvent { id, phase: TouchPhase::Move, position: outside, pressure: 0.5 },
        StylusEvent { id, phase: TouchPhase::End, position: outside, pressure: 0.0 },
      ],
      ..Default::default()
    };

    let frame = controller.take_stylus_frame(transform);

    assert!(frame.ended);
    assert_eq!(frame.samples.len(), 2);
    assert_eq!(frame.samples[0].point, PointPx::new(80.0, 40.0));
    assert_eq!(frame.samples[1].point, PointPx::new(80.0, 40.0));
    assert_eq!(frame.samples[1].pressure, 0.0);
    assert!(controller.active_stylus_id.is_none());
  }

  #[test]
  fn release_taper_reduces_pressure_before_a_nearby_tip() {
    let mut points = vec![
      StrokePoint::with_pressure(PointPx::new(10.0, 20.0), 0.8).unwrap(),
      StrokePoint::with_pressure(PointPx::new(36.0, 20.0), 0.8).unwrap(),
      StrokePoint::with_pressure(PointPx::new(40.0, 20.0), 0.0).unwrap(),
    ];

    apply_release_taper(&mut points, 12.0);

    assert_eq!(points[0].pressure, 0.8);
    assert!(points[1].pressure < 0.8);
    assert!(points[1].pressure > 0.0);
    assert_eq!(points[2].pressure, 0.0);
    let penultimate_radius = 12.0 * points[1].pressure / 2.0;
    assert!(penultimate_radius < points[1].point().distance_to(points[2].point()));
  }

  #[test]
  fn moving_each_element_kind_replaces_the_original_paint() {
    let template = document();
    let elements = vec![
      stroke(&template, 0),
      arrow(&template, 0, PointPx::new(70.0, 110.0), PointPx::new(170.0, 110.0)),
      rectangle(&template, 0, PointPx::new(80.0, 80.0), PointPx::new(180.0, 155.0)),
      text_element(&template, PointPx::new(80.0, 80.0), "move", 120.0),
      sequence_marker(&template, 0),
    ];
    let start = PointPx::new(100.0, 100.0);
    let current = PointPx::new(120.0, 110.0);

    for element in elements {
      let kind = element.kind();
      let element_id = element.element_id;
      let original_bounds = element.bounds_px;
      let mut document = document();
      document.elements.push(element);
      let baseline = paint_canvas_output(&EditorController::default(), &document);
      let baseline_shape_count = baseline.shapes.len();
      let baseline_rects =
        baseline.shapes.iter().map(|shape| shape.shape.visual_bounding_rect()).collect::<Vec<_>>();
      baseline.drop_without_applying_deltas();

      let interaction = PointerInteraction::Move { element_id, start, current };
      let preview = interaction_preview_element(&interaction, &document).unwrap();
      assert_ne!(preview.bounds_px, original_bounds, "kind={kind:?}");
      let controller = EditorController { interaction: Some(interaction), ..Default::default() };
      let output = paint_canvas_output(&controller, &document);
      assert_eq!(output.shapes.len(), baseline_shape_count, "kind={kind:?}");
      let preview_rects =
        output.shapes.iter().map(|shape| shape.shape.visual_bounding_rect()).collect::<Vec<_>>();
      assert_ne!(preview_rects, baseline_rects, "kind={kind:?}");
      for shape in &output.shapes {
        if let egui::Shape::LineSegment { stroke, .. } = &shape.shape {
          assert_eq!(stroke.color.a(), u8::MAX, "kind={kind:?}");
        }
      }
      output.drop_without_applying_deltas();

      let mut applied = document.clone();
      DocumentCommand::MoveElement { element_id, delta_px: current - start }
        .apply(&mut applied)
        .unwrap();
      assert_eq!(applied.element(element_id), Some(&preview), "kind={kind:?}");
    }
  }

  #[test]
  fn release_frame_paints_the_committed_selection_transform() {
    let cases = [
      {
        let mut document = document();
        let element = rectangle(&document, 0, PointPx::new(80.0, 80.0), PointPx::new(180.0, 155.0));
        let element_id = element.element_id;
        document.elements.push(element);
        (
          "move",
          document,
          PointerInteraction::Move {
            element_id,
            start: PointPx::new(100.0, 100.0),
            current: PointPx::new(132.0, 118.0),
          },
          element_id,
        )
      },
      {
        let mut document = document();
        let element = rectangle(&document, 0, PointPx::new(80.0, 80.0), PointPx::new(180.0, 155.0));
        let element_id = element.element_id;
        let ElementPayload::Rectangle(payload) = &element.payload else {
          unreachable!();
        };
        let original = RectPx::from_points(payload.start_px, payload.end_px);
        document.elements.push(element);
        (
          "resize rectangle",
          document,
          PointerInteraction::ResizeRectangle {
            element_id,
            handle: RectangleHandle::BottomRight,
            original,
            current: PointPx::new(230.0, 178.0),
          },
          element_id,
        )
      },
      {
        let mut document = document();
        let element = arrow(&document, 0, PointPx::new(70.0, 110.0), PointPx::new(170.0, 110.0));
        let element_id = element.element_id;
        document.elements.push(element);
        (
          "update arrow endpoint",
          document,
          PointerInteraction::UpdateArrowEndpoint {
            element_id,
            endpoint: ArrowEndpoint::End,
            current: PointPx::new(210.0, 150.0),
          },
          element_id,
        )
      },
    ];

    for (case, document, interaction, element_id) in cases {
      let baseline_output = paint_canvas_output(&EditorController::default(), &document);
      let baseline_rects = painted_shape_rects(&baseline_output);
      baseline_output.drop_without_applying_deltas();

      let mut controller = EditorController {
        selected_element_id: Some(element_id),
        interaction: Some(interaction),
        ..Default::default()
      };
      let mut actions = Vec::new();
      controller.finish_pointer_interaction(&document, &mut actions);
      assert!(controller.interaction.is_none(), "case={case}");
      assert!(
        !controller.released_preview_elements.is_empty(),
        "case={case} should keep the final element for release-frame painting"
      );

      let release_output = paint_canvas_output(&controller, &document);
      let release_rects = painted_shape_rects(&release_output);
      release_output.drop_without_applying_deltas();
      assert_ne!(release_rects, baseline_rects, "case={case}");

      let [EditorAction::Command(batch)] = actions.as_slice() else {
        panic!("expected one command for {case}, got {actions:?}");
      };
      let mut committed_document = document.clone();
      batch.clone().apply(&mut committed_document).unwrap();
      let committed_output = paint_canvas_output(&EditorController::default(), &committed_document);
      let committed_rects = painted_shape_rects(&committed_output);
      committed_output.drop_without_applying_deltas();

      assert_eq!(release_rects, committed_rects, "case={case}");
    }
  }

  #[test]
  fn new_rectangle_shows_raw_undersized_geometry_then_adds_minimum_size() {
    let document = document();
    let preview_controller = EditorController::default();
    let minimum =
      minimum_geometry_extent(preview_controller.tool_style(EditorTool::Rectangle).width_px)
        .unwrap();
    let start = PointPx::new(200.0, 100.0);
    let cases = [
      ("zero", start),
      ("small", PointPx::new(start.x_px + minimum / 4.0, start.y_px + minimum / 3.0)),
      ("reverse", PointPx::new(start.x_px - minimum / 4.0, start.y_px - minimum / 3.0)),
    ];

    for (case, requested) in cases {
      let element_id = ElementId::new();
      let preview =
        preview_controller.make_rectangle_preview(&document, element_id, start, requested).unwrap();
      let ElementPayload::Rectangle(preview_payload) = &preview.payload else {
        unreachable!();
      };
      assert_eq!(preview_payload.start_px, start, "case={case}");
      assert_eq!(preview_payload.end_px, requested, "case={case}");
      assert_eq!(
        preview_payload.label.text.as_deref(),
        Some(DEFAULT_RECTANGLE_LABEL),
        "case={case}"
      );
      let raw = RectPx::from_points(preview_payload.start_px, preview_payload.end_px);
      assert!(raw.width() < minimum && raw.height() < minimum, "case={case}");
      preview.bounds_px.validate().unwrap();
      assert!(preview.validate(document.canvas_size_px).is_err(), "case={case}");

      let interaction = PointerInteraction::Draw {
        element_id,
        tool: EditorTool::Rectangle,
        start,
        current: requested,
        stroke_points: Vec::new(),
      };
      let mut controller =
        EditorController { interaction: Some(interaction), ..Default::default() };
      let (context, output) = paint_canvas_output_with_context(&controller, &document);
      assert_paint_shapes_are_finite(&output);
      assert_tessellated_meshes_are_finite(&context, &output);
      output.drop_without_applying_deltas();

      let mut actions = Vec::new();
      controller.finish_pointer_interaction(&document, &mut actions);
      let [EditorAction::Command(batch)] = actions.as_slice() else {
        panic!("expected one added rectangle for {case}, got {actions:?}");
      };
      let commands = batch.commands();
      let Some(DocumentCommand::AddElement { element }) = commands.first() else {
        panic!("expected AddElement for {case}");
      };
      assert!(
        commands[1..]
          .iter()
          .all(|command| matches!(command, DocumentCommand::SetRectangleLabelPlacement { .. })),
        "expected AddElement followed by placement commands for {case}, got {commands:?}"
      );
      let committed = element.clone();
      let ElementPayload::Rectangle(committed_payload) = &committed.payload else {
        unreachable!();
      };
      let committed_body =
        RectPx::from_points(committed_payload.start_px, committed_payload.end_px);
      assert!(committed_body.width() >= minimum, "case={case}");
      assert!(committed_body.height() >= minimum, "case={case}");
      let expected_x_direction = if requested.x_px < start.x_px { -1.0 } else { 1.0 };
      let expected_y_direction = if requested.y_px < start.y_px { -1.0 } else { 1.0 };
      assert_eq!(
        (committed_payload.end_px.x_px - start.x_px).signum(),
        expected_x_direction,
        "case={case}"
      );
      assert_eq!(
        (committed_payload.end_px.y_px - start.y_px).signum(),
        expected_y_direction,
        "case={case}"
      );
      committed.validate(document.canvas_size_px).unwrap();

      let mut applied = document.clone();
      batch.clone().apply(&mut applied).unwrap();
      assert_eq!(applied.elements.len(), 1, "case={case}");
      let applied = applied.element(committed.element_id).unwrap();
      let ElementPayload::Rectangle(applied_payload) = &applied.payload else {
        unreachable!();
      };
      let applied_body = RectPx::from_points(applied_payload.start_px, applied_payload.end_px);
      assert!(applied_body.width() >= minimum, "case={case}");
      assert!(applied_body.height() >= minimum, "case={case}");
      applied.validate(document.canvas_size_px).unwrap();
    }
  }

  #[test]
  fn new_arrow_shows_raw_undersized_geometry_then_adds_minimum_length() {
    let document = document();
    let preview_controller = EditorController::default();
    let style = preview_controller.tool_style(EditorTool::Arrow);
    let minimum = ArrowHead::for_stroke_width(style.width_px).unwrap().min_body_length_px;
    let start = PointPx::new(200.0, 100.0);
    let cases = [
      ("zero", start),
      ("small", PointPx::new(start.x_px + minimum / 4.0, start.y_px + minimum / 5.0)),
      ("reverse", PointPx::new(start.x_px - minimum / 4.0, start.y_px - minimum / 5.0)),
    ];

    for (case, requested) in cases {
      let preview = preview_controller.make_arrow_preview(&document, start, requested).unwrap();
      let ElementPayload::Arrow(preview_payload) = &preview.payload else {
        unreachable!();
      };
      assert_eq!(preview_payload.start_px, start, "case={case}");
      assert_eq!(preview_payload.end_px, requested, "case={case}");
      assert!(
        preview_payload.start_px.distance_to(preview_payload.end_px) < minimum,
        "case={case}"
      );
      assert!(preview_payload.start_px.is_finite(), "case={case}");
      assert!(preview_payload.end_px.is_finite(), "case={case}");
      preview.bounds_px.validate().unwrap();
      assert!(preview.validate(document.canvas_size_px).is_err(), "case={case}");

      let interaction = PointerInteraction::Draw {
        element_id: ElementId::new(),
        tool: EditorTool::Arrow,
        start,
        current: requested,
        stroke_points: Vec::new(),
      };
      let mut controller =
        EditorController { interaction: Some(interaction), ..Default::default() };
      let (context, output) = paint_canvas_output_with_context(&controller, &document);
      assert_paint_shapes_are_finite(&output);
      assert_tessellated_meshes_are_finite(&context, &output);
      output.drop_without_applying_deltas();

      let mut actions = Vec::new();
      controller.finish_pointer_interaction(&document, &mut actions);
      let [EditorAction::Command(batch), EditorAction::Toast(message)] = actions.as_slice() else {
        panic!("expected an added arrow and short-label toast for {case}, got {actions:?}");
      };
      assert_eq!(message, ARROW_LABEL_TOO_SHORT_TOAST);
      let [DocumentCommand::AddElement { element }] = batch.commands() else {
        panic!("expected AddElement for {case}");
      };
      let committed = element.clone();
      let ElementPayload::Arrow(committed_payload) = &committed.payload else {
        unreachable!();
      };
      let committed_offset = committed_payload.end_px - committed_payload.start_px;
      let committed_length = committed_payload.start_px.distance_to(committed_payload.end_px);
      assert!((committed_length - minimum).abs() < 0.001, "case={case}");
      let requested_offset = requested - start;
      let requested_length = start.distance_to(requested);
      if requested_length <= f32::EPSILON {
        assert_point_close(
          committed_payload.end_px,
          PointPx::new(start.x_px + minimum, start.y_px),
        );
      } else {
        let cross = committed_offset.x_px * requested_offset.y_px
          - committed_offset.y_px * requested_offset.x_px;
        let dot = committed_offset.x_px * requested_offset.x_px
          + committed_offset.y_px * requested_offset.y_px;
        assert!(cross.abs() < 0.001, "case={case}, cross={cross}");
        assert!(dot > 0.0, "case={case}, dot={dot}");
      }
      committed.validate(document.canvas_size_px).unwrap();

      let mut applied = document.clone();
      batch.clone().apply(&mut applied).unwrap();
      assert_eq!(applied.elements, vec![committed], "case={case}");
    }
  }

  #[test]
  fn high_coordinate_new_arrow_release_still_reaches_the_minimum_length() {
    let document = document_with_size(SizePx::new(8_192, 8_192));
    let controller = EditorController::default();
    let start = PointPx::new(8_000.0, 8_000.0);
    let requested = start + PointPx::new(5.0, 3.0);

    let element = controller.make_arrow(&document, start, requested).unwrap();

    let ElementPayload::Arrow(payload) = &element.payload else {
      unreachable!();
    };
    let length = payload.start_px.distance_to(payload.end_px);
    assert!(length >= payload.head.min_body_length_px);
    assert!(length - payload.head.min_body_length_px < 0.01);
    element.validate(document.canvas_size_px).unwrap();
  }

  #[test]
  fn rectangle_resize_shows_raw_size_then_clamps_on_release() {
    let mut document = document();
    let element = rectangle(&document, 0, PointPx::new(80.0, 80.0), PointPx::new(200.0, 160.0));
    let element_id = element.element_id;
    let ElementPayload::Rectangle(payload) = &element.payload else {
      unreachable!();
    };
    let original = RectPx::from_points(payload.start_px, payload.end_px);
    let minimum = minimum_geometry_extent(payload.stroke_style.width_px).unwrap();
    document.elements.push(element);
    let baseline = paint_canvas_output(&EditorController::default(), &document);
    let baseline_shape_count = baseline.shapes.len();
    let baseline_rects =
      baseline.shapes.iter().map(|shape| shape.shape.visual_bounding_rect()).collect::<Vec<_>>();
    baseline.drop_without_applying_deltas();

    let undersized_interactions = [
      (RectangleHandle::TopLeft, original.max),
      (RectangleHandle::Top, PointPx::new(original.center().x_px, original.max.y_px)),
      (RectangleHandle::TopRight, PointPx::new(original.min.x_px, original.max.y_px)),
      (RectangleHandle::Right, PointPx::new(original.min.x_px, original.center().y_px)),
      (RectangleHandle::BottomRight, original.min),
      (RectangleHandle::Bottom, PointPx::new(original.center().x_px, original.min.y_px)),
      (RectangleHandle::BottomLeft, PointPx::new(original.max.x_px, original.min.y_px)),
      (RectangleHandle::Left, PointPx::new(original.max.x_px, original.center().y_px)),
    ];

    for (handle, current) in undersized_interactions {
      let interaction =
        PointerInteraction::ResizeRectangle { element_id, handle, original, current };
      let preview = interaction_preview_element(&interaction, &document).unwrap();
      let ElementPayload::Rectangle(preview_payload) = &preview.payload else {
        unreachable!();
      };
      let raw = RectPx::from_points(preview_payload.start_px, preview_payload.end_px);
      assert!(raw.width() < minimum || raw.height() < minimum, "handle={handle:?}");
      assert!(preview.validate(document.canvas_size_px).is_err());
      let mut controller =
        EditorController { interaction: Some(interaction), ..Default::default() };
      let output = paint_canvas_output(&controller, &document);
      assert_eq!(output.shapes.len(), baseline_shape_count, "handle={handle:?}");
      let painted_rects =
        output.shapes.iter().map(|shape| shape.shape.visual_bounding_rect()).collect::<Vec<_>>();
      assert_ne!(painted_rects, baseline_rects, "handle={handle:?}");
      output.drop_without_applying_deltas();
      let mut actions = Vec::new();
      controller.finish_pointer_interaction(&document, &mut actions);
      let [EditorAction::Command(batch)] = actions.as_slice() else {
        panic!("expected one rectangle command for {handle:?}, got {actions:?}");
      };
      let Some((start_px, end_px)) = resize_rectangle_command(batch.commands(), element_id) else {
        panic!("expected rectangle resize for {handle:?}, got {:?}", batch.commands());
      };
      assert_eq!(
        (start_px, end_px),
        minimum_resized_rectangle(original, handle, current, minimum),
        "handle={handle:?}"
      );
      let committed = RectPx::from_points(start_px, end_px);
      assert!(committed.width() >= minimum, "handle={handle:?}");
      assert!(committed.height() >= minimum, "handle={handle:?}");
      if raw.width() < minimum {
        assert!((committed.width() - minimum).abs() < 0.001, "handle={handle:?}");
      }
      if raw.height() < minimum {
        assert!((committed.height() - minimum).abs() < 0.001, "handle={handle:?}");
      }
      let mut applied = document.clone();
      batch.clone().apply(&mut applied).unwrap();
      applied.element(element_id).unwrap().validate(applied.canvas_size_px).unwrap();
    }

    let crossing_current = PointPx::new(original.min.x_px - minimum - 5.0, original.center().y_px);
    let interaction = PointerInteraction::ResizeRectangle {
      element_id,
      handle: RectangleHandle::Right,
      original,
      current: crossing_current,
    };
    let preview_controller = EditorController::default();
    let preview_set = interaction_preview_set(&preview_controller, &interaction, &document);
    let preview = preview_set.get(element_id).unwrap().clone();
    let mut controller = EditorController { interaction: Some(interaction), ..Default::default() };
    let output = paint_canvas_output(&controller, &document);
    assert_eq!(output.shapes.len(), baseline_shape_count);
    let preview_rects =
      output.shapes.iter().map(|shape| shape.shape.visual_bounding_rect()).collect::<Vec<_>>();
    assert_ne!(preview_rects, baseline_rects);
    output.drop_without_applying_deltas();
    let mut actions = Vec::new();
    controller.finish_pointer_interaction(&document, &mut actions);
    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one rectangle command, got {actions:?}");
    };
    assert_eq!(
      resize_rectangle_command(batch.commands(), element_id),
      Some((original.min, PointPx::new(crossing_current.x_px, original.max.y_px)))
    );
    let mut applied = document.clone();
    batch.clone().apply(&mut applied).unwrap();
    assert_eq!(applied.element(element_id), Some(&preview));
  }

  #[test]
  fn arrow_endpoint_clamp_uses_requested_direction_and_original_fallback() {
    let document = document();
    let element = arrow(&document, 0, PointPx::new(100.0, 100.0), PointPx::new(200.0, 100.0));
    let ElementPayload::Arrow(payload) = &element.payload else {
      unreachable!();
    };
    let minimum = payload.head.min_body_length_px;
    let cases = [
      (ArrowEndpoint::End, PointPx::new(105.0, 100.0), PointPx::new(100.0 + minimum, 100.0)),
      (ArrowEndpoint::Start, PointPx::new(195.0, 100.0), PointPx::new(200.0 - minimum, 100.0)),
      (ArrowEndpoint::End, payload.start_px, PointPx::new(100.0 + minimum, 100.0)),
      (ArrowEndpoint::Start, payload.end_px, PointPx::new(200.0 - minimum, 100.0)),
      (ArrowEndpoint::End, PointPx::new(95.0, 100.0), PointPx::new(100.0 - minimum, 100.0)),
      (
        ArrowEndpoint::End,
        PointPx::new(100.0 + minimum, 100.0),
        PointPx::new(100.0 + minimum, 100.0),
      ),
      (ArrowEndpoint::End, PointPx::new(70.0, 100.0), PointPx::new(70.0, 100.0)),
    ];

    for (endpoint, requested, expected) in cases {
      assert_point_close(clamped_arrow_endpoint(payload, endpoint, requested).0, expected);
    }

    let mut degenerate = payload.clone();
    degenerate.end_px = degenerate.start_px;
    let fallback = clamped_arrow_endpoint(&degenerate, ArrowEndpoint::End, degenerate.start_px).0;
    assert!(fallback.is_finite());
    assert_point_close(
      fallback,
      PointPx::new(
        degenerate.start_px.x_px + degenerate.head.min_body_length_px,
        degenerate.start_px.y_px,
      ),
    );
  }

  #[test]
  fn arrow_shows_raw_size_then_clamps_commit_and_history() {
    for endpoint in [ArrowEndpoint::Start, ArrowEndpoint::End] {
      let mut document = document();
      let element = arrow(&document, 0, PointPx::new(100.0, 100.0), PointPx::new(200.0, 100.0));
      let original = element.clone();
      let element_id = element.element_id;
      let ElementPayload::Arrow(payload) = &element.payload else {
        unreachable!();
      };
      let requested = match endpoint {
        ArrowEndpoint::Start => payload.end_px,
        ArrowEndpoint::End => payload.start_px,
      };
      let expected_position = clamped_arrow_endpoint(payload, endpoint, requested).0;
      document.elements.push(element);
      let interaction =
        PointerInteraction::UpdateArrowEndpoint { element_id, endpoint, current: requested };
      let preview = interaction_preview_element(&interaction, &document).unwrap();
      let ElementPayload::Arrow(preview_payload) = &preview.payload else {
        unreachable!();
      };
      let preview_position = match endpoint {
        ArrowEndpoint::Start => preview_payload.start_px,
        ArrowEndpoint::End => preview_payload.end_px,
      };
      assert_eq!(preview_position, requested);
      assert!(
        preview_payload.start_px.distance_to(preview_payload.end_px)
          < preview_payload.head.min_body_length_px
      );
      assert!(preview.validate(document.canvas_size_px).is_err());
      let baseline = paint_canvas_output(&EditorController::default(), &document);
      let baseline_shape_count = baseline.shapes.len();
      let baseline_rects =
        baseline.shapes.iter().map(|shape| shape.shape.visual_bounding_rect()).collect::<Vec<_>>();
      baseline.drop_without_applying_deltas();
      let mut controller =
        EditorController { interaction: Some(interaction), ..Default::default() };
      let output = paint_canvas_output(&controller, &document);
      let preview_shape_count = output.shapes.len();
      let preview_rects =
        output.shapes.iter().map(|shape| shape.shape.visual_bounding_rect()).collect::<Vec<_>>();
      output.drop_without_applying_deltas();
      assert!(
        preview_shape_count < baseline_shape_count,
        "raw endpoint preview must replace the arrowhead shapes: endpoint={endpoint:?}"
      );
      assert_ne!(preview_rects, baseline_rects, "endpoint={endpoint:?}");

      let mut actions = Vec::new();
      controller.finish_pointer_interaction(&document, &mut actions);
      let [EditorAction::Command(batch)] = actions.as_slice() else {
        panic!("expected one arrow command, got {actions:?}");
      };
      assert!(matches!(
        batch.commands(),
        [DocumentCommand::UpdateArrowEndpoint {
          endpoint: updated_endpoint,
          position_px,
          ..
        }] if *updated_endpoint == endpoint && *position_px == expected_position
      ));

      let mut changed = document.clone();
      let mut history = CommandHistory::new();
      history.execute_batch(&mut changed, batch.clone()).unwrap();
      let committed = changed.element(element_id).unwrap().clone();
      let ElementPayload::Arrow(committed_payload) = &committed.payload else {
        unreachable!();
      };
      assert!(
        (committed_payload.start_px.distance_to(committed_payload.end_px)
          - committed_payload.head.min_body_length_px)
          .abs()
          < 0.001
      );
      assert_ne!(committed, preview);
      assert!(history.undo(&mut changed).unwrap());
      assert_eq!(changed.element(element_id), Some(&original));
      assert!(history.redo(&mut changed).unwrap());
      assert_eq!(changed.element(element_id), Some(&committed));
    }
  }

  #[test]
  fn diagonal_arrow_endpoint_release_commits_a_valid_minimum_length() {
    for endpoint in [ArrowEndpoint::Start, ArrowEndpoint::End] {
      let mut document = document_with_size(SizePx::new(8_192, 8_192));
      let element =
        arrow(&document, 0, PointPx::new(7_900.0, 7_900.0), PointPx::new(8_000.0, 7_900.0));
      let element_id = element.element_id;
      let ElementPayload::Arrow(payload) = &element.payload else {
        unreachable!();
      };
      let fixed_px = match endpoint {
        ArrowEndpoint::Start => payload.end_px,
        ArrowEndpoint::End => payload.start_px,
      };
      let requested = fixed_px + PointPx::new(5.0, 3.0);
      let requested_direction = requested - fixed_px;
      let minimum = payload.head.min_body_length_px;
      document.elements.push(element);
      let mut controller = EditorController {
        interaction: Some(PointerInteraction::UpdateArrowEndpoint {
          element_id,
          endpoint,
          current: requested,
        }),
        ..Default::default()
      };
      let mut actions = Vec::new();

      controller.finish_pointer_interaction(&document, &mut actions);

      let [EditorAction::Command(batch)] = actions.as_slice() else {
        panic!("expected one diagonal arrow command for {endpoint:?}, got {actions:?}");
      };
      let mut changed = document.clone();
      batch.clone().apply(&mut changed).unwrap();
      let ElementPayload::Arrow(committed) = &changed.element(element_id).unwrap().payload else {
        unreachable!();
      };
      let committed_position = match endpoint {
        ArrowEndpoint::Start => committed.start_px,
        ArrowEndpoint::End => committed.end_px,
      };
      let committed_direction = committed_position - fixed_px;
      assert!(committed_position.distance_to(fixed_px) >= minimum);
      assert!(committed_position.distance_to(fixed_px) - minimum < 0.01);
      let cross = committed_direction.x_px * requested_direction.y_px
        - committed_direction.y_px * requested_direction.x_px;
      let dot = committed_direction.x_px * requested_direction.x_px
        + committed_direction.y_px * requested_direction.y_px;
      assert!(cross.abs() < 0.05, "endpoint={endpoint:?}, cross={cross}");
      assert!(dot > 0.0, "endpoint={endpoint:?}, dot={dot}");
    }
  }

  #[test]
  fn shape_selection_handles_follow_the_transform_preview_without_an_outline() {
    let mut rectangle_document = document();
    let rectangle =
      rectangle(&rectangle_document, 0, PointPx::new(80.0, 80.0), PointPx::new(200.0, 150.0));
    let rectangle_id = rectangle.element_id;
    let ElementPayload::Rectangle(rectangle_payload) = &rectangle.payload else {
      unreachable!();
    };
    let original_rectangle =
      RectPx::from_points(rectangle_payload.start_px, rectangle_payload.end_px);
    rectangle_document.elements.push(rectangle);
    let rectangle_interaction = PointerInteraction::ResizeRectangle {
      element_id: rectangle_id,
      handle: RectangleHandle::BottomRight,
      original: original_rectangle,
      current: PointPx::new(260.0, 175.0),
    };
    let rectangle_preview =
      interaction_preview_element(&rectangle_interaction, &rectangle_document).unwrap();
    let rectangle_controller = EditorController {
      selected_element_id: Some(rectangle_id),
      interaction: Some(rectangle_interaction),
      ..Default::default()
    };
    let rectangle_output = paint_selection_output(&rectangle_controller, &rectangle_document);
    let transform = CanvasTransform::fit(
      rectangle_document.canvas_size_px,
      Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 400.0)),
    )
    .unwrap();
    assert!(
      rectangle_output.shapes.iter().all(|shape| !matches!(shape.shape, egui::Shape::Rect(_)))
    );
    let circle_centers = rectangle_output
      .shapes
      .iter()
      .filter_map(|shape| match &shape.shape {
        egui::Shape::Circle(circle) => Some(circle.center),
        _ => None,
      })
      .collect::<Vec<_>>();
    let ElementPayload::Rectangle(preview_payload) = &rectangle_preview.payload else {
      unreachable!();
    };
    for (_, point) in
      rectangle_handles(RectPx::from_points(preview_payload.start_px, preview_payload.end_px))
    {
      let expected = transform.document_to_egui(point);
      assert!(circle_centers.iter().any(|center| center.distance(expected) < 0.001));
    }
    let old_bottom_right = transform.document_to_egui(original_rectangle.max);
    assert!(!circle_centers.iter().any(|center| center.distance(old_bottom_right) < 0.001));
    rectangle_output.drop_without_applying_deltas();

    let mut arrow_document = document();
    let arrow = arrow(&arrow_document, 0, PointPx::new(100.0, 100.0), PointPx::new(200.0, 100.0));
    let arrow_id = arrow.element_id;
    let ElementPayload::Arrow(arrow_payload) = &arrow.payload else {
      unreachable!();
    };
    let old_end = arrow_payload.end_px;
    let arrow_interaction = PointerInteraction::UpdateArrowEndpoint {
      element_id: arrow_id,
      endpoint: ArrowEndpoint::End,
      current: arrow_payload.start_px,
    };
    arrow_document.elements.push(arrow);
    let arrow_preview = interaction_preview_element(&arrow_interaction, &arrow_document).unwrap();
    let arrow_controller = EditorController {
      selected_element_id: Some(arrow_id),
      interaction: Some(arrow_interaction),
      ..Default::default()
    };
    let arrow_output = paint_selection_output(&arrow_controller, &arrow_document);
    assert!(arrow_output.shapes.iter().all(|shape| !matches!(shape.shape, egui::Shape::Rect(_))));
    let arrow_centers = arrow_output
      .shapes
      .iter()
      .filter_map(|shape| match &shape.shape {
        egui::Shape::Circle(circle) => Some(circle.center),
        _ => None,
      })
      .collect::<Vec<_>>();
    let ElementPayload::Arrow(preview_payload) = &arrow_preview.payload else {
      unreachable!();
    };
    for point in [preview_payload.start_px, preview_payload.end_px] {
      let expected = transform.document_to_egui(point);
      assert!(arrow_centers.iter().any(|center| center.distance(expected) < 0.001));
    }
    let old_end = transform.document_to_egui(old_end);
    assert!(!arrow_centers.iter().any(|center| center.distance(old_end) < 0.001));
    arrow_output.drop_without_applying_deltas();
  }

  #[test]
  fn other_elements_keep_the_selection_outline() {
    let mut document = document();
    let elements = [
      stroke(&document, 0),
      text_element(&document, PointPx::new(80.0, 80.0), "Text", 120.0),
      sequence_marker(&document, 2),
    ];

    for element in elements {
      let element_id = element.element_id;
      document.elements.push(element);
      let controller =
        EditorController { selected_element_id: Some(element_id), ..Default::default() };
      let output = paint_selection_output(&controller, &document);

      assert!(output.shapes.iter().any(|shape| matches!(shape.shape, egui::Shape::Rect(_))));
      output.drop_without_applying_deltas();
    }
  }

  #[test]
  fn fit_transform_centers_canvas_and_round_trips() {
    let transform = CanvasTransform::fit(
      SizePx::new(400, 200),
      Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(300.0, 300.0)),
    )
    .unwrap();
    assert_eq!(
      transform.canvas_rect(),
      Rect::from_min_max(egui::pos2(0.0, 75.0), egui::pos2(300.0, 225.0))
    );
    assert_eq!(transform.scale(), 0.75);
    let point = PointPx::new(123.0, 77.0);
    let round_trip = transform.egui_to_document(transform.document_to_egui(point)).unwrap();
    assert!((round_trip.x_px - point.x_px).abs() < 0.001);
    assert!((round_trip.y_px - point.y_px).abs() < 0.001);
    assert!(transform.egui_to_document(egui::pos2(150.0, 20.0)).is_none());
  }

  #[test]
  fn fit_transform_fills_a_viewport_with_the_capture_aspect_ratio() {
    let viewport = Rect::from_min_size(Pos2::ZERO, egui::vec2(1728.0, 1117.0));
    let transform = CanvasTransform::fit(SizePx::new(3456, 2234), viewport).unwrap();

    assert_eq!(transform.canvas_rect(), viewport);
  }

  #[test]
  fn toolbar_and_option_controls_share_the_same_vertical_center() {
    let context = egui::Context::default();
    context.all_styles_mut(|style| style.spacing.button_padding = egui::vec2(12.0, 7.0));
    let measurements = Cell::new([(0.0, 0.0); 7]);
    let panel_height = Cell::new(0.0);
    let panel_stroke_width = Cell::new(0.0);

    context
      .run_ui(
        egui::RawInput {
          screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1200.0, 200.0))),
          ..Default::default()
        },
        |ui| {
          let frame = floating_panel_frame(ui.style());
          panel_stroke_width.set(frame.stroke.width);
          let panel = frame.show(ui, |ui| {
            set_floating_control_style(ui);
            ui.horizontal(|ui| {
              let left_handle = toolbar_drag_handle(ui);
              let tool = ui.selectable_label(true, "方框");
              let color = egui::ComboBox::from_id_salt("toolbar-layout-test-color")
                .selected_text(
                  egui::RichText::new("●").color(Color32::RED).size(COLOR_SWATCH_FONT_SIZE_PT),
                )
                .show_ui(ui, |_| {})
                .response;
              let width = egui::ComboBox::from_id_salt("toolbar-layout-test-width")
                .selected_text("8 px")
                .show_ui(ui, |_| {})
                .response;
              let font_size = egui::ComboBox::from_id_salt("toolbar-layout-test-font-size")
                .selected_text("24 pt")
                .show_ui(ui, |_| {})
                .response;
              let save = ui.button("保存");
              let right_handle = toolbar_drag_handle(ui);
              measurements.set([
                (left_handle.center().y, left_handle.height()),
                (tool.rect.center().y, tool.rect.height()),
                (color.rect.center().y, color.rect.height()),
                (width.rect.center().y, width.rect.height()),
                (font_size.rect.center().y, font_size.rect.height()),
                (save.rect.center().y, save.rect.height()),
                (right_handle.center().y, right_handle.height()),
              ]);
            });
          });
          panel_height.set(panel.response.rect.height());
        },
      )
      .drop_without_applying_deltas();

    let measurements = measurements.get();
    for (_, height) in measurements {
      assert!((height - FLOATING_CONTROL_HEIGHT_PT).abs() < 0.1, "controls: {measurements:?}");
    }
    for pair in measurements.windows(2) {
      assert!((pair[0].0 - pair[1].0).abs() < 0.1, "controls: {measurements:?}");
    }
    let expected_panel_height = FLOATING_CONTROL_HEIGHT_PT
      + 2.0 * FLOATING_PANEL_MARGIN_PT as f32
      + 2.0 * panel_stroke_width.get();
    assert!(
      (panel_height.get() - expected_panel_height).abs() < 0.1,
      "panel={}, expected={expected_panel_height}",
      panel_height.get()
    );
  }

  #[test]
  fn toolbar_recenters_after_fullscreen_resize_and_drag_stays_within_screen() {
    let context = egui::Context::default();
    let startup_screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 300.0));
    let fullscreen = Rect::from_min_size(Pos2::ZERO, egui::vec2(1200.0, 700.0));
    let toolbar_rect = Cell::new(Rect::NOTHING);
    let left_handle_rect = Cell::new(Rect::NOTHING);
    let dragged = Cell::new(false);
    let mut controller = EditorController::default();
    let mut run_frame = |screen, events| {
      let recenter = controller.should_recenter_toolbar(screen);
      context
        .run_ui(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ui| {
          let area = toolbar_area(screen, recenter).show(ui.ctx(), |ui| {
            floating_panel_frame(ui.style()).show(ui, |ui| {
              set_floating_control_style(ui);
              ui.horizontal(|ui| {
                left_handle_rect.set(toolbar_drag_handle(ui));
                ui.label("toolbar");
                toolbar_drag_handle(ui);
              });
            });
          });
          toolbar_rect.set(area.response.rect);
          dragged.set(area.response.dragged());
        })
        .drop_without_applying_deltas();
      controller.toolbar_was_moved |= dragged.get();
    };

    run_frame(startup_screen, Vec::new());
    run_frame(fullscreen, Vec::new());
    let initial_rect = toolbar_rect.get();
    assert!(
      (initial_rect.center().x - fullscreen.center().x).abs() <= 0.5,
      "toolbar={initial_rect:?}, screen={fullscreen:?}"
    );
    assert!(
      (initial_rect.bottom() - fullscreen.bottom()).abs() < 0.1,
      "toolbar={initial_rect:?}, screen={fullscreen:?}"
    );
    let handle_center = left_handle_rect.get().center();
    run_frame(
      fullscreen,
      vec![
        Event::PointerMoved(handle_center),
        Event::PointerButton {
          pos: handle_center,
          button: egui::PointerButton::Primary,
          pressed: true,
          modifiers: Modifiers::default(),
        },
      ],
    );
    run_frame(fullscreen, vec![Event::PointerMoved(egui::pos2(-100.0, -100.0))]);

    let dragged_rect = toolbar_rect.get();
    assert!(dragged_rect.center().distance(initial_rect.center()) > 1.0);
    assert!(
      fullscreen.contains_rect(dragged_rect),
      "toolbar={dragged_rect:?}, screen={fullscreen:?}"
    );

    let expanded_screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(1400.0, 800.0));
    run_frame(expanded_screen, Vec::new());
    let rect_after_resize = toolbar_rect.get();
    assert!(rect_after_resize.min.distance(dragged_rect.min) <= 0.5);
  }

  #[test]
  fn floating_menus_are_compact_complete_and_above_panels() {
    let context = egui::Context::default();
    context.all_styles_mut(|style| style.spacing.button_padding = egui::vec2(12.0, 7.0));
    let heights = Cell::new([0.0; 2]);
    let overflows = Cell::new([0.0; 2]);

    context
      .run_ui(
        egui::RawInput {
          screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1200.0, 800.0))),
          ..Default::default()
        },
        |ui| {
          egui::containers::menu::menu_style(ui.style_mut());
          set_floating_control_style(ui);
          let colors =
            egui::ScrollArea::vertical().max_height(FLOATING_MENU_MAX_HEIGHT_PT).show(ui, |ui| {
              let mut first_height: f32 = 0.0;
              for (index, color) in mvp_colors().into_iter().enumerate() {
                let response = ui.selectable_label(
                  index == 0,
                  egui::RichText::new("●").color(color32(color)).size(COLOR_SWATCH_FONT_SIZE_PT),
                );
                first_height = first_height.max(response.rect.height());
              }
              first_height
            });
          let font_sizes =
            egui::ScrollArea::vertical().max_height(FLOATING_MENU_MAX_HEIGHT_PT).show(ui, |ui| {
              let mut first_height: f32 = 0.0;
              for font_size in PRESET_FONT_SIZES_PX {
                let response = ui.selectable_label(font_size == 24.0, format!("{font_size} pt"));
                first_height = first_height.max(response.rect.height());
              }
              first_height
            });
          heights.set([colors.inner, font_sizes.inner]);
          overflows.set([
            colors.content_size.y - colors.inner_rect.height().ceil(),
            font_sizes.content_size.y - font_sizes.inner_rect.height().ceil(),
          ]);
        },
      )
      .drop_without_applying_deltas();

    for height in heights.get() {
      assert!((height - FLOATING_CONTROL_HEIGHT_PT).abs() < 0.1);
    }
    for overflow in overflows.get() {
      assert!(overflow <= 0.0, "menu overflow={overflow}");
    }
    assert!(FLOATING_PANEL_ORDER < egui::PopupKind::Menu.order());
  }

  #[test]
  fn hit_test_uses_highest_z_order() {
    let mut document = document();
    let lower = rectangle(&document, 0, PointPx::new(40.0, 70.0), PointPx::new(180.0, 170.0));
    let upper = rectangle(&document, 1, PointPx::new(80.0, 80.0), PointPx::new(220.0, 180.0));
    let expected = upper.element_id;
    document.elements = vec![lower, upper];
    assert_eq!(hit_test_document(&document, PointPx::new(100.0, 100.0), 2.0), Some(expected));
  }

  #[test]
  fn shortcut_mapping_respects_text_editing_priority() {
    assert_eq!(
      map_shortcut(Key::Num2, Modifiers::NONE, false),
      Some(ShortcutAction::Tool(EditorTool::Rectangle))
    );
    assert_eq!(map_shortcut(Key::Num2, Modifiers::NONE, true), None);
    assert_eq!(
      map_shortcut(Key::Num2, Modifiers::COMMAND, true),
      Some(ShortcutAction::Tool(EditorTool::Rectangle))
    );
    assert_eq!(map_shortcut(Key::C, Modifiers::COMMAND, true), None);
    assert_eq!(
      map_shortcut(Key::Z, Modifiers::COMMAND | Modifiers::SHIFT, false),
      Some(ShortcutAction::Redo)
    );
    assert_eq!(map_shortcut(Key::Enter, Modifiers::NONE, true), None);
    assert_eq!(map_shortcut(Key::Enter, Modifiers::SHIFT, true), None);
  }

  #[test]
  fn escape_without_text_editing_reaches_the_keyboard_handler() {
    let document = document();
    let history = CommandHistory::new();
    let escape = Event::Key {
      key: Key::Escape,
      physical_key: Some(Key::Escape),
      pressed: true,
      repeat: false,
      modifiers: Modifiers::NONE,
    };

    let mut idle = EditorController::default();
    assert_eq!(
      run_editor_frame(
        &egui::Context::default(),
        &mut idle,
        &document,
        &history,
        vec![escape.clone()],
      ),
      vec![EditorAction::Close]
    );

    let mut interacting = EditorController {
      interaction: Some(PointerInteraction::Move {
        element_id: ElementId::new(),
        start: PointPx::new(10.0, 10.0),
        current: PointPx::new(20.0, 20.0),
      }),
      ..Default::default()
    };
    assert!(
      run_editor_frame(
        &egui::Context::default(),
        &mut interacting,
        &document,
        &history,
        vec![escape],
      )
      .is_empty()
    );
    assert!(interacting.interaction.is_none());
  }

  fn enter_event(modifiers: Modifiers) -> Event {
    Event::Key {
      key: Key::Enter,
      physical_key: Some(Key::Enter),
      pressed: true,
      repeat: false,
      modifiers,
    }
  }

  #[test]
  fn inline_enter_events_distinguish_submit_and_newline() {
    let mut ime = InlineImeState::default();
    let mut events = vec![enter_event(Modifiers::NONE)];
    assert!(normalize_inline_text_events(&mut events, &mut ime));
    assert!(events.is_empty());

    for modifiers in [Modifiers::SHIFT, Modifiers::COMMAND, Modifiers::SHIFT | Modifiers::COMMAND] {
      let mut events = vec![enter_event(modifiers)];
      assert!(!normalize_inline_text_events(&mut events, &mut ime));
      assert!(matches!(
        events.as_slice(),
        [Event::Key { key: Key::Enter, modifiers: Modifiers::SHIFT, pressed: true, .. }]
      ));
    }
  }

  #[test]
  fn inline_newline_replaces_selection_or_inserts_at_cursor_once() {
    use egui::text::{CCursor, CCursorRange};

    for (modifiers, cursor_range, expected) in [
      (Modifiers::SHIFT, CCursorRange::two(CCursor::new(1), CCursor::new(3)), "a\nd"),
      (Modifiers::MAC_CMD | Modifiers::COMMAND, CCursorRange::one(CCursor::new(2)), "ab\ncd"),
      (
        Modifiers::SHIFT | Modifiers::MAC_CMD | Modifiers::COMMAND,
        CCursorRange::two(CCursor::new(1), CCursor::new(3)),
        "a\nd",
      ),
    ] {
      let context = egui::Context::default();
      let editor_id = Id::new(("inline-newline-test", modifiers));
      let mut buffer = "abcd".to_owned();
      context
        .run_ui(raw_input(Vec::new(), egui::vec2(320.0, 120.0)), |ui| {
          let mut output = egui::TextEdit::multiline(&mut buffer)
            .id(editor_id)
            .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
            .show(ui);
          output.response.request_focus();
          output.state.cursor.set_char_range(Some(cursor_range));
          output.state.store(&context, editor_id);
        })
        .drop_without_applying_deltas();

      let mut ime = InlineImeState::default();
      let mut events = vec![enter_event(modifiers)];
      assert!(!normalize_inline_text_events(&mut events, &mut ime));
      let focused = Cell::new(false);
      context
        .run_ui(raw_input(events, egui::vec2(320.0, 120.0)), |ui| {
          let output = egui::TextEdit::multiline(&mut buffer)
            .id(editor_id)
            .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
            .show(ui);
          focused.set(output.response.has_focus());
        })
        .drop_without_applying_deltas();

      assert_eq!(buffer, expected);
      assert_eq!(buffer.matches('\n').count(), 1);
      assert!(focused.get());
    }
  }

  #[test]
  fn ime_commit_trims_confirmation_newline_without_submitting() {
    for key_first in [false, true] {
      let mut ime = InlineImeState { preedit: Some("o".to_owned()), ..Default::default() };
      let commit = Event::Ime(ImeEvent::Commit("OK\r\n".to_owned()));
      let enter = enter_event(Modifiers::NONE);
      let mut events = if key_first { vec![enter, commit] } else { vec![commit, enter] };

      assert!(!normalize_inline_text_events(&mut events, &mut ime));
      assert_eq!(events, vec![Event::Ime(ImeEvent::Commit("OK".to_owned()))]);
      assert!(ime.preedit.is_none());
    }
  }

  #[test]
  fn ime_enter_is_consumed_until_composition_commits() {
    let mut ime = InlineImeState::default();
    let mut preedit =
      vec![Event::Ime(ImeEvent::Preedit { text: "o".to_owned(), active_range_chars: Some(0..1) })];
    assert!(!normalize_inline_text_events(&mut preedit, &mut ime));
    assert_eq!(ime.preedit.as_deref(), Some("o"));

    let mut first_enter = vec![enter_event(Modifiers::NONE)];
    assert!(!normalize_inline_text_events(&mut first_enter, &mut ime));
    assert!(first_enter.is_empty());

    let mut commit = vec![Event::Ime(ImeEvent::Commit("OK\n".to_owned()))];
    assert!(!normalize_inline_text_events(&mut commit, &mut ime));
    assert_eq!(commit, vec![Event::Ime(ImeEvent::Commit("OK".to_owned()))]);
    assert!(ime.preedit.is_none());

    let mut second_enter = vec![enter_event(Modifiers::NONE)];
    assert!(normalize_inline_text_events(&mut second_enter, &mut ime));
    assert!(second_enter.is_empty());
  }

  #[test]
  fn dictation_commit_preserves_trailing_and_internal_newlines() {
    let mut ime = InlineImeState::default();
    let mut events = vec![Event::Ime(ImeEvent::Commit("A\nB\n".to_owned()))];
    assert!(!normalize_inline_text_events(&mut events, &mut ime));
    assert_eq!(events, vec![Event::Ime(ImeEvent::Commit("A\nB\n".to_owned()))]);
  }

  #[test]
  fn ime_confirmation_handles_preedit_clear_before_commit() {
    let preedit =
      Event::Ime(ImeEvent::Preedit { text: "O".to_owned(), active_range_chars: Some(0..1) });
    let clear_preedit =
      Event::Ime(ImeEvent::Preedit { text: String::new(), active_range_chars: None });
    let commit = Event::Ime(ImeEvent::Commit("OK\n".to_owned()));
    let enter = enter_event(Modifiers::NONE);
    for mut events in [
      vec![preedit.clone(), commit.clone(), enter.clone()],
      vec![preedit.clone(), enter.clone(), commit.clone()],
      vec![preedit.clone(), clear_preedit.clone(), commit.clone()],
    ] {
      let mut ime = InlineImeState::default();
      assert!(!normalize_inline_text_events(&mut events, &mut ime));
      assert!(
        events
          .iter()
          .all(|event| !matches!(event, Event::Key { key: Key::Enter, pressed: true, .. }))
      );
      assert!(
        events
          .iter()
          .any(|event| { matches!(event, Event::Ime(ImeEvent::Commit(text)) if text == "OK") })
      );
    }
  }

  #[test]
  fn ime_newline_commit_does_not_use_future_preedit() {
    let mut ime = InlineImeState::default();
    let mut events = vec![
      Event::Ime(ImeEvent::Commit("\n".to_owned())),
      Event::Ime(ImeEvent::Preedit { text: "future".to_owned(), active_range_chars: Some(0..6) }),
    ];

    assert!(!normalize_inline_text_events(&mut events, &mut ime));
    assert_eq!(events[0], synthetic_shift_enter_event());
    assert_eq!(ime.preedit.as_deref(), Some("future"));
  }

  #[test]
  fn active_ime_newline_confirmation_falls_back_to_preedit() {
    let mut ime = InlineImeState { preedit: Some("候选".to_owned()), ..Default::default() };
    let mut events = vec![Event::Ime(ImeEvent::Commit("\r\n".to_owned()))];

    assert!(!normalize_inline_text_events(&mut events, &mut ime));
    assert_eq!(events, vec![Event::Ime(ImeEvent::Commit("候选".to_owned()))]);
    assert!(ime.preedit.is_none());
  }

  #[test]
  fn idle_ime_newline_commit_routes_to_inline_newline() {
    for line_ending in ["\n", "\r", "\r\n"] {
      let mut ime = InlineImeState::default();
      let mut events = vec![Event::Ime(ImeEvent::Commit(line_ending.to_owned()))];

      assert!(!normalize_inline_text_events(&mut events, &mut ime));
      assert_eq!(events, vec![synthetic_shift_enter_event()]);
    }
  }

  #[test]
  fn dictation_commit_then_enter_submits_after_widget() {
    let mut ime = InlineImeState::default();
    let mut events =
      vec![Event::Ime(ImeEvent::Commit("dictated".to_owned())), enter_event(Modifiers::NONE)];

    assert!(normalize_inline_text_events(&mut events, &mut ime));
    assert_eq!(events, vec![Event::Ime(ImeEvent::Commit("dictated".to_owned()))]);
    assert!(ime.preedit.is_none());
  }

  #[test]
  fn dictation_same_frame_enter_applies_text_before_submit() {
    let document = document();
    let mut controller = EditorController {
      text_editing: Some(TextEditing {
        target: TextTarget::NewText { anchor_px: PointPx::new(120.0, 40.0) },
        buffer: String::new(),
        text_style: TextStyle::mvp(ColorRgba::WHITE, 24.0).unwrap(),
        ime: InlineImeState::default(),
        request_focus: true,
        select_all: false,
        auto_place_rectangle: false,
      }),
      ..Default::default()
    };
    let history = CommandHistory::new();
    let context = egui::Context::default();

    assert!(
      run_editor_frame(&context, &mut controller, &document, &history, Vec::new()).is_empty()
    );
    let actions = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![Event::Ime(ImeEvent::Commit("dictated".to_owned())), enter_event(Modifiers::NONE)],
    );

    assert!(controller.text_editing.is_none());
    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one text add, got {actions:?}");
    };
    assert!(matches!(
      batch.commands(),
      [DocumentCommand::AddElement {
        element: Element {
          payload: ElementPayload::Text(TextPayload { text, .. }),
          ..
        },
      }] if text == "dictated"
    ));
  }

  #[test]
  fn focus_loss_clears_inline_ime_state() {
    let mut ime =
      InlineImeState { preedit: Some("候选".to_owned()), confirming_preedit_with_enter: true };
    let mut events = vec![Event::WindowFocused(false)];

    assert!(!normalize_inline_text_events(&mut events, &mut ime));
    assert_eq!(events, vec![Event::WindowFocused(false)]);
    assert!(ime.preedit.is_none());
    assert!(!ime.confirming_preedit_with_enter);
  }

  #[test]
  fn ime_cancel_clears_inline_ime_state() {
    let mut ime =
      InlineImeState { preedit: Some("候选".to_owned()), confirming_preedit_with_enter: true };
    let mut events =
      vec![Event::Ime(ImeEvent::Preedit { text: String::new(), active_range_chars: None })];

    assert!(!normalize_inline_text_events(&mut events, &mut ime));
    assert_eq!(
      events,
      vec![Event::Ime(ImeEvent::Preedit { text: String::new(), active_range_chars: None })]
    );
    assert!(ime.preedit.is_none());
    assert!(!ime.confirming_preedit_with_enter);
  }

  #[test]
  fn rectangle_ime_confirmation_keeps_editing_then_second_enter_submits() {
    let mut document = document();
    let element = rectangle(&document, 0, PointPx::new(80.0, 100.0), PointPx::new(220.0, 170.0));
    let element_id = element.element_id;
    let ElementPayload::Rectangle(payload) = &element.payload else {
      unreachable!();
    };
    let text_style = payload.label.text_style.clone();
    document.elements.push(element);
    let mut controller = EditorController {
      selected_element_id: Some(element_id),
      text_editing: Some(TextEditing {
        target: TextTarget::RectangleLabel { element_id },
        buffer: String::new(),
        text_style,
        ime: InlineImeState::default(),
        request_focus: true,
        select_all: false,
        auto_place_rectangle: false,
      }),
      ..Default::default()
    };
    let history = CommandHistory::new();
    let context = egui::Context::default();

    assert!(
      run_editor_frame(&context, &mut controller, &document, &history, Vec::new()).is_empty()
    );
    assert!(
      run_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![Event::Ime(ImeEvent::Preedit {
          text: "O".to_owned(),
          active_range_chars: Some(0..1),
        })],
      )
      .is_empty()
    );
    assert!(
      run_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![enter_event(Modifiers::NONE), Event::Ime(ImeEvent::Commit("OK\n".to_owned()))],
      )
      .is_empty()
    );
    assert_eq!(controller.text_editing.as_ref().map(|editing| editing.buffer.as_str()), Some("OK"));

    let actions = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![enter_event(Modifiers::NONE)],
    );
    assert!(controller.text_editing.is_none());
    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one rectangle label update, got {actions:?}");
    };
    assert!(matches!(
      batch.commands(),
      [DocumentCommand::UpdateElementLabel { element_id: updated, text: Some(text) }]
        if *updated == element_id && text == "OK"
    ));
  }

  #[test]
  fn existing_text_commit_updates_width_and_history_restores_both_fields() {
    let mut document = document();
    let element = text_element(&document, PointPx::new(100.0, 40.0), "legacy", 80.0);
    let element_id = element.element_id;
    let ElementPayload::Text(payload) = &element.payload else {
      unreachable!();
    };
    let text_style = payload.text_style.clone();
    document.elements.push(element);
    let mut controller = EditorController {
      text_editing: Some(TextEditing {
        target: TextTarget::ExistingText { element_id },
        buffer: "updated\ntext".to_owned(),
        text_style,
        ime: InlineImeState::default(),
        request_focus: false,
        select_all: false,
        auto_place_rectangle: false,
      }),
      ..Default::default()
    };
    let mut actions = Vec::new();
    controller.commit_text(&document, &mut actions);
    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one text update, got {actions:?}");
    };

    let mut history = CommandHistory::new();
    history.execute_batch(&mut document, batch.clone()).unwrap();
    let ElementPayload::Text(updated) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(updated.text, "updated\ntext");
    assert_eq!(updated.box_width_px, 300.0);

    assert!(history.undo(&mut document).unwrap());
    let ElementPayload::Text(undone) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(undone.text, "legacy");
    assert_eq!(undone.box_width_px, 80.0);

    assert!(history.redo(&mut document).unwrap());
    let ElementPayload::Text(redone) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(redone.text, "updated\ntext");
    assert_eq!(redone.box_width_px, 300.0);
  }

  #[test]
  fn new_text_commit_persists_width_to_the_canvas_edge() {
    let document = document();
    let mut controller = EditorController {
      text_editing: Some(TextEditing {
        target: TextTarget::NewText { anchor_px: PointPx::new(120.0, 40.0) },
        buffer: "new text".to_owned(),
        text_style: TextStyle::mvp(ColorRgba::WHITE, 24.0).unwrap(),
        ime: InlineImeState::default(),
        request_focus: false,
        select_all: false,
        auto_place_rectangle: false,
      }),
      ..Default::default()
    };
    let mut actions = Vec::new();

    controller.commit_text(&document, &mut actions);

    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one added text element, got {actions:?}");
    };
    assert!(matches!(
      batch.commands(),
      [DocumentCommand::AddElement {
        element: Element {
          payload: ElementPayload::Text(TextPayload { box_width_px, .. }),
          ..
        },
      }] if *box_width_px == 280.0
    ));
  }

  #[test]
  fn existing_text_commit_updates_legacy_width_when_content_is_unchanged() {
    let mut document = document();
    let element = text_element(&document, PointPx::new(120.0, 40.0), "same", 40.0);
    let element_id = element.element_id;
    let ElementPayload::Text(payload) = &element.payload else {
      unreachable!();
    };
    let text_style = payload.text_style.clone();
    document.elements.push(element);
    let mut controller = EditorController {
      text_editing: Some(TextEditing {
        target: TextTarget::ExistingText { element_id },
        buffer: "same".to_owned(),
        text_style,
        ime: InlineImeState::default(),
        request_focus: false,
        select_all: false,
        auto_place_rectangle: false,
      }),
      ..Default::default()
    };
    let mut actions = Vec::new();

    controller.commit_text(&document, &mut actions);

    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected a width-only text update, got {actions:?}");
    };
    assert!(matches!(
      batch.commands(),
      [DocumentCommand::UpdateElement {
        element_id: updated,
        payload: ElementPayload::Text(TextPayload { text, box_width_px, .. }),
      }] if *updated == element_id && text == "same" && *box_width_px == 280.0
    ));
  }

  #[test]
  fn rectangle_inline_geometry_tracks_draft_and_empty_label() {
    let mut document = document();
    let element = rectangle(&document, 0, PointPx::new(80.0, 90.0), PointPx::new(180.0, 160.0));
    let element_id = element.element_id;
    let ElementPayload::Rectangle(payload) = &element.payload else {
      unreachable!();
    };
    let text_style = payload.label.text_style.clone();
    document.elements.push(element);
    let mut editing = TextEditing {
      target: TextTarget::RectangleLabel { element_id },
      buffer: "OK".to_owned(),
      text_style,
      ime: InlineImeState::default(),
      request_focus: false,
      select_all: false,
      auto_place_rectangle: false,
    };

    let geometry = inline_text_geometry(&editing, &document).unwrap();
    let ElementPayload::Rectangle(payload) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    let draft = rectangle_label_draft(payload, &editing, &document, element_id);
    let layout = rectangle_label_layout(&draft, document.canvas_size_px).unwrap().unwrap();
    let label_inner_width = (layout.bounds_px.width() - draft.label.padding_px * 2.0).max(1.0);
    assert_eq!(geometry.wrap_width_px, layout.text_wrap_width_px);
    assert_eq!(geometry.editor_width_px, label_inner_width);
    assert_eq!(
      geometry.origin_px,
      layout.bounds_px.min + PointPx::new(draft.label.padding_px, draft.label.padding_px)
    );
    assert_eq!(layout.text_layout.line_count, 1);
    assert!(layout.text_wrap_width_px > layout.bounds_px.width() - draft.label.padding_px * 2.0);
    assert!(geometry.wrap_width_px > geometry.editor_width_px);

    editing.buffer.clear();
    let empty_draft = rectangle_label_draft(payload, &editing, &document, element_id);
    let empty_layout =
      rectangle_label_layout(&empty_draft, document.canvas_size_px).unwrap().unwrap();
    let empty_geometry = inline_text_geometry(&editing, &document).unwrap();
    assert_eq!(empty_draft.label.text.as_deref(), Some(EMPTY_LABEL_DRAFT));
    assert_eq!(empty_layout.text_layout.width_px, 1.0);
    assert_eq!(empty_geometry.wrap_width_px, empty_layout.text_wrap_width_px);
    assert_eq!(
      empty_geometry.editor_width_px,
      (empty_layout.bounds_px.width() - empty_draft.label.padding_px * 2.0).max(1.0)
    );
  }

  #[test]
  fn pending_rectangle_editor_has_no_fallback_origin() {
    let document = document();
    let editing = TextEditing {
      target: TextTarget::RectangleLabel { element_id: ElementId::new() },
      buffer: DEFAULT_RECTANGLE_LABEL.to_owned(),
      text_style: TextStyle::mvp(ColorRgba::WHITE, 24.0).unwrap(),
      ime: InlineImeState::default(),
      request_focus: true,
      select_all: true,
      auto_place_rectangle: false,
    };

    assert!(inline_text_geometry(&editing, &document).is_none());
  }

  #[test]
  fn inline_editor_is_frameless_visible_and_not_capped_at_default_area_width() {
    let mut document = document_with_size(SizePx::new(1000, 200));
    let element = text_element(&document, PointPx::new(20.0, 40.0), "A".repeat(60), 120.0);
    let element_id = element.element_id;
    document.elements.push(element);
    let mut controller = EditorController::default();
    controller.start_editing_existing(&document, element_id, false, &mut Vec::new());
    let transform = CanvasTransform::fit(
      document.canvas_size_px,
      Rect::from_min_size(Pos2::ZERO, egui::vec2(1000.0, 200.0)),
    )
    .unwrap();
    let context = egui::Context::default();
    let output = context.run_ui(raw_input(Vec::new(), egui::vec2(1000.0, 200.0)), |ui| {
      assert_eq!(
        controller.show_text_editor(&context, transform, &document, false),
        TextEditorCompletion::None
      );
      controller.paint_document_for_editing(ui.painter(), transform, &document);
    });
    let text_shape_count =
      output.shapes.iter().filter(|shape| matches!(shape.shape, egui::Shape::Text(_))).count();
    let text_row_count = output.shapes.iter().find_map(|shape| match &shape.shape {
      egui::Shape::Text(text) => Some(text.galley.rows.len()),
      _ => None,
    });
    let rectangle_shapes_are_invisible = output.shapes.iter().all(|shape| match &shape.shape {
      egui::Shape::Rect(rectangle) => {
        rectangle.fill == Color32::TRANSPARENT && rectangle.stroke == Stroke::NONE
      }
      _ => true,
    });
    output.drop_without_applying_deltas();
    assert_eq!(text_shape_count, 1);
    assert_eq!(text_row_count, Some(1));
    assert!(rectangle_shapes_are_invisible);
  }

  #[test]
  fn active_text_edit_hides_the_element_selection_chrome() {
    let mut document = document();
    let element = rectangle(&document, 0, PointPx::new(80.0, 90.0), PointPx::new(220.0, 170.0));
    let element_id = element.element_id;
    let ElementPayload::Rectangle(payload) = &element.payload else {
      unreachable!();
    };
    let text_style = payload.label.text_style.clone();
    let element_bounds = element.bounds_px;
    document.elements.push(element);
    let mut controller = EditorController {
      selected_element_id: Some(element_id),
      text_editing: Some(TextEditing {
        target: TextTarget::RectangleLabel { element_id },
        buffer: DEFAULT_RECTANGLE_LABEL.to_owned(),
        text_style,
        ime: InlineImeState::default(),
        request_focus: true,
        select_all: false,
        auto_place_rectangle: false,
      }),
      ..Default::default()
    };
    let history = CommandHistory::new();
    let context = egui::Context::default();
    let output = context.run_ui(raw_input(Vec::new(), egui::vec2(800.0, 400.0)), |ui| {
      assert!(controller.show(ui, &document, &history, None).is_empty());
    });
    let transform = CanvasTransform::fit(
      document.canvas_size_px,
      Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 400.0)),
    )
    .unwrap();
    let selection_rect = transform.document_rect_to_egui(element_bounds).expand(3.0);
    let paints_selection_outline = output.shapes.iter().any(|shape| match &shape.shape {
      egui::Shape::Rect(rectangle) => {
        rectangle.rect.min.distance(selection_rect.min) < 0.1
          && rectangle.rect.max.distance(selection_rect.max) < 0.1
          && rectangle.stroke == Stroke::new(1.0, Color32::WHITE)
      }
      _ => false,
    });
    output.drop_without_applying_deltas();

    assert!(!paints_selection_outline);
  }

  #[test]
  fn rectangle_background_updates_in_the_same_frame_as_ime_commit() {
    let mut document = document();
    let mut element =
      rectangle(&document, 0, PointPx::new(340.0, 100.0), PointPx::new(400.0, 170.0));
    let ElementPayload::Rectangle(payload) = &mut element.payload else {
      unreachable!();
    };
    payload.label.anchor_offset_px = 30.0;
    payload.label_anchor.position = 0.5;
    element.refresh_bounds(document.canvas_size_px).unwrap();
    let element_id = element.element_id;
    let ElementPayload::Rectangle(payload) = &element.payload else {
      unreachable!();
    };
    let text_style = payload.label.text_style.clone();
    document.elements.push(element);
    let mut controller = EditorController {
      text_editing: Some(TextEditing {
        target: TextTarget::RectangleLabel { element_id },
        buffer: String::new(),
        text_style,
        ime: InlineImeState::default(),
        request_focus: true,
        select_all: false,
        auto_place_rectangle: false,
      }),
      ..Default::default()
    };
    let transform = CanvasTransform::fit(
      document.canvas_size_px,
      Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 200.0)),
    )
    .unwrap();
    let context = egui::Context::default();
    fn run_frame(
      context: &egui::Context,
      controller: &mut EditorController,
      transform: CanvasTransform,
      document: &BoardDocument,
      events: Vec<Event>,
    ) -> egui::FullOutput {
      context.run_ui(raw_input(events, egui::vec2(400.0, 200.0)), |ui| {
        assert_eq!(
          controller.show_text_editor(context, transform, document, false),
          TextEditorCompletion::None
        );
        controller.paint_document_for_editing(ui.painter(), transform, document);
      })
    }
    run_frame(&context, &mut controller, transform, &document, Vec::new())
      .drop_without_applying_deltas();
    let preedit_output = run_frame(
      &context,
      &mut controller,
      transform,
      &document,
      vec![Event::Ime(ImeEvent::Preedit { text: "O".to_owned(), active_range_chars: Some(0..1) })],
    );
    let preedit_origin =
      inline_text_geometry(controller.text_editing.as_ref().unwrap(), &document).unwrap().origin_px;
    let preedit_ime_rect = preedit_output.platform_output.ime.map(|ime| ime.rect);
    preedit_output.drop_without_applying_deltas();
    assert!(
      preedit_ime_rect.is_some_and(|rect| {
        rect.min.distance(transform.document_to_egui(preedit_origin)) < 0.1
      })
    );
    let output = run_frame(
      &context,
      &mut controller,
      transform,
      &document,
      vec![enter_event(Modifiers::NONE), Event::Ime(ImeEvent::Commit("OK\n".to_owned()))],
    );

    let editing = controller.text_editing.as_ref().unwrap();
    assert_eq!(editing.buffer, "OK");
    let ElementPayload::Rectangle(payload) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    let draft = rectangle_label_draft(payload, editing, &document, element_id);
    let expected_layout = rectangle_label_layout(&draft, document.canvas_size_px).unwrap().unwrap();
    let expected_label_rect = transform.document_rect_to_egui(expected_layout.bounds_px);
    let expected_geometry = inline_text_geometry(editing, &document).unwrap();
    let expected_text_position = transform.document_to_egui(expected_geometry.origin_px);
    let colored_rectangles = output
      .shapes
      .iter()
      .filter_map(|shape| match &shape.shape {
        egui::Shape::Rect(rectangle) if rectangle.fill != Color32::TRANSPARENT => Some(rectangle),
        _ => None,
      })
      .collect::<Vec<_>>();
    let text_row_counts = output
      .shapes
      .iter()
      .filter_map(|shape| match &shape.shape {
        egui::Shape::Text(text) => Some(text.galley.rows.len()),
        _ => None,
      })
      .collect::<Vec<_>>();
    let text_positions = output
      .shapes
      .iter()
      .filter_map(|shape| match &shape.shape {
        egui::Shape::Text(text) => Some(text.pos),
        _ => None,
      })
      .collect::<Vec<_>>();
    let text_clip_rects = output
      .shapes
      .iter()
      .filter_map(|shape| matches!(shape.shape, egui::Shape::Text(_)).then_some(shape.clip_rect))
      .collect::<Vec<_>>();
    let background_matches = colored_rectangles.iter().any(|rectangle| {
      rectangle.rect.min.distance(expected_label_rect.min) < 0.1
        && rectangle.rect.max.distance(expected_label_rect.max) < 0.1
    });
    output.drop_without_applying_deltas();

    assert!(background_matches);
    assert_eq!(text_row_counts, vec![1]);
    assert!(preedit_origin.distance_to(expected_geometry.origin_px) > 1.0);
    assert_eq!(text_positions.len(), 1);
    assert!(text_positions[0].distance(expected_text_position) < 0.1);
    assert_eq!(text_clip_rects.len(), 1);
    assert!(text_clip_rects[0].min.distance(transform.canvas_rect().min) < 0.1);
    assert!(text_clip_rects[0].max.distance(transform.canvas_rect().max) < 0.1);
  }

  #[test]
  fn long_new_arrow_enters_empty_label_editing_and_blank_submit_keeps_only_the_arrow() {
    let mut document = document();
    let interaction = PointerInteraction::Draw {
      element_id: ElementId::new(),
      tool: EditorTool::Arrow,
      start: PointPx::new(40.0, 100.0),
      current: PointPx::new(360.0, 100.0),
      stroke_points: Vec::new(),
    };
    let mut controller = EditorController { interaction: Some(interaction), ..Default::default() };
    let mut actions = Vec::new();

    controller.finish_pointer_interaction(&document, &mut actions);

    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one arrow add, got {actions:?}");
    };
    let [DocumentCommand::AddElement { element }] = batch.commands() else {
      panic!("expected AddElement");
    };
    let element_id = element.element_id;
    assert!(matches!(
      controller.text_editing.as_ref(),
      Some(TextEditing { target: TextTarget::ArrowLabel { element_id: editing_id }, buffer, .. })
        if *editing_id == element_id && buffer.is_empty()
    ));

    batch.clone().apply(&mut document).unwrap();
    let mut commit_actions = Vec::new();
    controller.commit_text(&document, &mut commit_actions);
    assert!(commit_actions.is_empty());
    let ElementPayload::Arrow(payload) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(payload.label.text, None);
  }

  #[test]
  fn pending_arrow_label_commit_makes_a_clean_document_dirty_before_quit_decision() {
    let mut document = document();
    let mut element = arrow(&document, 0, PointPx::new(30.0, 100.0), PointPx::new(370.0, 100.0));
    let element_id = element.element_id;
    let ElementPayload::Arrow(payload) = &mut element.payload else {
      unreachable!();
    };
    payload.label.text = Some("原内容".to_owned());
    element.refresh_bounds(document.canvas_size_px).unwrap();
    document.elements.push(element);
    document.validate().unwrap();
    let baseline = document.dirty_baseline();
    let revision = document.revision;
    let mut history = CommandHistory::new();
    let mut controller = EditorController::new(EditorTool::Select);
    let mut edit_actions = Vec::new();
    controller.start_editing_existing(&document, element_id, false, &mut edit_actions);
    assert!(edit_actions.is_empty());
    controller.text_editing.as_mut().unwrap().buffer = "退出前提交".to_owned();

    let actions = controller.commit_pending_text(&document);

    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one pending label update, got {actions:?}");
    };
    history.execute_batch(&mut document, batch.clone()).unwrap();
    assert!(controller.text_editing.is_none());
    assert_eq!(document.revision, revision + 1);
    assert!(document.is_dirty_against(baseline));
    let ElementPayload::Arrow(payload) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(payload.label.text.as_deref(), Some("退出前提交"));
  }

  #[test]
  fn arrow_label_click_edit_clear_and_escape_preserve_the_expected_content() {
    let mut document = document();
    let mut element = arrow(&document, 0, PointPx::new(30.0, 100.0), PointPx::new(370.0, 100.0));
    let element_id = element.element_id;
    let ElementPayload::Arrow(payload) = &mut element.payload else {
      unreachable!();
    };
    payload.label.text = Some("原内容".to_owned());
    element.refresh_bounds(document.canvas_size_px).unwrap();
    document.elements.push(element);
    document.validate().unwrap();
    let label_center = arrow_label_layout(
      match &document.element(element_id).unwrap().payload {
        ElementPayload::Arrow(payload) => payload,
        _ => unreachable!(),
      },
      document.canvas_size_px,
    )
    .unwrap()
    .unwrap()
    .bounds_px
    .center();
    assert!(single_click_starts_editing(&document, element_id, label_center));

    let mut controller = EditorController::new(EditorTool::Select);
    let context = egui::Context::default();
    let history = CommandHistory::new();
    let transform = CanvasTransform::fit(
      document.canvas_size_px,
      Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 400.0)),
    )
    .unwrap();
    let pointer = transform.document_to_egui(label_center);
    assert!(
      run_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![Event::PointerMoved(pointer)],
      )
      .is_empty()
    );
    assert!(
      run_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![Event::PointerButton {
          pos: pointer,
          button: egui::PointerButton::Primary,
          pressed: true,
          modifiers: Modifiers::NONE,
        }],
      )
      .is_empty()
    );
    assert!(
      run_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![Event::PointerButton {
          pos: pointer,
          button: egui::PointerButton::Primary,
          pressed: false,
          modifiers: Modifiers::NONE,
        }],
      )
      .is_empty()
    );
    assert!(matches!(
      controller.text_editing.as_ref().map(|editing| &editing.target),
      Some(TextTarget::ArrowLabel { element_id: editing_id }) if *editing_id == element_id
    ));
    controller.text_editing.as_mut().unwrap().buffer = "临时修改".to_owned();
    let escape_actions = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![Event::Key {
        key: Key::Escape,
        physical_key: Some(Key::Escape),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
      }],
    );
    assert!(escape_actions.is_empty());
    assert!(controller.text_editing.is_none());

    let mut actions = Vec::new();
    controller.start_editing_existing(&document, element_id, false, &mut actions);
    controller.text_editing.as_mut().unwrap().buffer = " \n\t ".to_owned();
    controller.commit_text(&document, &mut actions);
    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one label clear, got {actions:?}");
    };
    assert!(matches!(
      batch.commands(),
      [DocumentCommand::UpdateElementLabel { element_id: updated, text: None }]
        if *updated == element_id
    ));
    batch.clone().apply(&mut document).unwrap();
    let ElementPayload::Arrow(payload) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(payload.label.text, None);
  }

  #[test]
  fn short_hidden_arrow_double_click_request_emits_one_toast() {
    let mut document = document();
    let element = arrow(&document, 0, PointPx::new(100.0, 100.0), PointPx::new(130.0, 100.0));
    let element_id = element.element_id;
    document.elements.push(element);
    let mut controller = EditorController::new(EditorTool::Select);
    let mut actions = Vec::new();

    controller.start_editing_existing(&document, element_id, true, &mut actions);

    assert_eq!(actions, vec![EditorAction::Toast(ARROW_LABEL_TOO_SHORT_TOAST.to_owned())]);
    assert!(controller.text_editing.is_none());
  }

  #[test]
  fn visible_arrow_endpoint_and_style_constraints_emit_one_toast_without_invalid_commands() {
    let mut document = document();
    let mut element = arrow(&document, 0, PointPx::new(40.0, 100.0), PointPx::new(190.0, 100.0));
    let element_id = element.element_id;
    let ElementPayload::Arrow(payload) = &mut element.payload else {
      unreachable!();
    };
    payload.label.text = Some("标签".to_owned());
    element.refresh_bounds(document.canvas_size_px).unwrap();
    document.elements.push(element);
    document.validate().unwrap();

    let mut controller = EditorController {
      selected_element_id: Some(element_id),
      interaction: Some(PointerInteraction::UpdateArrowEndpoint {
        element_id,
        endpoint: ArrowEndpoint::End,
        current: PointPx::new(50.0, 100.0),
      }),
      ..Default::default()
    };
    let mut actions = Vec::new();
    controller.finish_pointer_interaction(&document, &mut actions);
    let [EditorAction::Command(batch), EditorAction::Toast(message)] = actions.as_slice() else {
      panic!("expected a clamped endpoint and one toast, got {actions:?}");
    };
    assert_eq!(message, ARROW_LABEL_TOO_SHORT_TOAST);
    let before_revision = document.revision;
    let mut history = CommandHistory::new();
    history.execute_batch(&mut document, batch.clone()).unwrap();
    assert_eq!(document.revision, before_revision + 1);
    let ElementPayload::Arrow(payload) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    assert!(
      (payload.start_px.distance_to(payload.end_px)
        - arrow_minimum_length_for_label(payload).unwrap())
      .abs()
        < 0.001
    );
    assert!(history.undo(&mut document).unwrap());

    let before = document.clone();
    for change in [
      StyleChange { font_size_px: Some(64.0), ..StyleChange::default() },
      StyleChange { width_px: Some(12.0), ..StyleChange::default() },
    ] {
      let mut style_actions = Vec::new();
      controller.apply_style_change(&document, change, &mut style_actions);
      assert_eq!(style_actions, vec![EditorAction::Toast(ARROW_LABEL_TOO_SHORT_TOAST.to_owned())]);
    }
    assert_eq!(document, before);
  }

  #[test]
  fn arrow_style_preflight_uses_a_label_commit_queued_in_the_same_frame() {
    let mut document = document();
    let element = arrow(&document, 0, PointPx::new(40.0, 100.0), PointPx::new(190.0, 100.0));
    let element_id = element.element_id;
    document.elements.push(element);
    let mut controller =
      EditorController { selected_element_id: Some(element_id), ..Default::default() };

    let mut reveal_actions = Vec::new();
    controller.start_editing_existing(&document, element_id, true, &mut reveal_actions);
    controller.text_editing.as_mut().unwrap().buffer = "新标签".to_owned();
    controller.commit_text(&document, &mut reveal_actions);
    controller.apply_style_change(
      &document,
      StyleChange { font_size_px: Some(64.0), ..StyleChange::default() },
      &mut reveal_actions,
    );
    assert!(matches!(
      reveal_actions.as_slice(),
      [
        EditorAction::Command(label_batch),
        EditorAction::Toast(message),
      ] if matches!(
        label_batch.commands(),
        [DocumentCommand::UpdateElementLabel { element_id: updated, text: Some(text) }]
          if *updated == element_id && text == "新标签"
      ) && message == ARROW_LABEL_TOO_SHORT_TOAST
    ));
    let EditorAction::Command(label_batch) = &reveal_actions[0] else {
      unreachable!();
    };
    label_batch.clone().apply(&mut document).unwrap();

    let mut clear_actions = Vec::new();
    controller.start_editing_existing(&document, element_id, false, &mut clear_actions);
    controller.text_editing.as_mut().unwrap().buffer = " \n\t ".to_owned();
    controller.commit_text(&document, &mut clear_actions);
    controller.apply_style_change(
      &document,
      StyleChange { font_size_px: Some(64.0), ..StyleChange::default() },
      &mut clear_actions,
    );
    let [EditorAction::Command(clear_batch), EditorAction::Command(style_batch)] =
      clear_actions.as_slice()
    else {
      panic!("expected clear then style commands, got {clear_actions:?}");
    };
    clear_batch.clone().apply(&mut document).unwrap();
    style_batch.clone().apply(&mut document).unwrap();
    let ElementPayload::Arrow(payload) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(payload.label.text, None);
    assert_eq!(payload.label.text_style.font_size_px, 64.0);
  }

  #[test]
  fn rectangle_collision_inputs_ignore_non_rectangles_and_label_drag_is_one_command() {
    let mut document = document();
    let rectangle = rectangle(&document, 0, PointPx::new(120.0, 80.0), PointPx::new(240.0, 160.0));
    let rectangle_id = rectangle.element_id;
    document.elements.push(rectangle);
    document.elements.push(arrow(
      &document,
      1,
      PointPx::new(20.0, 20.0),
      PointPx::new(380.0, 180.0),
    ));
    document.elements.push(text_element(&document, PointPx::new(40.0, 40.0), "不参与碰撞", 160.0));
    assert!(rectangle_label_obstacles(&document, Some(rectangle_id)).is_empty());

    let ElementPayload::Rectangle(payload) = &document.element(rectangle_id).unwrap().payload
    else {
      unreachable!();
    };
    let current_layout = rectangle_label_layout(payload, document.canvas_size_px).unwrap().unwrap();
    let desired_center = PointPx::new(payload.end_px.x_px + 30.0, payload.end_px.y_px - 20.0);
    let expected =
      snap_rectangle_label_layout(payload, document.canvas_size_px, desired_center).unwrap();
    let mut controller = EditorController {
      interaction: Some(PointerInteraction::DragRectangleLabel {
        element_id: rectangle_id,
        current: desired_center,
        grab_offset_px: PointPx::ZERO,
      }),
      ..Default::default()
    };
    let mut actions = Vec::new();
    controller.finish_pointer_interaction(&document, &mut actions);
    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one anchor command, got {actions:?}");
    };
    assert!(matches!(
      batch.commands(),
      [DocumentCommand::SetRectangleLabelPlacement { element_id, actual_anchor, .. }]
        if *element_id == rectangle_id && *actual_anchor == expected.anchor
    ));
    assert_ne!(expected.bounds_px, current_layout.bounds_px);
    let before_revision = document.revision;
    let mut history = CommandHistory::new();
    history.execute_batch(&mut document, batch.clone()).unwrap();
    assert_eq!(document.revision, before_revision + 1);
    assert!(history.undo(&mut document).unwrap());
  }

  #[test]
  fn rectangle_label_drag_past_seventy_percent_overhang_switches_track_consistently() {
    let mut document = document_with_size(SizePx::new(600, 400));
    let element = rectangle(&document, 0, PointPx::new(150.0, 160.0), PointPx::new(390.0, 300.0));
    let element_id = element.element_id;
    let before_element = element.clone();
    let ElementPayload::Rectangle(payload) = &element.payload else {
      unreachable!();
    };
    let initial_layout = rectangle_label_layout(payload, document.canvas_size_px).unwrap().unwrap();
    let label_width = initial_layout.bounds_px.width();
    let desired_center = PointPx::new(
      payload.end_px.x_px - label_width * 0.25 + label_width / 2.0,
      initial_layout.bounds_px.center().y_px,
    );
    document.elements.push(element);
    let interaction = PointerInteraction::DragRectangleLabel {
      element_id,
      current: desired_center,
      grab_offset_px: PointPx::ZERO,
    };
    let preview_controller = EditorController::default();

    let preview = interaction_preview_set(&preview_controller, &interaction, &document);
    let preview_element = preview.get(element_id).expect("dragged label should be previewed");
    let ElementPayload::Rectangle(preview_payload) = &preview_element.payload else {
      unreachable!();
    };
    let preview_preferred = preview_payload.preferred_label_anchor;
    let preview_actual = preview_payload.label_anchor;
    assert_ne!(
      (preview_preferred.edge, preview_preferred.side),
      (RectangleLabelEdge::Top, RectangleLabelSide::Outside)
    );
    assert_eq!(preview_actual, preview_preferred);

    let mut controller = EditorController { interaction: Some(interaction), ..Default::default() };
    let mut actions = Vec::new();
    controller.finish_pointer_interaction(&document, &mut actions);
    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one label placement batch, got {actions:?}");
    };
    let (command_preferred, command_actual) =
      rectangle_label_placement_command(batch.commands(), element_id)
        .expect("label placement command should be present");
    assert_eq!(command_preferred, preview_preferred);
    assert_eq!(command_actual, preview_actual);

    let before_revision = document.revision;
    let mut history = CommandHistory::new();
    history.execute_batch(&mut document, batch.clone()).unwrap();
    assert_eq!(document.revision, before_revision + 1);
    assert_eq!(history.undo_len(), 1);
    let ElementPayload::Rectangle(committed_payload) =
      &document.element(element_id).unwrap().payload
    else {
      unreachable!();
    };
    assert_eq!(committed_payload.preferred_label_anchor, preview_preferred);
    assert_eq!(committed_payload.label_anchor, preview_actual);

    assert!(history.undo(&mut document).unwrap());
    assert_eq!(document.element(element_id), Some(&before_element));
  }

  #[test]
  fn rectangle_label_drag_at_top_right_corner_then_reflows_down_on_intrusion() {
    let mut document = document_with_size(SizePx::new(942, 332));
    let mut element =
      rectangle(&document, 0, PointPx::new(51.0, 132.0), PointPx::new(347.0, 288.0));
    let element_id = element.element_id;
    let ElementPayload::Rectangle(payload) = &mut element.payload else {
      unreachable!();
    };
    payload.label.text = Some(
      "abcdefghijklmnopqrstuv\nabcdefghijklmnopqrstuv\nabcdefghijklmnopqrstuv\nabcdefghijklmnopqrstuv"
        .to_owned(),
    );
    payload.label.max_width_px = 340.0;
    payload.label.text_style.line_height_px = 25.75;
    element.refresh_bounds(document.canvas_size_px).unwrap();
    document.elements.push(element);
    let mut intruder =
      rectangle(&document, 1, PointPx::new(543.0, 0.0), PointPx::new(841.0, 125.0));
    let intruder_id = intruder.element_id;
    let ElementPayload::Rectangle(payload) = &mut intruder.payload else {
      unreachable!();
    };
    payload.label.text = None;
    intruder.refresh_bounds(document.canvas_size_px).unwrap();
    document.elements.push(intruder);
    let ElementPayload::Rectangle(payload) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    let corner =
      RectangleLabelAnchor::new(RectangleLabelEdge::Right, RectangleLabelSide::Outside, 0.0);
    let desired_center =
      common::rectangle_label_layout_at_anchor(payload, corner, document.canvas_size_px)
        .unwrap()
        .unwrap()
        .bounds_px
        .center();
    let mut controller = EditorController {
      interaction: Some(PointerInteraction::DragRectangleLabel {
        element_id,
        current: desired_center,
        grab_offset_px: PointPx::ZERO,
      }),
      ..Default::default()
    };
    let mut actions = Vec::new();

    controller.finish_pointer_interaction(&document, &mut actions);

    let [EditorAction::Command(drag_batch)] = actions.as_slice() else {
      panic!("expected one label placement batch, got {actions:?}");
    };
    assert!(matches!(
      drag_batch.commands(),
      [DocumentCommand::SetRectangleLabelPlacement {
        element_id: updated_id,
        preferred_anchor,
        actual_anchor,
      }] if *updated_id == element_id
        && preferred_anchor.edge == RectangleLabelEdge::Right
        && preferred_anchor.side == RectangleLabelSide::Outside
        && preferred_anchor.position.abs() < 0.001
        && *preferred_anchor == *actual_anchor
    ));

    let mut history = CommandHistory::new();
    history.execute_batch(&mut document, drag_batch.clone()).unwrap();
    let move_batch = rectangle_reflow_batch(
      &document,
      vec![DocumentCommand::MoveElement {
        element_id: intruder_id,
        delta_px: PointPx::new(0.0, 53.0),
      }],
      intruder_id,
      &[],
    )
    .unwrap();
    let (_, actual_anchor) =
      rectangle_label_placement_command(move_batch.commands(), element_id).unwrap();
    assert_eq!(actual_anchor.edge, RectangleLabelEdge::Right);
    assert_eq!(actual_anchor.side, RectangleLabelSide::Outside);
    assert!(actual_anchor.position > 0.0);
  }

  #[test]
  fn rectangle_label_drag_back_to_blocked_preferred_track_is_ignored() {
    let mut document = document_with_size(SizePx::new(420, 500));
    let mut upper = rectangle(&document, 0, PointPx::new(80.0, 50.0), PointPx::new(280.0, 140.0));
    let ElementPayload::Rectangle(payload) = &mut upper.payload else {
      unreachable!();
    };
    payload.label.text = None;
    upper.refresh_bounds(document.canvas_size_px).unwrap();
    document.elements.push(upper);

    let lower = rectangle(&document, 1, PointPx::new(80.0, 210.0), PointPx::new(280.0, 290.0));
    let lower_id = lower.element_id;
    document.elements.push(lower);
    document.validate().unwrap();

    let long_text = "第一行\n第二行\n第三行\n第四行".to_owned();
    let text_batch = rectangle_reflow_batch(
      &document,
      vec![DocumentCommand::UpdateElementLabel { element_id: lower_id, text: Some(long_text) }],
      lower_id,
      &[],
    )
    .expect("long label should reflow away from the blocked top track");
    let (_, backed_off_actual) = rectangle_label_placement_command(text_batch.commands(), lower_id)
      .expect("long label should include a placement command");
    assert_eq!(backed_off_actual.edge, RectangleLabelEdge::Bottom);
    assert_eq!(backed_off_actual.side, RectangleLabelSide::Outside);

    let mut history = CommandHistory::new();
    history.execute_batch(&mut document, text_batch).unwrap();
    let ElementPayload::Rectangle(payload) = &document.element(lower_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(payload.preferred_label_anchor.edge, RectangleLabelEdge::Top);
    assert_eq!(payload.preferred_label_anchor.side, RectangleLabelSide::Outside);
    assert_eq!(payload.label_anchor, backed_off_actual);

    let blocked_layout = common::rectangle_label_layout_at_anchor(
      payload,
      payload.preferred_label_anchor,
      document.canvas_size_px,
    )
    .unwrap()
    .unwrap();
    let interaction = PointerInteraction::DragRectangleLabel {
      element_id: lower_id,
      current: blocked_layout.bounds_px.center(),
      grab_offset_px: PointPx::ZERO,
    };

    let preview = interaction_preview_set(&EditorController::default(), &interaction, &document);
    assert!(
      preview.get(lower_id).is_none(),
      "blocked drag should not preview an uncommittable outer-track placement"
    );

    let mut controller = EditorController { interaction: Some(interaction), ..Default::default() };
    let mut actions = Vec::new();
    controller.finish_pointer_interaction(&document, &mut actions);

    assert!(actions.is_empty(), "blocked drag should not emit a no-op placement command");
  }

  #[test]
  fn rectangle_move_preview_reflows_connected_neighbor_labels() {
    let mut document = document_with_size(SizePx::new(420, 260));
    let mut first = rectangle(&document, 0, PointPx::new(60.0, 100.0), PointPx::new(170.0, 180.0));
    first.element_id = ElementId::from_uuid(Uuid::from_u128(1));
    let first_id = first.element_id;
    document.elements.push(first);
    let mut second =
      rectangle(&document, 1, PointPx::new(220.0, 100.0), PointPx::new(330.0, 180.0));
    second.element_id = ElementId::from_uuid(Uuid::from_u128(2));
    let second_id = second.element_id;
    let second_anchor = match &second.payload {
      ElementPayload::Rectangle(payload) => payload.label_anchor,
      _ => unreachable!(),
    };
    document.elements.push(second);
    document.validate().unwrap();
    let interaction = PointerInteraction::Move {
      element_id: first_id,
      start: PointPx::ZERO,
      current: PointPx::new(55.0, 0.0),
    };
    let controller = EditorController::default();

    let preview = interaction_preview_set(&controller, &interaction, &document);

    let moved = preview.get(first_id).expect("moving rectangle should be previewed");
    let ElementPayload::Rectangle(moved_payload) = &moved.payload else {
      unreachable!();
    };
    assert_eq!(moved_payload.start_px, PointPx::new(115.0, 100.0));
    let neighbor = preview.get(second_id).expect("connected neighbor label should be previewed");
    let ElementPayload::Rectangle(neighbor_payload) = &neighbor.payload else {
      unreachable!();
    };
    assert_ne!(neighbor_payload.label_anchor, second_anchor);
  }

  #[test]
  fn new_rectangle_reflowed_label_keeps_its_actual_anchor_during_inline_editing() {
    let mut document = document_with_size(SizePx::new(420, 400));
    let mut blocker = rectangle(&document, 0, PointPx::new(24.0, 40.0), PointPx::new(300.0, 160.0));
    let ElementPayload::Rectangle(payload) = &mut blocker.payload else {
      unreachable!();
    };
    payload.label.text = None;
    blocker.refresh_bounds(document.canvas_size_px).unwrap();
    document.elements.push(blocker);
    document.validate().unwrap();

    let element_id = ElementId::from_uuid(Uuid::from_u128(3));
    let interaction = PointerInteraction::Draw {
      element_id,
      tool: EditorTool::Rectangle,
      start: PointPx::new(24.0, 170.0),
      current: PointPx::new(360.0, 300.0),
      stroke_points: Vec::new(),
    };
    let mut controller = EditorController {
      tool: EditorTool::Rectangle,
      interaction: Some(interaction),
      ..Default::default()
    };

    let drag_preview =
      interaction_preview_set(&controller, controller.interaction.as_ref().unwrap(), &document);
    let ElementPayload::Rectangle(drag_payload) =
      &drag_preview.get(element_id).expect("new rectangle should be previewed").payload
    else {
      unreachable!();
    };
    let drag_actual = drag_payload.label_anchor;
    assert_eq!(drag_actual.edge, RectangleLabelEdge::Top);
    assert_eq!(drag_actual.side, RectangleLabelSide::Outside);
    assert!(drag_actual.position > 0.0);

    let mut add_actions = Vec::new();
    controller.finish_pointer_interaction(&document, &mut add_actions);
    let [EditorAction::Command(add_batch)] = add_actions.as_slice() else {
      panic!("expected one rectangle add batch, got {add_actions:?}");
    };
    let released = controller
      .released_preview_elements
      .get(element_id)
      .expect("released rectangle should remain available for inline editing")
      .clone();
    let ElementPayload::Rectangle(released_payload) = &released.payload else {
      unreachable!();
    };
    assert_eq!(released_payload.label_anchor, drag_actual);
    assert_eq!(released_payload.label_anchor.edge, RectangleLabelEdge::Top);
    assert_eq!(released_payload.label_anchor.side, RectangleLabelSide::Outside);
    assert!(released_payload.label_anchor.position > 0.0);
    assert_eq!(released_payload.preferred_label_anchor.position, 0.0);

    let editing = controller.text_editing.as_ref().expect("new rectangle should edit its label");
    assert!(matches!(
      editing.target,
      TextTarget::RectangleLabel { element_id: editing_id } if editing_id == element_id
    ));
    assert!(!editing.auto_place_rectangle);
    let released_draft = rectangle_label_draft(released_payload, editing, &document, element_id);
    assert_eq!(released_draft.label_anchor, released_payload.label_anchor);
    let released_layout =
      rectangle_label_layout(&released_draft, document.canvas_size_px).unwrap().unwrap();
    let released_geometry =
      inline_text_geometry_with_preview(editing, &document, Some(&released)).unwrap();
    assert!(
      released_geometry.origin_px.distance_to(
        released_layout.bounds_px.min
          + PointPx::new(released_draft.label.padding_px, released_draft.label.padding_px)
      ) < 0.01
    );

    let before_add_revision = document.revision;
    let mut history = CommandHistory::new();
    history.execute_batch(&mut document, add_batch.clone()).unwrap();
    assert_eq!(document.revision, before_add_revision + 1);
    let ElementPayload::Rectangle(persisted_payload) =
      &document.element(element_id).unwrap().payload
    else {
      unreachable!();
    };
    assert_eq!(persisted_payload.label_anchor, released_payload.label_anchor);
    let editing = controller.text_editing.as_ref().unwrap();
    let persisted_draft = rectangle_label_draft(persisted_payload, editing, &document, element_id);
    assert_eq!(persisted_draft.label_anchor, persisted_payload.label_anchor);
    let persisted_geometry = inline_text_geometry(editing, &document).unwrap();
    assert!(persisted_geometry.origin_px.distance_to(released_geometry.origin_px) < 0.01);
    assert!((persisted_geometry.wrap_width_px - released_geometry.wrap_width_px).abs() < 0.01);

    let committed_text = "这是一个需要重新排布的长标题";
    controller.text_editing.as_mut().unwrap().buffer = committed_text.to_owned();
    let mut commit_actions = Vec::new();
    controller.commit_text(&document, &mut commit_actions);
    let [EditorAction::Command(commit_batch)] = commit_actions.as_slice() else {
      panic!("expected one label commit batch, got {commit_actions:?}");
    };
    assert!(matches!(
      commit_batch.commands().first(),
      Some(DocumentCommand::UpdateElementLabel {
        element_id: updated_id,
        text: Some(text),
      }) if *updated_id == element_id && text == committed_text
    ));
    let committed_actual = rectangle_label_placement_command(commit_batch.commands(), element_id)
      .map_or(persisted_payload.label_anchor, |(_, actual)| actual);

    history.execute_batch(&mut document, commit_batch.clone()).unwrap();
    let ElementPayload::Rectangle(committed_payload) =
      &document.element(element_id).unwrap().payload
    else {
      unreachable!();
    };
    assert_eq!(committed_payload.label.text.as_deref(), Some(committed_text));
    assert_eq!(committed_payload.label_anchor, committed_actual);
  }

  #[test]
  fn hidden_rectangle_label_reappears_with_an_atomic_collision_aware_anchor() {
    let mut document = document();
    let mut current =
      rectangle(&document, 0, PointPx::new(150.0, 90.0), PointPx::new(270.0, 170.0));
    let current_id = current.element_id;
    let ElementPayload::Rectangle(payload) = &mut current.payload else {
      unreachable!();
    };
    payload.label.text = None;
    payload.label_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Right, RectangleLabelSide::Inside, 0.8);
    current.refresh_bounds(document.canvas_size_px).unwrap();
    document.elements.push(current);
    let blocker = rectangle(&document, 1, PointPx::new(140.0, 20.0), PointPx::new(310.0, 78.0));
    let blocker_before = blocker.clone();
    document.elements.push(blocker);
    document.validate().unwrap();

    let mut controller = EditorController::new(EditorTool::Select);
    let mut actions = Vec::new();
    controller.start_editing_existing(&document, current_id, true, &mut actions);
    let editing = controller.text_editing.as_mut().unwrap();
    assert!(editing.auto_place_rectangle);
    editing.buffer = "重新展示".to_owned();
    controller.commit_text(&document, &mut actions);

    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected one atomic label batch, got {actions:?}");
    };
    let commands = batch.commands();
    assert!(matches!(
      commands.first(),
      Some(DocumentCommand::UpdateElementLabel {
        element_id: text_id,
        text: Some(text),
      }) if *text_id == current_id && text == "重新展示"
    ));
    assert!(
      commands[1..]
        .iter()
        .all(|command| matches!(command, DocumentCommand::SetRectangleLabelPlacement { .. })),
      "expected placement commands after text command, got {commands:?}"
    );
    let Some((current_preferred, current_actual)) =
      rectangle_label_placement_command(commands, current_id)
    else {
      panic!("expected current rectangle placement, got {commands:?}");
    };

    let before_revision = document.revision;
    let mut history = CommandHistory::new();
    history.execute_batch(&mut document, batch.clone()).unwrap();
    assert_eq!(document.revision, before_revision + 1);
    let ElementPayload::Rectangle(payload) = &document.element(current_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(payload.label.text.as_deref(), Some("重新展示"));
    assert_eq!(payload.preferred_label_anchor, current_preferred);
    assert_eq!(payload.label_anchor, current_actual);
    let ElementPayload::Rectangle(blocker) =
      &document.element(blocker_before.element_id).unwrap().payload
    else {
      unreachable!();
    };
    let ElementPayload::Rectangle(blocker_before_payload) = &blocker_before.payload else {
      unreachable!();
    };
    assert_eq!(blocker.start_px, blocker_before_payload.start_px);
    assert_eq!(blocker.end_px, blocker_before_payload.end_px);
    assert_eq!(blocker.label.text, blocker_before_payload.label.text);
    assert!(history.undo(&mut document).unwrap());
    assert_eq!(document.elements[1], blocker_before);
    let ElementPayload::Rectangle(payload) = &document.element(current_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(payload.label.text, None);
    assert_eq!(
      payload.label_anchor,
      RectangleLabelAnchor::new(RectangleLabelEdge::Right, RectangleLabelSide::Inside, 0.8,)
    );
  }

  #[test]
  fn rectangle_label_drag_back_to_blocked_outer_gap_is_ignored() {
    let mut document = document_with_size(SizePx::new(420, 360));
    let mut upper = rectangle(&document, 0, PointPx::new(80.0, 80.0), PointPx::new(280.0, 160.0));
    let ElementPayload::Rectangle(payload) = &mut upper.payload else {
      unreachable!();
    };
    payload.label.text = None;
    upper.refresh_bounds(document.canvas_size_px).unwrap();
    document.elements.push(upper);

    let mut lower = rectangle(&document, 1, PointPx::new(80.0, 200.0), PointPx::new(280.0, 300.0));
    let lower_id = lower.element_id;
    let ElementPayload::Rectangle(payload) = &mut lower.payload else {
      unreachable!();
    };
    payload.label.text = Some("line one\nline two\nline three".to_owned());
    payload.preferred_label_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Top, RectangleLabelSide::Outside, 0.0);
    payload.label_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Bottom, RectangleLabelSide::Outside, 0.0);
    lower.refresh_bounds(document.canvas_size_px).unwrap();
    document.elements.push(lower);
    document.validate().unwrap();

    let blocked_top_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Top, RectangleLabelSide::Outside, 0.0);
    let reflow_batch = rectangle_reflow_batch(
      &document,
      vec![DocumentCommand::SetRectangleLabelPlacement {
        element_id: lower_id,
        preferred_anchor: blocked_top_anchor,
        actual_anchor: blocked_top_anchor,
      }],
      lower_id,
      &[],
    )
    .unwrap();
    let mut history = CommandHistory::new();
    history.execute_batch(&mut document, reflow_batch).unwrap();

    let ElementPayload::Rectangle(payload) = &document.element(lower_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(payload.label_anchor.edge, RectangleLabelEdge::Top);
    assert_eq!(payload.label_anchor.side, RectangleLabelSide::Inside);
    let blocked_top_center = common::rectangle_label_layout_at_anchor(
      payload,
      payload.preferred_label_anchor,
      document.canvas_size_px,
    )
    .unwrap()
    .unwrap()
    .bounds_px
    .center();
    let interaction = PointerInteraction::DragRectangleLabel {
      element_id: lower_id,
      current: blocked_top_center,
      grab_offset_px: PointPx::ZERO,
    };

    let preview = interaction_preview_set(&EditorController::default(), &interaction, &document);
    assert!(preview.is_empty());

    let mut controller = EditorController { interaction: Some(interaction), ..Default::default() };
    let mut actions = Vec::new();
    controller.finish_pointer_interaction(&document, &mut actions);

    assert!(actions.is_empty());
  }

  #[test]
  fn rectangle_bodies_overlap_without_reflowing_any_existing_label() {
    let mut document = document();
    let first = rectangle(&document, 0, PointPx::new(30.0, 80.0), PointPx::new(130.0, 160.0));
    let first_id = first.element_id;
    let first_anchor = match &first.payload {
      ElementPayload::Rectangle(payload) => payload.label_anchor,
      _ => unreachable!(),
    };
    document.elements.push(first);
    let second = rectangle(&document, 1, PointPx::new(190.0, 80.0), PointPx::new(330.0, 165.0));
    let second_before = second.clone();
    document.elements.push(second);
    document.validate().unwrap();

    DocumentCommand::MoveElement { element_id: first_id, delta_px: PointPx::new(170.0, 0.0) }
      .apply(&mut document)
      .unwrap();
    let ElementPayload::Rectangle(first) = &document.element(first_id).unwrap().payload else {
      unreachable!();
    };
    let ElementPayload::Rectangle(second) = &second_before.payload else {
      unreachable!();
    };
    assert!(
      RectPx::from_points(first.start_px, first.end_px)
        .intersects(RectPx::from_points(second.start_px, second.end_px))
    );
    assert_eq!(first.label_anchor, first_anchor);
    assert_eq!(document.elements[1], second_before);

    DocumentCommand::ResizeRectangle {
      element_id: first_id,
      start_px: PointPx::new(175.0, 70.0),
      end_px: PointPx::new(350.0, 175.0),
    }
    .apply(&mut document)
    .unwrap();
    let ElementPayload::Rectangle(first) = &document.element(first_id).unwrap().payload else {
      unreachable!();
    };
    assert_eq!(first.label_anchor, first_anchor);
    assert_eq!(document.elements[1], second_before);
  }

  #[test]
  fn text_width_extends_from_anchor_to_canvas_edge() {
    let canvas = SizePx::new(400, 200);
    assert_eq!(text_width_to_canvas_edge(PointPx::new(120.0, 20.0), canvas), 280.0);
    assert_eq!(text_width_to_canvas_edge(PointPx::new(-30.0, 20.0), canvas), 400.0);
    assert_eq!(text_width_to_canvas_edge(PointPx::new(399.5, 20.0), canvas), 1.0);
  }

  #[test]
  fn tool_filtered_hit_testing_penetrates_other_types_and_keeps_matching_z_order() {
    let mut document = document();
    let position = PointPx::new(150.0, 100.0);
    let lower_rectangle =
      rectangle(&document, 0, PointPx::new(20.0, 30.0), PointPx::new(220.0, 180.0));
    let upper_rectangle =
      rectangle(&document, 1, PointPx::new(40.0, 40.0), PointPx::new(240.0, 160.0));
    let upper_rectangle_id = upper_rectangle.element_id;
    let arrow =
      arrow(&document, 2, PointPx::new(80.0, position.y_px), PointPx::new(220.0, position.y_px));
    let arrow_id = arrow.element_id;
    let text = Element::new(
      ElementId::new(),
      3,
      ElementPayload::Text(TextPayload {
        anchor_px: PointPx::new(140.0, 84.0),
        text: "X".to_owned(),
        box_width_px: 80.0,
        text_style: TextStyle::mvp(ColorRgba::WHITE, 24.0).unwrap(),
      }),
      document.canvas_size_px,
    )
    .unwrap();
    let text_id = text.element_id;
    let marker = sequence_marker(&document, 4);
    let marker_id = marker.element_id;
    let stroke_payload = StrokePayload::from_raw_points(
      &[PointPx::new(100.0, position.y_px), PointPx::new(200.0, position.y_px)],
      StrokeStyle::default(),
    )
    .unwrap();
    let stroke = Element::new(
      ElementId::new(),
      5,
      ElementPayload::Stroke(stroke_payload),
      document.canvas_size_px,
    )
    .unwrap();
    let stroke_id = stroke.element_id;
    document.elements = vec![lower_rectangle, upper_rectangle, arrow, text, marker, stroke];
    document.validate().unwrap();

    assert_eq!(
      hit_test_document_for_tool(&document, position, 2.0, EditorTool::Rectangle),
      Some(upper_rectangle_id)
    );
    assert_eq!(
      hit_test_document_for_tool(&document, position, 2.0, EditorTool::Arrow),
      Some(arrow_id)
    );
    assert_eq!(
      hit_test_document_for_tool(&document, position, 2.0, EditorTool::Text),
      Some(text_id)
    );
    assert_eq!(
      hit_test_document_for_tool(&document, position, 2.0, EditorTool::Sequence),
      Some(marker_id)
    );
    assert_eq!(
      hit_test_document_for_tool(&document, position, 2.0, EditorTool::Select),
      Some(stroke_id)
    );
    assert_eq!(hit_test_document_for_tool(&document, position, 2.0, EditorTool::Stroke), None);
  }

  #[test]
  fn matching_tools_reuse_move_label_and_shape_handle_interactions() {
    let mut document = document();
    let rectangle = rectangle(&document, 0, PointPx::new(80.0, 80.0), PointPx::new(180.0, 155.0));
    let rectangle_id = rectangle.element_id;
    document.elements.push(rectangle);
    let arrow = arrow(&document, 1, PointPx::new(220.0, 80.0), PointPx::new(360.0, 80.0));
    let arrow_id = arrow.element_id;
    document.elements.push(arrow);
    let text = text_element(&document, PointPx::new(220.0, 120.0), "move", 120.0);
    let text_id = text.element_id;
    let text_center = text.bounds_px.center();
    document.elements.push(text);
    let mut marker = sequence_marker(&document, 3);
    let marker_id = marker.element_id;
    let ElementPayload::SequenceMarker(payload) = &mut marker.payload else {
      unreachable!();
    };
    payload.center_px = PointPx::new(330.0, 150.0);
    let marker_center = payload.center_px;
    marker.refresh_bounds(document.canvas_size_px).unwrap();
    document.elements.push(marker);
    document.validate().unwrap();
    let transform = CanvasTransform::fit(
      document.canvas_size_px,
      Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 400.0)),
    )
    .unwrap();

    let mut rectangle_controller = EditorController::new(EditorTool::Rectangle);
    rectangle_controller.selected_element_id = Some(rectangle_id);
    assert!(rectangle_controller.begin_selection_drag(
      PointPx::new(180.0, 155.0),
      transform,
      &document
    ));
    assert!(matches!(
      rectangle_controller.interaction,
      Some(PointerInteraction::ResizeRectangle {
        element_id,
        handle: RectangleHandle::BottomRight,
        ..
      }) if element_id == rectangle_id
    ));
    rectangle_controller.interaction = None;
    assert!(rectangle_controller.begin_selection_drag(
      PointPx::new(130.0, 125.0),
      transform,
      &document
    ));
    assert!(matches!(
      rectangle_controller.interaction,
      Some(PointerInteraction::Move { element_id, .. }) if element_id == rectangle_id
    ));
    rectangle_controller.interaction = None;
    let rectangle_label_center = rectangle_label_layout(
      match &document.element(rectangle_id).unwrap().payload {
        ElementPayload::Rectangle(payload) => payload,
        _ => unreachable!(),
      },
      document.canvas_size_px,
    )
    .unwrap()
    .unwrap()
    .bounds_px
    .center();
    assert!(rectangle_controller.begin_selection_drag(
      rectangle_label_center,
      transform,
      &document
    ));
    assert!(matches!(
      rectangle_controller.interaction,
      Some(PointerInteraction::DragRectangleLabel { element_id, .. })
        if element_id == rectangle_id
    ));

    let mut arrow_controller = EditorController::new(EditorTool::Arrow);
    arrow_controller.selected_element_id = Some(arrow_id);
    assert!(arrow_controller.begin_selection_drag(PointPx::new(360.0, 80.0), transform, &document));
    assert!(matches!(
      arrow_controller.interaction,
      Some(PointerInteraction::UpdateArrowEndpoint {
        element_id,
        endpoint: ArrowEndpoint::End,
        ..
      }) if element_id == arrow_id
    ));
    arrow_controller.interaction = None;
    assert!(arrow_controller.begin_selection_drag(PointPx::new(285.0, 80.0), transform, &document));
    assert!(matches!(
      arrow_controller.interaction,
      Some(PointerInteraction::Move { element_id, .. }) if element_id == arrow_id
    ));

    for (tool, element_id, position) in
      [(EditorTool::Text, text_id, text_center), (EditorTool::Sequence, marker_id, marker_center)]
    {
      let mut controller = EditorController::new(tool);
      assert!(controller.begin_selection_drag(position, transform, &document));
      assert!(matches!(
        controller.interaction,
        Some(PointerInteraction::Move { element_id: moved_id, .. }) if moved_id == element_id
      ));
      assert_eq!(controller.selected_element_id, Some(element_id));
    }
  }

  #[test]
  fn switch_tool_is_idempotent_and_real_switch_commits_then_clears_state() {
    let mut document = document();
    let element = text_element(&document, PointPx::new(80.0, 60.0), "before", 120.0);
    let element_id = element.element_id;
    let ElementPayload::Text(payload) = &element.payload else {
      unreachable!();
    };
    let text_style = payload.text_style.clone();
    document.elements.push(element.clone());
    document.validate().unwrap();
    let stylus_id = StylusId { device_id: TouchDeviceId(7), touch_id: TouchId(11) };
    let mut controller = EditorController {
      tool: EditorTool::Text,
      selected_element_id: Some(element_id),
      interaction: Some(PointerInteraction::Move {
        element_id,
        start: PointPx::new(90.0, 70.0),
        current: PointPx::new(100.0, 80.0),
      }),
      released_preview_elements: ElementPreviewSet::single(element),
      text_editing: Some(TextEditing {
        target: TextTarget::ExistingText { element_id },
        buffer: "after".to_owned(),
        text_style,
        ime: InlineImeState::default(),
        request_focus: false,
        select_all: false,
        auto_place_rectangle: false,
      }),
      queued_stylus_events: vec![StylusEvent {
        id: stylus_id,
        phase: TouchPhase::Start,
        position: Pos2::new(100.0, 100.0),
        pressure: 0.5,
      }],
      active_stylus_id: Some(stylus_id),
      pending_stroke_points: vec![StrokePoint::new(PointPx::new(50.0, 50.0))],
      ..Default::default()
    };
    let mut actions = Vec::new();

    controller.switch_tool(EditorTool::Text, &document, &mut actions);

    assert!(actions.is_empty());
    assert_eq!(controller.selected_element_id, Some(element_id));
    assert!(controller.interaction.is_some());
    assert!(!controller.released_preview_elements.is_empty());
    assert_eq!(
      controller.text_editing.as_ref().map(|editing| editing.buffer.as_str()),
      Some("after")
    );
    assert_eq!(controller.active_stylus_id, Some(stylus_id));
    assert_eq!(controller.queued_stylus_events.len(), 1);
    assert_eq!(controller.pending_stroke_points.len(), 1);

    controller.switch_tool(EditorTool::Arrow, &document, &mut actions);

    let [EditorAction::Command(batch)] = actions.as_slice() else {
      panic!("expected the text commit before switching, got {actions:?}");
    };
    assert!(matches!(
      batch.commands(),
      [DocumentCommand::UpdateElement {
        element_id: updated_id,
        payload: ElementPayload::Text(TextPayload { text, .. }),
      }] if *updated_id == element_id && text == "after"
    ));
    assert_eq!(controller.tool, EditorTool::Arrow);
    assert!(controller.selected_element_id.is_none());
    assert!(controller.interaction.is_none());
    assert!(controller.released_preview_elements.is_empty());
    assert!(controller.text_editing.is_none());
    assert!(controller.active_stylus_id.is_none());
    assert!(controller.queued_stylus_events.is_empty());
    assert!(controller.pending_stroke_points.is_empty());
  }

  #[test]
  fn tab_and_shift_tab_cycle_through_all_tools() {
    let document = document();
    let history = CommandHistory::new();
    let context = egui::Context::default();

    let mut controller = EditorController::new(EditorTool::Select);
    controller.set_tab_order(EditorTool::ALL.to_vec());
    for expected in [EditorTool::Rectangle, EditorTool::Arrow, EditorTool::Text, EditorTool::Stroke]
    {
      run_tab_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![tab_event(Modifiers::NONE)],
      );
      assert_eq!(controller.active_tool(), expected);
    }
    run_tab_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![tab_event(Modifiers::NONE)],
    );
    assert_eq!(controller.active_tool(), EditorTool::Sequence);

    run_tab_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![tab_event(Modifiers::NONE)],
    );
    assert_eq!(controller.active_tool(), EditorTool::Select);

    run_tab_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![tab_event(Modifiers::SHIFT)],
    );
    assert_eq!(controller.active_tool(), EditorTool::Sequence);

    for expected in [EditorTool::Stroke, EditorTool::Text, EditorTool::Arrow, EditorTool::Rectangle]
    {
      run_tab_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![tab_event(Modifiers::SHIFT)],
      );
      assert_eq!(controller.active_tool(), expected);
    }
  }

  #[test]
  fn tab_capture_ignores_modifier_combinations_and_key_releases() {
    let document = document();
    let history = CommandHistory::new();
    let context = egui::Context::default();
    let mut controller = EditorController::new(EditorTool::Select);
    let events = vec![
      tab_event(Modifiers::COMMAND),
      tab_event(Modifiers::CTRL),
      Event::Key {
        key: Key::Tab,
        physical_key: Some(Key::Tab),
        pressed: false,
        repeat: false,
        modifiers: Modifiers::NONE,
      },
    ];
    run_tab_editor_frame(&context, &mut controller, &document, &history, events);
    assert_eq!(controller.active_tool(), EditorTool::Select);
    assert_eq!(controller.queued_tool_switch, None);
  }

  #[test]
  fn tab_switching_does_not_move_keyboard_focus() {
    let document = document();
    let history = CommandHistory::new();
    let context = egui::Context::default();
    let mut controller = EditorController::new(EditorTool::Select);
    run_tab_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![tab_event(Modifiers::NONE)],
    );
    assert_eq!(controller.active_tool(), EditorTool::Rectangle);
    assert!(context.memory(|memory| memory.focused()).is_none());
  }

  #[test]
  fn tab_cycles_follow_the_configured_tab_order() {
    let document = document();
    let history = CommandHistory::new();
    let context = egui::Context::default();
    let mut controller = EditorController::new(EditorTool::Select);
    controller.set_tab_order(vec![EditorTool::Text, EditorTool::Sequence, EditorTool::Arrow]);

    for expected in [EditorTool::Text, EditorTool::Sequence, EditorTool::Arrow, EditorTool::Text] {
      run_tab_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![tab_event(Modifiers::NONE)],
      );
      assert_eq!(controller.active_tool(), expected);
    }
    for expected in [EditorTool::Arrow, EditorTool::Sequence, EditorTool::Text] {
      run_tab_editor_frame(
        &context,
        &mut controller,
        &document,
        &history,
        vec![tab_event(Modifiers::SHIFT)],
      );
      assert_eq!(controller.active_tool(), expected);
    }
  }

  #[test]
  fn tab_does_nothing_with_an_empty_tab_order() {
    let document = document();
    let history = CommandHistory::new();
    let context = egui::Context::default();
    let mut controller = EditorController::new(EditorTool::Stroke);
    controller.set_tab_order(Vec::new());
    run_tab_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![tab_event(Modifiers::NONE)],
    );
    assert_eq!(controller.active_tool(), EditorTool::Stroke);
    run_tab_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![tab_event(Modifiers::SHIFT)],
    );
    assert_eq!(controller.active_tool(), EditorTool::Stroke);
  }

  #[test]
  fn tab_jumps_to_the_edge_when_the_current_tool_is_missing_from_the_order() {
    let document = document();
    let history = CommandHistory::new();
    let context = egui::Context::default();
    let mut controller = EditorController::new(EditorTool::Stroke);
    controller.set_tab_order(vec![EditorTool::Text, EditorTool::Rectangle]);
    run_tab_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![tab_event(Modifiers::NONE)],
    );
    assert_eq!(controller.active_tool(), EditorTool::Text);
    controller.set_active_tool(EditorTool::Stroke);
    run_tab_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![tab_event(Modifiers::SHIFT)],
    );
    assert_eq!(controller.active_tool(), EditorTool::Rectangle);
  }

  #[test]
  fn plain_enter_maps_to_deselect_and_clears_selection_without_a_command() {
    assert_eq!(map_shortcut(Key::Enter, Modifiers::NONE, false), Some(ShortcutAction::Deselect));
    assert_eq!(map_shortcut(Key::Enter, Modifiers::NONE, true), None);
    assert_eq!(map_shortcut(Key::Enter, Modifiers::SHIFT, false), None);

    let mut document = document();
    let element = rectangle(&document, 0, PointPx::new(80.0, 70.0), PointPx::new(180.0, 150.0));
    let element_id = element.element_id;
    document.elements.push(element);
    let revision = document.revision;
    let history = CommandHistory::new();
    let mut controller =
      EditorController { selected_element_id: Some(element_id), ..Default::default() };

    let actions = run_editor_frame(
      &egui::Context::default(),
      &mut controller,
      &document,
      &history,
      vec![enter_event(Modifiers::NONE)],
    );

    assert!(actions.is_empty());
    assert!(controller.selected_element_id.is_none());
    assert_eq!(document.revision, revision);
  }

  #[test]
  fn canvas_press_while_editing_commits_before_drag_creation_and_keeps_press_origin() {
    let mut document = document();
    let element = rectangle(&document, 0, PointPx::new(240.0, 80.0), PointPx::new(360.0, 160.0));
    let element_id = element.element_id;
    let ElementPayload::Rectangle(payload) = &element.payload else {
      unreachable!();
    };
    let text_style = payload.label.text_style.clone();
    document.elements.push(element);
    document.validate().unwrap();
    let history = CommandHistory::new();
    let context = egui::Context::default();
    let mut controller = EditorController {
      tool: EditorTool::Rectangle,
      selected_element_id: Some(element_id),
      text_editing: Some(TextEditing {
        target: TextTarget::RectangleLabel { element_id },
        buffer: "updated".to_owned(),
        text_style,
        ime: InlineImeState::default(),
        request_focus: true,
        select_all: false,
        auto_place_rectangle: false,
      }),
      ..Default::default()
    };
    assert!(
      run_editor_frame(&context, &mut controller, &document, &history, Vec::new()).is_empty()
    );

    let start = PointPx::new(30.0, 30.0);
    let middle = PointPx::new(70.0, 55.0);
    let end = PointPx::new(120.0, 95.0);
    let transform = CanvasTransform::fit(
      document.canvas_size_px,
      Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 400.0)),
    )
    .unwrap();

    let press_actions = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![
        Event::PointerMoved(transform.document_to_egui(start)),
        primary_button(transform.document_to_egui(start), true),
      ],
    );
    let [EditorAction::Command(commit_batch)] = press_actions.as_slice() else {
      panic!("expected the label commit on pointer press, got {press_actions:?}");
    };
    assert!(matches!(
      commit_batch.commands(),
      [DocumentCommand::UpdateElementLabel { element_id: updated_id, text: Some(text) }]
        if *updated_id == element_id && text == "updated"
    ));
    commit_batch.clone().apply(&mut document).unwrap();

    let drag_actions = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![Event::PointerMoved(transform.document_to_egui(middle))],
    );
    assert!(drag_actions.is_empty());
    assert!(matches!(
      controller.interaction,
      Some(PointerInteraction::Draw {
        element_id: _,
        tool: EditorTool::Rectangle,
        start: interaction_start,
        current,
        ..
      }) if interaction_start == start && current == middle
    ));

    let end_drag_actions = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![Event::PointerMoved(transform.document_to_egui(end))],
    );
    assert!(end_drag_actions.is_empty(), "unexpected drag actions: {end_drag_actions:?}");

    let release_actions = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![primary_button(transform.document_to_egui(end), false)],
    );
    let [EditorAction::Command(add_batch)] = release_actions.as_slice() else {
      panic!("expected the rectangle add on release, got {release_actions:?}");
    };
    assert!(matches!(
      add_batch.commands(),
      [DocumentCommand::AddElement {
        element: Element {
          payload: ElementPayload::Rectangle(RectanglePayload { start_px, end_px, .. }),
          ..
        },
      }] if *start_px == start && *end_px == end
    ));
  }

  #[test]
  fn clicking_the_active_toolbar_tool_preserves_text_editing() {
    let document = document();
    let history = CommandHistory::new();
    let context = egui::Context::default();
    let mut controller = EditorController {
      tool: EditorTool::Text,
      text_editing: Some(TextEditing {
        target: TextTarget::NewText { anchor_px: PointPx::new(380.0, 20.0) },
        buffer: "draft".to_owned(),
        text_style: TextStyle::mvp(ColorRgba::WHITE, 24.0).unwrap(),
        ime: InlineImeState::default(),
        request_focus: true,
        select_all: false,
        auto_place_rectangle: false,
      }),
      ..Default::default()
    };
    assert!(
      run_editor_frame(&context, &mut controller, &document, &history, Vec::new()).is_empty()
    );
    let button_center = controller.tool_button_rects[EditorTool::Text.index()].unwrap().center();

    let press_actions = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![Event::PointerMoved(button_center), primary_button(button_center, true)],
    );
    assert!(press_actions.is_empty());
    let release_actions = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![primary_button(button_center, false)],
    );
    assert!(release_actions.is_empty());

    assert_eq!(controller.tool, EditorTool::Text);
    assert!(matches!(
      controller.text_editing.as_ref(),
      Some(TextEditing { target: TextTarget::NewText { .. }, buffer, .. }) if buffer == "draft"
    ));
  }

  #[test]
  fn inline_editor_internal_press_does_not_commit_or_start_canvas_operation() {
    let document = document();
    let history = CommandHistory::new();
    let context = egui::Context::default();
    let mut controller = EditorController {
      tool: EditorTool::Rectangle,
      text_editing: Some(TextEditing {
        target: TextTarget::NewText { anchor_px: PointPx::new(20.0, 20.0) },
        buffer: "draft".to_owned(),
        text_style: TextStyle::mvp(ColorRgba::WHITE, 24.0).unwrap(),
        ime: InlineImeState::default(),
        request_focus: true,
        select_all: false,
        auto_place_rectangle: false,
      }),
      ..Default::default()
    };
    assert!(
      run_editor_frame(&context, &mut controller, &document, &history, Vec::new()).is_empty()
    );
    let transform = CanvasTransform::fit(
      document.canvas_size_px,
      Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 400.0)),
    )
    .unwrap();
    let press = transform.document_to_egui(PointPx::new(30.0, 30.0));

    let actions = run_editor_frame(
      &context,
      &mut controller,
      &document,
      &history,
      vec![Event::PointerMoved(press), primary_button(press, true)],
    );

    assert!(actions.is_empty());
    assert!(controller.interaction.is_none());
    assert!(matches!(
      controller.text_editing.as_ref(),
      Some(TextEditing { target: TextTarget::NewText { .. }, buffer, .. }) if buffer == "draft"
    ));
  }
}
