use common::{
  ArrowEndpoint, ArrowHead, ArrowPayload, BoardDocument, ColorRgba, CommandBatch, CommandHistory,
  DocumentCommand, Element, ElementId, ElementPayload, LabelPlacementPreference,
  PRESET_FONT_SIZES_PX, PRESET_STROKE_WIDTHS_PX, PointPx, RectPx, RectangleLabel, RectanglePayload,
  SequenceMarkerPayload, SizePx, StrokePayload, StrokeStyle, StyleChange, TextAlign, TextPayload,
  TextStyle, minimum_geometry_extent, rectangle_label_layout,
};
use eframe::egui::{
  self, Align2, Color32, CursorIcon, Event, FontId, Id, ImeEvent, Key, KeyboardShortcut, Modifiers,
  Pos2, Rect, Response, Sense, Stroke, StrokeKind, TextureHandle,
};

use crate::renderer::{
  layout_egui_text, paint_document, paint_element, paint_raw_polyline,
  paint_rectangle_without_label_text,
};

const DEFAULT_RECTANGLE_LABEL: &str = "标题";
const EMPTY_RECTANGLE_LABEL_DRAFT: &str = "\u{200b}";
const COLOR_SWATCH_FONT_SIZE_PT: f32 = 20.0;
const FLOATING_CONTROL_HEIGHT_PT: f32 = 32.0;
const FLOATING_MENU_MAX_HEIGHT_PT: f32 = f32::INFINITY;
const FLOATING_PANEL_ORDER: egui::Order = egui::Order::Middle;
const FLOATING_PANEL_MARGIN_PT: i8 = 4;
const TOOLBAR_DRAG_HANDLE_WIDTH_PT: f32 = 18.0;
const TOOLBAR_DRAG_HANDLE_DOT_RADIUS_PT: f32 = 1.5;
const HANDLE_VISUAL_RADIUS_PT: f32 = 4.5;
const HANDLE_HIT_RADIUS_PT: f32 = 11.0;
const HIT_TOLERANCE_PT: f32 = 7.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

  fn label(self) -> &'static str {
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
}

