use common::{
  ArrowEndpoint, ArrowHead, ArrowPayload, BoardDocument, ColorRgba, CommandBatch, CommandHistory,
  DocumentCommand, Element, ElementId, ElementPayload, LabelPlacementPreference,
  PRESET_FONT_SIZES_PX, PRESET_STROKE_WIDTHS_PX, PointPx, RectPx, RectangleLabel, RectanglePayload,
  SequenceMarkerPayload, SizePx, StrokePayload, StrokeStyle, StyleChange, TextPayload, TextStyle,
  minimum_geometry_extent, rectangle_label_layout,
};
use eframe::egui::{
  self, Align2, Color32, CursorIcon, Event, FontId, Id, Key, Modifiers, Pos2, Rect, Response,
  Sense, Stroke, StrokeKind, TextureHandle,
};

use crate::renderer::{paint_document, paint_element, paint_raw_polyline};

const DEFAULT_RECTANGLE_LABEL: &str = "标题";
const DEFAULT_TEXT_BOX_WIDTH_PX: f32 = 420.0;
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
  style: ToolStyle,
  request_focus: bool,
  select_all: bool,
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
  CommitText,
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
    background: &TextureHandle,
  ) -> Vec<EditorAction> {
    let ctx = root_ui.ctx().clone();
    if self.selected_element_id.is_some_and(|id| document.element(id).is_none()) {
      self.selected_element_id = None;
    }

    let mut actions = Vec::new();

    let mut transform = None;
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::BLACK)).show(
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
        painter.image(
          background.id(),
          canvas_transform.canvas_rect(),
          Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
          Color32::WHITE,
        );
        paint_document(&painter, &canvas_transform, document);
        self.paint_interaction(&painter, canvas_transform, document);
        self.paint_selection(&painter, canvas_transform, document);
      },
    );

    if let Some(transform) = transform {
      self.show_text_editor(&ctx, transform, document, &mut actions);
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
    background: &TextureHandle,
  ) {
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::BLACK)).show(
      root_ui,
      |ui| {
        let Some(canvas_transform) = CanvasTransform::fit(document.canvas_size_px, ui.max_rect())
        else {
          return;
        };
        let painter = ui.painter().with_clip_rect(canvas_transform.canvas_rect());
        painter.image(
          background.id(),
          canvas_transform.canvas_rect(),
          Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
          Color32::WHITE,
        );
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
    background: &TextureHandle,
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
      ShortcutAction::CommitText => self.commit_text(document, actions),
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
          self.text_editing = Some(TextEditing {
            target: TextTarget::NewText { anchor_px: position },
            buffer: String::new(),
            style: self.tool_style(EditorTool::Text),
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
            actions.push(command_action(DocumentCommand::AddElement { element }));
            self.selected_element_id = Some(element_id);
            self.text_editing = Some(TextEditing {
              target: TextTarget::RectangleLabel { element_id },
              buffer: DEFAULT_RECTANGLE_LABEL.to_owned(),
              style: self.tool_style(EditorTool::Rectangle),
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
          style: ToolStyle {
            color_rgba: payload.text_style.color_rgba,
            width_px: 8.0,
            font_size_px: payload.text_style.font_size_px,
          },
          request_focus: true,
          select_all: false,
        });
      }
      ElementPayload::Rectangle(payload) => {
        self.text_editing = Some(TextEditing {
          target: TextTarget::RectangleLabel { element_id },
          buffer: payload.label.text.clone(),
          style: ToolStyle {
            color_rgba: payload.stroke_style.color_rgba,
            width_px: payload.stroke_style.width_px,
            font_size_px: payload.label.text_style.font_size_px,
          },
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
        let box_width_px = DEFAULT_TEXT_BOX_WIDTH_PX
          .min((document.canvas_size_px.width_px as f32 - anchor_px.x_px).max(1.0));
        let Ok(text_style) = TextStyle::mvp(editing.style.color_rgba, editing.style.font_size_px)
        else {
          return;
        };
        let Ok(element) = Element::new(
          ElementId::new(),
          document.elements.len() as i64,
          ElementPayload::Text(TextPayload {
            anchor_px,
            text: editing.buffer,
            box_width_px,
            text_style,
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
        if payload.text == editing.buffer {
          return;
        }
        let mut payload = payload.clone();
        payload.text = editing.buffer;
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
    actions: &mut Vec<EditorAction>,
  ) {
    let Some(editing) = self.text_editing.as_ref() else {
      return;
    };
    let origin = match editing.target {
      TextTarget::NewText { anchor_px } => anchor_px,
      TextTarget::ExistingText { element_id } => document
        .element(element_id)
        .and_then(|element| match &element.payload {
          ElementPayload::Text(payload) => Some(payload.anchor_px),
          _ => None,
        })
        .unwrap_or(PointPx::ZERO),
      TextTarget::RectangleLabel { element_id } => document
        .element(element_id)
        .and_then(|element| match &element.payload {
          ElementPayload::Rectangle(payload) => {
            rectangle_label_layout(payload, document.canvas_size_px)
              .ok()
              .map(|layout| layout.bounds_px.min)
          }
          _ => None,
        })
        .unwrap_or(PointPx::ZERO),
    };
    let screen_position = transform.document_to_egui(origin);
    let editor_id = Id::new("rs-board-inline-text-editor");
    let mut lost_focus = false;
    let editing = self.text_editing.as_mut().expect("text editor checked above");
    egui::Area::new(Id::new("rs-board-inline-text-area"))
      .order(egui::Order::Foreground)
      .fixed_pos(screen_position)
      .constrain_to(ctx.content_rect())
      .show(ctx, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
          let width = (DEFAULT_TEXT_BOX_WIDTH_PX * transform.scale()).clamp(160.0, 520.0);
          let mut output = egui::TextEdit::multiline(&mut editing.buffer)
            .id(editor_id)
            .font(FontId::proportional(
              (editing.style.font_size_px * transform.scale()).clamp(12.0, 64.0),
            ))
            .desired_width(width)
            .desired_rows(1)
            .return_key(None)
            .show(ui);
          if editing.request_focus {
            output.response.request_focus();
            if editing.select_all {
              use egui::text::{CCursor, CCursorRange};
              output.state.cursor.set_char_range(Some(CCursorRange::two(
                CCursor::new(0),
                CCursor::new(editing.buffer.chars().count()),
              )));
              output.state.store(ctx, editor_id);
            }
            editing.request_focus = false;
          } else {
            lost_focus = output.response.lost_focus();
          }
        });
      });
    if lost_focus {
      self.commit_text(document, actions);
    }
  }

  fn show_toolbar(
    &mut self,
    ctx: &egui::Context,
    document: &BoardDocument,
    history: &CommandHistory,
    actions: &mut Vec<EditorAction>,
  ) {
    egui::Area::new(Id::new("rs-board-toolbar"))
      .order(egui::Order::Foreground)
      .anchor(Align2::CENTER_TOP, egui::vec2(0.0, 10.0))
      .show(ctx, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
          ui.horizontal(|ui| {
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
          });
        });
      });
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
      .selected_text(egui::RichText::new("●").color(color).size(20.0))
      .width(38.0)
      .show_ui(ui, |ui| {
        for choice in mvp_colors() {
          let text = egui::RichText::new("●").color(color32(choice)).size(20.0);
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
      .show_ui(ui, |ui| {
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
      .show_ui(ui, |ui| {
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
      .order(egui::Order::Foreground)
      .fixed_pos(anchor + egui::vec2(14.0, 14.0))
      .constrain_to(ctx.content_rect())
      .show(ctx, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
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
    if key == Key::Enter && !modifiers.shift && !command {
      return Some(ShortcutAction::CommitText);
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

#[cfg(test)]
mod tests {
  use chrono::{TimeZone, Utc};
  use common::{CapturedDisplay, DocumentId, GlobalBoundsPx};
  use uuid::Uuid;

  use super::*;

  fn document() -> BoardDocument {
    BoardDocument::new_capture(
      DocumentId::from_uuid(Uuid::nil()),
      SizePx::new(400, 200),
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx { x_px: 0, y_px: 0, width_px: 400, height_px: 200 },
        scale_factor: 1.0,
      },
      Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).unwrap(),
    )
    .unwrap()
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
    assert_eq!(map_shortcut(Key::Enter, Modifiers::NONE, true), Some(ShortcutAction::CommitText));
    assert_eq!(map_shortcut(Key::Enter, Modifiers::SHIFT, true), None);
  }
}