impl Default for ToolStyle {
  fn default() -> Self {
    Self { color_rgba: ColorRgba::RED, width_px: 8.0, font_size_px: 24.0 }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditorAction {
  Command(CommandBatch),
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
    Some(PointPx::new(
      ((position.x - self.canvas_rect.min.x) / self.scale)
        .clamp(0.0, self.document_size.width_px as f32),
      ((position.y - self.canvas_rect.min.y) / self.scale)
        .clamp(0.0, self.document_size.height_px as f32),
    ))
  }

  pub fn document_rect_to_egui(self, rect: RectPx) -> Rect {
    Rect::from_min_max(self.document_to_egui(rect.min), self.document_to_egui(rect.max))
  }
}

#[derive(Debug, Clone)]
enum PointerInteraction {
  Draw {
    tool: EditorTool,
    start: PointPx,
    current: PointPx,
    points: Vec<PointPx>,
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
}

#[derive(Debug, Clone, Default)]
struct InlineImeState {
  preedit: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct InlineTextGeometry {
  origin_px: PointPx,
  wrap_width_px: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShortcutAction {
  Tool(EditorTool),
  Undo,
  Redo,
  Save,
  CloseOrCancel,
  Delete,
  Copy,
  Paste,
}

#[derive(Debug, Clone)]
pub struct EditorController {
  tool: EditorTool,
  styles: [ToolStyle; 6],
  selected_element_id: Option<ElementId>,
  interaction: Option<PointerInteraction>,
  text_editing: Option<TextEditing>,
  last_pointer_document: Option<PointPx>,
  option_panel_anchor: Option<Pos2>,
  toolbar_screen_rect: Option<Rect>,
  toolbar_was_moved: bool,
}

impl Default for EditorController {
  fn default() -> Self {
    Self::new(None)
  }
}

impl EditorController {
  pub fn new(restored_tool: impl Into<Option<EditorTool>>) -> Self {
    Self {
      tool: restored_tool.into().unwrap_or(EditorTool::Rectangle),
      styles: [ToolStyle::default(); 6],
      selected_element_id: None,
      interaction: None,
      text_editing: None,
      last_pointer_document: None,
      option_panel_anchor: None,
      toolbar_screen_rect: None,
      toolbar_was_moved: false,
    }
  }

  pub fn active_tool(&self) -> EditorTool {
    self.tool
  }

  pub fn set_active_tool(&mut self, tool: EditorTool) {
    self.tool = tool;
    self.interaction = None;
  }

  pub fn selected_element_id(&self) -> Option<ElementId> {
    self.selected_element_id
  }

  pub fn set_selected_element_id(&mut self, element_id: Option<ElementId>) {
    self.selected_element_id = element_id;
  }

  pub fn tool_style(&self, tool: EditorTool) -> ToolStyle {
    self.styles[tool.index()]
  }

  pub fn show(
    &mut self,
    root_ui: &mut egui::Ui,
    document: &BoardDocument,
    history: &CommandHistory,
    background: Option<&TextureHandle>,
  ) -> Vec<EditorAction> {
    let ctx = root_ui.ctx().clone();
    if self.selected_element_id.is_some_and(|id| document.element(id).is_none()) {
      self.selected_element_id = None;
    }

    let mut actions = Vec::new();

    let mut transform = None;
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
        if self.text_editing.is_none() {
          self.handle_pointer(&response, canvas_transform, document, &mut actions);
        }

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
      let submit_text = self.show_text_editor(&ctx, transform, document);
      if let Some(painter) = canvas_painter {
        self.paint_document_for_editing(&painter, transform, document);
        self.paint_interaction(&painter, transform, document);
        if self.text_editing.is_none() {
          self.paint_selection(&painter, transform, document);
        }
      }
      if submit_text {
        self.commit_text(document, &mut actions);
      }
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

  fn handle_keyboard(
    &mut self,
    ctx: &egui::Context,
    document: &BoardDocument,
    actions: &mut Vec<EditorAction>,
  ) {
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
        if text_editing {
          self.commit_text(document, actions);
        }
        self.tool = tool;
        self.interaction = None;
      }
      ShortcutAction::Undo => {
        self.interaction = None;
        self.selected_element_id = None;
        actions.push(EditorAction::Undo);
      }
      ShortcutAction::Redo => {
        self.interaction = None;
        self.selected_element_id = None;
        actions.push(EditorAction::Redo);
      }
      ShortcutAction::Save => {
        if text_editing {
          self.commit_text(document, actions);
        }
        actions.push(EditorAction::Save);
      }
      ShortcutAction::CloseOrCancel => {
        if self.text_editing.take().is_some() || self.interaction.take().is_some() {
          return;
        }
        actions.push(EditorAction::Close);
      }
      ShortcutAction::Delete => {
        if let Some(element_id) = self.selected_element_id.take() {
          actions.push(command_action(DocumentCommand::DeleteElement { element_id }));
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

  fn handle_pointer(
    &mut self,
    response: &Response,
    transform: CanvasTransform,
    document: &BoardDocument,
    actions: &mut Vec<EditorAction>,
  ) {
    let pointer_position =
      response.interact_pointer_pos().and_then(|p| transform.egui_to_document(p));

    if response.double_clicked()
      && self.tool == EditorTool::Select
      && let Some(position) = pointer_position
      && let Some(element_id) =
        hit_test_document(document, position, HIT_TOLERANCE_PT / transform.scale())
    {
      self.selected_element_id = Some(element_id);
      self.start_editing_existing(document, element_id);
      return;
    }

    if response.clicked()
      && let Some(position) = pointer_position
    {
      match self.tool {
        EditorTool::Select => {
          self.selected_element_id =
            hit_test_document(document, position, HIT_TOLERANCE_PT / transform.scale());
        }
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
          });
        }
        EditorTool::Sequence => self.insert_sequence(document, position, actions),
        EditorTool::Rectangle | EditorTool::Arrow | EditorTool::Stroke => {}
      }
    }

    if response.drag_started() {
      let start = response
        .ctx
        .input(|input| input.pointer.press_origin())
        .and_then(|position| transform.egui_to_document(position))
        .or(pointer_position);
      if let Some(start) = start {
        if self.tool == EditorTool::Select {
          self.begin_selection_drag(start, transform, document);
        } else if matches!(
          self.tool,
          EditorTool::Rectangle | EditorTool::Arrow | EditorTool::Stroke
        ) {
          self.interaction = Some(PointerInteraction::Draw {
            tool: self.tool,
            start,
            current: pointer_position.unwrap_or(start),
            points: vec![start],
          });
        }
      }
    }

    if response.dragged()
      && let Some(position) = pointer_position
    {
      match &mut self.interaction {
        Some(PointerInteraction::Draw { current, points, tool, .. }) => {
          *current = position;
          if *tool == EditorTool::Stroke
            && points.last().is_none_or(|last| last.distance_to(position) >= 0.75)
          {
            points.push(position);
          }
        }
        Some(PointerInteraction::Move { current, .. })
        | Some(PointerInteraction::ResizeRectangle { current, .. })
        | Some(PointerInteraction::UpdateArrowEndpoint { current, .. }) => *current = position,
        None => {}
      }
    }

    if response.drag_stopped() {
      self.finish_pointer_interaction(document, actions);
    }
  }

  fn begin_selection_drag(
    &mut self,
    position: PointPx,
    transform: CanvasTransform,
    document: &BoardDocument,
  ) {
    if let Some(element_id) = self.selected_element_id
      && let Some(element) = document.element(element_id)
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
        return;
      }
      if let Some(endpoint) = hit_arrow_handle(element, position, transform) {
        self.interaction =
          Some(PointerInteraction::UpdateArrowEndpoint { element_id, endpoint, current: position });
        return;
      }
    }

    self.selected_element_id =
      hit_test_document(document, position, HIT_TOLERANCE_PT / transform.scale());
    if let Some(element_id) = self.selected_element_id {
      self.interaction =
        Some(PointerInteraction::Move { element_id, start: position, current: position });
    }
  }

  fn finish_pointer_interaction(
    &mut self,
    document: &BoardDocument,
    actions: &mut Vec<EditorAction>,
  ) {
    let Some(interaction) = self.interaction.take() else {
      return;
    };
    match interaction {
      PointerInteraction::Draw { tool, start, current, mut points } => match tool {
        EditorTool::Rectangle => {
          if let Some(element) = self.make_rectangle(document, start, current) {
            let element_id = element.element_id;
            let ElementPayload::Rectangle(payload) = &element.payload else {
              unreachable!("rectangle tool created a non-rectangle element");
            };
            let text_style = payload.label.text_style.clone();
            actions.push(command_action(DocumentCommand::AddElement { element }));
            self.selected_element_id = Some(element_id);
            self.text_editing = Some(TextEditing {
              target: TextTarget::RectangleLabel { element_id },
              buffer: DEFAULT_RECTANGLE_LABEL.to_owned(),
              text_style,
              ime: InlineImeState::default(),
              request_focus: true,
              select_all: true,
            });
          }
        }
        EditorTool::Arrow => {
          if let Some(element) = self.make_arrow(document, start, current) {
            actions.push(command_action(DocumentCommand::AddElement { element }));
          }
        }
        EditorTool::Stroke => {
          if points.last().copied() != Some(current) {
            points.push(current);
          }
          if let Some(element) = self.make_stroke(document, &points) {
            actions.push(command_action(DocumentCommand::AddElement { element }));
          }
        }
        EditorTool::Select | EditorTool::Text | EditorTool::Sequence => {}
      },
      PointerInteraction::Move { element_id, start, current } => {
        let delta_px = current - start;
        if delta_px.distance_to(PointPx::ZERO) > 0.01 {
          actions.push(command_action(DocumentCommand::MoveElement { element_id, delta_px }));
        }
      }
      PointerInteraction::ResizeRectangle { element_id, handle, original, current } => {
        let (start_px, end_px) = resized_rectangle(original, handle, current);
        if let Some(element) = document.element(element_id)
          && let ElementPayload::Rectangle(payload) = &element.payload
          && minimum_geometry_extent(payload.stroke_style.width_px).is_ok_and(|minimum| {
            let rect = RectPx::from_points(start_px, end_px);
            rect.width() >= minimum && rect.height() >= minimum
          })
        {
          actions.push(command_action(DocumentCommand::ResizeRectangle {
            element_id,
            start_px,
            end_px,
          }));
        }
      }
      PointerInteraction::UpdateArrowEndpoint { element_id, endpoint, current } => {
        if let Some(element) = document.element(element_id)
          && let ElementPayload::Arrow(payload) = &element.payload
        {
          let other = match endpoint {
            ArrowEndpoint::Start => payload.end_px,
            ArrowEndpoint::End => payload.start_px,
          };
          if current.distance_to(other) >= payload.head.min_body_length_px {
            actions.push(command_action(DocumentCommand::UpdateArrowEndpoint {
              element_id,
              endpoint,
              position_px: current,
            }));
          }
        }
      }
    }
  }

  fn make_rectangle(
    &self,
    document: &BoardDocument,
    start_px: PointPx,
    end_px: PointPx,
  ) -> Option<Element> {
    let style = self.tool_style(EditorTool::Rectangle);
    let stroke_style = StrokeStyle::mvp(style.color_rgba, style.width_px).ok()?;
    let rect = RectPx::from_points(start_px, end_px);
    let minimum = minimum_geometry_extent(style.width_px).ok()?;
    if rect.width() < minimum || rect.height() < minimum {
      return None;
    }
    let text_style =
      TextStyle::mvp(style.color_rgba.contrasting_text(), style.font_size_px).ok()?;
    Element::new(
      ElementId::new(),
      document.elements.len() as i64,
      ElementPayload::Rectangle(RectanglePayload {
        start_px,
        end_px,
        stroke_style,
        fill_rgba: None,
        label: RectangleLabel {
          text: DEFAULT_RECTANGLE_LABEL.to_owned(),
          placement_preference: LabelPlacementPreference::Above,
          max_width_px: 420.0,
          padding_px: 8.0,
          anchor_offset_px: 8.0,
          text_style,
        },
      }),
      document.canvas_size_px,
    )
    .ok()
  }

  fn make_arrow(
    &self,
    document: &BoardDocument,
    start_px: PointPx,
    end_px: PointPx,
  ) -> Option<Element> {
    let style = self.tool_style(EditorTool::Arrow);
    let stroke_style = StrokeStyle::mvp(style.color_rgba, style.width_px).ok()?;
    let head = ArrowHead::for_stroke_width(style.width_px).ok()?;
    if start_px.distance_to(end_px) < head.min_body_length_px {
      return None;
    }
    Element::new(
      ElementId::new(),
      document.elements.len() as i64,
      ElementPayload::Arrow(ArrowPayload { start_px, end_px, stroke_style, head }),
      document.canvas_size_px,
    )
    .ok()
  }

  fn make_stroke(&self, document: &BoardDocument, points: &[PointPx]) -> Option<Element> {
    let style = self.tool_style(EditorTool::Stroke);
    let stroke_style = StrokeStyle::mvp(style.color_rgba, style.width_px).ok()?;
    let payload = StrokePayload::from_raw_points(points, stroke_style).ok()?;
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
    let radius_px = (style.font_size_px * 0.72).max(14.0);
    let digit_count = document.next_sequence_number.to_string().len() as f32;
    let pill_width_px = (digit_count * style.font_size_px * 0.68 + 16.0).max(radius_px * 2.0);
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

  fn start_editing_existing(&mut self, document: &BoardDocument, element_id: ElementId) {
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
        });
      }
      ElementPayload::Rectangle(payload) => {
        self.text_editing = Some(TextEditing {
          target: TextTarget::RectangleLabel { element_id },
          buffer: payload.label.text.clone(),
          text_style: payload.label.text_style.clone(),
          ime: InlineImeState::default(),
          request_focus: true,
          select_all: false,
        });
      }
      _ => {}
    }
  }

  fn commit_text(&mut self, document: &BoardDocument, actions: &mut Vec<EditorAction>) {
    let Some(editing) = self.text_editing.take() else {
      return;
    };
    if editing.buffer.trim().is_empty() {
      return;
    }
    match editing.target {
      TextTarget::NewText { anchor_px } => {
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
        actions.push(command_action(DocumentCommand::UpdateElement {
          element_id,
          payload: ElementPayload::Text(payload),
        }));
      }
      TextTarget::RectangleLabel { element_id } => {
        let Some(element) = document.element(element_id) else {
          return;
        };
        let ElementPayload::Rectangle(payload) = &element.payload else {
          return;
        };
        if payload.label.text != editing.buffer {
          actions.push(command_action(DocumentCommand::UpdateRectangleLabel {
            element_id,
            text: editing.buffer,
          }));
        }
      }
    }
  }

  fn show_text_editor(
    &mut self,
    ctx: &egui::Context,
    transform: CanvasTransform,
    document: &BoardDocument,
  ) -> bool {
    let Some(editing) = self.text_editing.as_ref() else {
      return false;
    };
    let Some(geometry) = inline_text_geometry(editing, document) else {
      return false;
    };
    let screen_position = transform.document_to_egui(geometry.origin_px);
    let editor_id = Id::new("rs-board-inline-text-editor");
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
    let desired_width = (geometry.wrap_width_px * transform.scale()).max(1.0);
    let editor_rect = Rect::from_min_size(
      screen_position,
      egui::vec2(desired_width, transform.canvas_rect().height().max(1.0)),
    );
    let layer_id = egui::LayerId::new(egui::Order::Foreground, editor_id.with("layer"));
    ctx.set_transform_layer(layer_id, egui::emath::TSTransform::IDENTITY);
    ctx.move_to_top(layer_id);
    let first_shape = ctx.graphics_mut(|graphics| graphics.entry(layer_id).next_idx());
    let mut ui = egui::Ui::new(
      ctx.clone(),
      editor_id.with("ui"),
      egui::UiBuilder::new().layer_id(layer_id).max_rect(editor_rect),
    );
    ui.set_clip_rect(transform.canvas_rect());
    ui.set_width(desired_width);
    let layout_text_style = editing.text_style.clone();
    let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
      layout_egui_text(
        ui.painter(),
        text.as_str(),
        &layout_text_style,
        wrap_width,
        transform.scale(),
        1.0,
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
    if submit_after_widget {
      output.response.surrender_focus();
    }
    if let Some(updated_geometry) = inline_text_geometry(editing, document) {
      let updated_position = transform.document_to_egui(updated_geometry.origin_px);
      translate_inline_text_layer(
        ctx,
        layer_id,
        first_shape,
        updated_position - screen_position,
        transform.canvas_rect(),
      );
    }
    lost_focus || submit_after_widget
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
            if ui
              .selectable_label(self.tool == tool, tool.label())
              .on_hover_text(tool.tooltip())
              .clicked()
            {
              if self.text_editing.is_some() {
                self.commit_text(document, actions);
              }
              self.tool = tool;
              self.interaction = None;
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
    self.toolbar_was_moved |= area.response.dragged();
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
                actions.push(command_action(DocumentCommand::BringForward { element_id }));
              }
              if ui.button("↓").on_hover_text("下移一层").clicked() {
                actions.push(command_action(DocumentCommand::SendBackward { element_id }));
              }
              if ui.button("⇈").on_hover_text("置于顶层").clicked() {
                actions.push(command_action(DocumentCommand::BringToFront { element_id }));
              }
              if ui.button("⇊").on_hover_text("置于底层").clicked() {
                actions.push(command_action(DocumentCommand::SendToBack { element_id }));
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

  fn apply_style_change(
    &mut self,
    document: &BoardDocument,
    mut change: StyleChange,
    actions: &mut Vec<EditorAction>,
  ) {
    if let Some(element_id) = self.selected_element_id
      && let Some(element) = document.element(element_id)
    {
      match element.payload {
        ElementPayload::Stroke(_) | ElementPayload::Arrow(_) => change.font_size_px = None,
        ElementPayload::Text(_) => change.width_px = None,
        ElementPayload::Rectangle(_) | ElementPayload::SequenceMarker(_) => {}
      }
      if change != StyleChange::default() {
        actions.push(command_action(DocumentCommand::ChangeElementStyle { element_id, change }));
      }
      return;
    }

    let style = &mut self.styles[self.tool.index()];
    if let Some(color) = change.color_rgba {
      style.color_rgba = color;
    }
    if let Some(width) = change.width_px {
      style.width_px = width;
    }
    if let Some(font_size) = change.font_size_px {
      style.font_size_px = font_size;
    }
  }

  fn paint_document_for_editing(
    &self,
    painter: &egui::Painter,
    transform: CanvasTransform,
    document: &BoardDocument,
  ) {
    let painter = painter.with_clip_rect(transform.canvas_rect());
    for element in &document.elements {
      match self.text_editing.as_ref().map(|editing| &editing.target) {
        Some(TextTarget::ExistingText { element_id }) if *element_id == element.element_id => {}
        Some(TextTarget::RectangleLabel { element_id }) if *element_id == element.element_id => {
          let ElementPayload::Rectangle(payload) = &element.payload else {
            continue;
          };
          let editing = self.text_editing.as_ref().expect("text editing target checked above");
          let draft = rectangle_label_draft(payload, editing);
          paint_rectangle_without_label_text(&painter, &transform, &draft, 1.0);
        }
        _ => paint_element(&painter, &transform, element, 1.0),
      }
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
      PointerInteraction::Draw { tool, start, current, points } => match tool {
        EditorTool::Rectangle => {
          if let Some(element) = self.make_rectangle(document, *start, *current) {
            paint_element(painter, &transform, &element, 0.72);
          }
        }
        EditorTool::Arrow => {
          if let Some(element) = self.make_arrow(document, *start, *current) {
            paint_element(painter, &transform, &element, 0.72);
          } else {
            paint_raw_polyline(
              painter,
              &transform,
              &[*start, *current],
              self.tool_style(EditorTool::Arrow).color_rgba,
              self.tool_style(EditorTool::Arrow).width_px,
            );
          }
        }
        EditorTool::Stroke => paint_raw_polyline(
          painter,
          &transform,
          points,
          self.tool_style(EditorTool::Stroke).color_rgba,
          self.tool_style(EditorTool::Stroke).width_px,
        ),
        EditorTool::Select | EditorTool::Text | EditorTool::Sequence => {}
      },
      PointerInteraction::Move { element_id, start, current } => {
        if let Some(element) = document.element(*element_id) {
          let mut preview = element.clone();
          if preview.move_by(*current - *start, document.canvas_size_px).is_ok() {
            paint_element(painter, &transform, &preview, 0.64);
          }
        }
      }
      PointerInteraction::ResizeRectangle { element_id, handle, original, current } => {
        if let Some(element) = document.element(*element_id) {
          let mut preview = element.clone();
          if let ElementPayload::Rectangle(payload) = &mut preview.payload {
            (payload.start_px, payload.end_px) = resized_rectangle(*original, *handle, *current);
            if preview.refresh_bounds(document.canvas_size_px).is_ok() {
              paint_element(painter, &transform, &preview, 0.7);
            }
          }
        }
      }
      PointerInteraction::UpdateArrowEndpoint { element_id, endpoint, current } => {
        if let Some(element) = document.element(*element_id) {
          let mut preview = element.clone();
          if let ElementPayload::Arrow(payload) = &mut preview.payload {
            match endpoint {
              ArrowEndpoint::Start => payload.start_px = *current,
              ArrowEndpoint::End => payload.end_px = *current,
            }
            if preview.refresh_bounds(document.canvas_size_px).is_ok() {
              paint_element(painter, &transform, &preview, 0.7);
            }
          }
        }
      }
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
    let Some(element) = document.element(element_id) else {
      return;
    };
    let mut bounds = element.bounds_px;
    if let Some(PointerInteraction::Move { element_id: moving, start, current }) = &self.interaction
      && *moving == element_id
    {
      bounds = bounds.translated(*current - *start);
    }
    let rect = transform.document_rect_to_egui(bounds).expand(3.0);
    painter.rect_stroke(
      rect,
      egui::CornerRadius::ZERO,
      Stroke::new(1.0, Color32::WHITE),
      StrokeKind::Outside,
    );
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
      _ => {}
    }
  }
}

pub fn hit_test_document(
  document: &BoardDocument,
  position_px: PointPx,
  tolerance_px: f32,
) -> Option<ElementId> {
  document
    .elements
    .iter()
    .filter(|element| hit_test_element(element, position_px, tolerance_px, document.canvas_size_px))
    .max_by_key(|element| element.z_index)
    .map(|element| element.element_id)
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
    ElementPayload::Stroke(payload) => payload.points.windows(2).any(|points| {
      distance_to_segment(point, points[0].point(), points[1].point())
        <= payload.stroke_style.width_px / 2.0 + tolerance
    }),
    ElementPayload::Arrow(payload) => {
      distance_to_segment(point, payload.start_px, payload.end_px)
        <= payload.stroke_style.width_px / 2.0 + tolerance
        || contains(element.bounds_px.expanded(tolerance), point)
    }
    ElementPayload::Rectangle(payload) => {
      contains(RectPx::from_points(payload.start_px, payload.end_px).expanded(tolerance), point)
        || rectangle_label_layout(payload, canvas_size)
          .is_ok_and(|layout| contains(layout.bounds_px.expanded(tolerance), point))
    }
    ElementPayload::Text(_) | ElementPayload::SequenceMarker(_) => {
      contains(element.bounds_px.expanded(tolerance), point)
    }
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

fn inline_text_geometry(
  editing: &TextEditing,
  document: &BoardDocument,
) -> Option<InlineTextGeometry> {
  match editing.target {
    TextTarget::NewText { anchor_px } => Some(InlineTextGeometry {
      origin_px: anchor_px,
      wrap_width_px: text_width_to_canvas_edge(anchor_px, document.canvas_size_px),
    }),
    TextTarget::ExistingText { element_id } => {
      let ElementPayload::Text(payload) = &document.element(element_id)?.payload else {
        return None;
      };
      Some(InlineTextGeometry {
        origin_px: payload.anchor_px,
        wrap_width_px: text_width_to_canvas_edge(payload.anchor_px, document.canvas_size_px),
      })
    }
    TextTarget::RectangleLabel { element_id } => {
      // A newly-created rectangle is added after this frame. Waiting for it to
      // enter the document avoids briefly placing its editor at the origin.
      let ElementPayload::Rectangle(payload) = &document.element(element_id)?.payload else {
        return None;
      };
      let draft = rectangle_label_draft(payload, editing);
      let layout = rectangle_label_layout(&draft, document.canvas_size_px).ok()?;
      let alignment_width_px = (layout.bounds_px.width() - draft.label.padding_px * 2.0).max(1.0);
      let alignment_offset_px = match editing.text_style.align {
        TextAlign::Left => 0.0,
        TextAlign::Center => (alignment_width_px - layout.text_wrap_width_px) / 2.0,
        TextAlign::Right => alignment_width_px - layout.text_wrap_width_px,
      };
      Some(InlineTextGeometry {
        origin_px: layout.bounds_px.min
          + PointPx::new(draft.label.padding_px + alignment_offset_px, draft.label.padding_px),
        wrap_width_px: layout.text_wrap_width_px,
      })
    }
  }
}

fn rectangle_label_draft(payload: &RectanglePayload, editing: &TextEditing) -> RectanglePayload {
  let mut draft = payload.clone();
  draft.label.text = if editing.buffer.trim().is_empty() {
    EMPTY_RECTANGLE_LABEL_DRAFT.to_owned()
  } else {
    editing.buffer.clone()
  };
  draft.label.text_style = editing.text_style.clone();
  draft
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

fn normalize_inline_text_events(events: &mut Vec<Event>, ime: &mut InlineImeState) -> bool {
  let frame_preedit = events.iter().rev().find_map(|event| match event {
    Event::Ime(ImeEvent::Preedit { text, .. }) if !text.is_empty() => Some(text.clone()),
    _ => None,
  });
  let ime_involved_this_frame = ime.preedit.as_ref().is_some_and(|text| !text.is_empty())
    || frame_preedit.is_some()
    || events.iter().any(|event| match event {
      Event::Ime(ImeEvent::Commit(text)) => !text.trim_end_matches(['\r', '\n']).is_empty(),
      _ => false,
    });

  let previous_preedit = ime.preedit.clone();
  let mut submit_after_widget = false;
  events.retain_mut(|event| match event {
    Event::Ime(ImeEvent::Preedit { text, .. }) => {
      ime.preedit = (!text.is_empty()).then(|| text.clone());
      true
    }
    Event::Ime(ImeEvent::Commit(text)) => {
      let trimmed_len = text.trim_end_matches(['\r', '\n']).len();
      let had_trailing_line_ending = trimmed_len != text.len();
      text.truncate(trimmed_len);
      if text.is_empty()
        && had_trailing_line_ending
        && let Some(preedit) = ime.preedit.clone().or_else(|| previous_preedit.clone())
      {
        *text = preedit;
      }
      let keep_event = if text.is_empty() && had_trailing_line_ending && !ime_involved_this_frame {
        submit_after_widget = true;
        false
      } else {
        true
      };
      ime.preedit = None;
      keep_event
    }
    Event::Key { key: Key::Enter, pressed: true, modifiers, .. } => {
      if ime_involved_this_frame {
        false
      } else if modifiers.shift || modifiers.mac_cmd || modifiers.command {
        *modifiers = Modifiers::SHIFT;
        true
      } else {
        submit_after_widget = true;
        false
      }
    }
    _ => true,
  });
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
    },
    ElementPayload::Arrow(payload) => ToolStyle {
      color_rgba: payload.stroke_style.color_rgba,
      width_px: payload.stroke_style.width_px,
      font_size_px: 24.0,
    },
    ElementPayload::Rectangle(payload) => ToolStyle {
      color_rgba: payload.stroke_style.color_rgba,
      width_px: payload.stroke_style.width_px,
      font_size_px: payload.label.text_style.font_size_px,
    },
    ElementPayload::Text(payload) => ToolStyle {
      color_rgba: payload.text_style.color_rgba,
      width_px: 8.0,
      font_size_px: payload.text_style.font_size_px,
    },
    ElementPayload::SequenceMarker(payload) => ToolStyle {
      color_rgba: payload.fill_rgba,
      width_px: payload.stroke_style.width_px,
      font_size_px: payload.text_style.font_size_px,
    },
  }
}

fn command_action(command: DocumentCommand) -> EditorAction {
  EditorAction::Command(CommandBatch::single(command))
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
  painter.circle_filled(position, HANDLE_VISUAL_RADIUS_PT + 1.5, Color32::BLACK);
  painter.circle(
    position,
    HANDLE_VISUAL_RADIUS_PT,
    Color32::WHITE,
    Stroke::new(1.0, Color32::BLACK),
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
        label: RectangleLabel {
          text: DEFAULT_RECTANGLE_LABEL.to_owned(),
          placement_preference: LabelPlacementPreference::Above,
          max_width_px: 200.0,
          padding_px: 8.0,
          anchor_offset_px: 8.0,
          text_style: TextStyle::mvp(style.color_rgba.contrasting_text(), 24.0).unwrap(),
        },
      }),
      document.canvas_size_px,
    )
    .unwrap()
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
      let mut ime = InlineImeState { preedit: Some("o".to_owned()) };
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
  fn ime_commit_preserves_internal_newlines() {
    let mut ime = InlineImeState::default();
    let mut events = vec![Event::Ime(ImeEvent::Commit("A\nB\n".to_owned()))];
    assert!(!normalize_inline_text_events(&mut events, &mut ime));
    assert_eq!(events, vec![Event::Ime(ImeEvent::Commit("A\nB".to_owned()))]);
  }

  #[test]
  fn ime_events_are_order_independent_for_candidate_confirmation() {
    let preedit =
      Event::Ime(ImeEvent::Preedit { text: "O".to_owned(), active_range_chars: Some(0..1) });
    let commit = Event::Ime(ImeEvent::Commit("OK\n".to_owned()));
    let enter = enter_event(Modifiers::NONE);
    for mut events in [
      vec![preedit.clone(), commit.clone(), enter.clone()],
      vec![preedit.clone(), enter.clone(), commit.clone()],
      vec![commit.clone(), preedit.clone(), enter.clone()],
      vec![commit.clone(), enter.clone(), preedit.clone()],
      vec![enter.clone(), preedit.clone(), commit.clone()],
      vec![enter.clone(), commit.clone(), preedit.clone()],
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
  fn ime_newline_commit_uses_only_an_already_active_preedit() {
    let mut ime = InlineImeState::default();
    let mut events = vec![
      Event::Ime(ImeEvent::Commit("\n".to_owned())),
      Event::Ime(ImeEvent::Preedit { text: "future".to_owned(), active_range_chars: Some(0..6) }),
    ];

    assert!(!normalize_inline_text_events(&mut events, &mut ime));
    assert_eq!(events[0], Event::Ime(ImeEvent::Commit(String::new())));
    assert_eq!(ime.preedit.as_deref(), Some("future"));
  }

  #[test]
  fn idle_ime_newline_commit_routes_to_submit() {
    for line_ending in ["\n", "\r", "\r\n"] {
      let mut ime = InlineImeState::default();
      let mut events = vec![Event::Ime(ImeEvent::Commit(line_ending.to_owned()))];

      assert!(normalize_inline_text_events(&mut events, &mut ime));
      assert!(events.is_empty());
    }
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
      [DocumentCommand::UpdateRectangleLabel { element_id: updated, text }]
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
    };

    let geometry = inline_text_geometry(&editing, &document).unwrap();
    let ElementPayload::Rectangle(payload) = &document.element(element_id).unwrap().payload else {
      unreachable!();
    };
    let draft = rectangle_label_draft(payload, &editing);
    let layout = rectangle_label_layout(&draft, document.canvas_size_px).unwrap();
    assert_eq!(geometry.wrap_width_px, layout.text_wrap_width_px);
    assert_eq!(
      geometry.origin_px,
      layout.bounds_px.min + PointPx::new(draft.label.padding_px, draft.label.padding_px)
    );
    assert_eq!(layout.text_layout.line_count, 1);
    assert!(layout.text_wrap_width_px > layout.bounds_px.width() - draft.label.padding_px * 2.0);

    editing.buffer.clear();
    let empty_draft = rectangle_label_draft(payload, &editing);
    let empty_layout = rectangle_label_layout(&empty_draft, document.canvas_size_px).unwrap();
    let empty_geometry = inline_text_geometry(&editing, &document).unwrap();
    assert_eq!(empty_draft.label.text, EMPTY_RECTANGLE_LABEL_DRAFT);
    assert_eq!(empty_layout.text_layout.width_px, 1.0);
    assert_eq!(empty_geometry.wrap_width_px, empty_layout.text_wrap_width_px);
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
    controller.start_editing_existing(&document, element_id);
    let transform = CanvasTransform::fit(
      document.canvas_size_px,
      Rect::from_min_size(Pos2::ZERO, egui::vec2(1000.0, 200.0)),
    )
    .unwrap();
    let context = egui::Context::default();
    let output = context.run_ui(raw_input(Vec::new(), egui::vec2(1000.0, 200.0)), |ui| {
      assert!(!controller.show_text_editor(&context, transform, &document));
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
        assert!(!controller.show_text_editor(context, transform, document));
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
    let draft = rectangle_label_draft(payload, editing);
    let expected_layout = rectangle_label_layout(&draft, document.canvas_size_px).unwrap();
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
  fn text_width_extends_from_anchor_to_canvas_edge() {
    let canvas = SizePx::new(400, 200);
    assert_eq!(text_width_to_canvas_edge(PointPx::new(120.0, 20.0), canvas), 280.0);
    assert_eq!(text_width_to_canvas_edge(PointPx::new(-30.0, 20.0), canvas), 400.0);
    assert_eq!(text_width_to_canvas_edge(PointPx::new(399.5, 20.0), canvas), 1.0);
  }
}
