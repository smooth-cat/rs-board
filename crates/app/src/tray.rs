use std::sync::mpsc::{self, Receiver};

use thiserror::Error;
use tray_icon::{
  Icon, TrayIcon, TrayIconBuilder,
  menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
  Capture,
  ShowRecent,
  RestoreDraft,
  ShowSettings,
  Quit,
}

pub struct TrayController {
  _icon: TrayIcon,
  capture: MenuItem,
  recent: MenuItem,
  restore: MenuItem,
  settings: MenuItem,
  quit: MenuItem,
  events: Receiver<MenuEvent>,
}

impl TrayController {
  pub fn new() -> Result<Self, TrayError> {
    Self::with_waker(|| {})
  }

  pub fn with_waker(wake: impl Fn() + Send + Sync + 'static) -> Result<Self, TrayError> {
    let (sender, events) = mpsc::channel();
    MenuEvent::set_event_handler(Some(move |event| {
      let _ = sender.send(event);
      wake();
    }));
    let menu = Menu::new();
    let capture = MenuItem::with_id("capture", "新截图  F1", true, None);
    let recent = MenuItem::with_id("recent", "最近讲义", true, None);
    let restore = MenuItem::with_id("restore", "恢复最新草稿", false, None);
    let settings = MenuItem::with_id("settings", "设置...", true, None);
    let quit = MenuItem::with_id("quit", "退出 RS Board", true, None);
    menu.append(&capture).map_err(display_error)?;
    menu.append(&recent).map_err(display_error)?;
    menu.append(&restore).map_err(display_error)?;
    menu.append(&PredefinedMenuItem::separator()).map_err(display_error)?;
    menu.append(&settings).map_err(display_error)?;
    menu.append(&PredefinedMenuItem::separator()).map_err(display_error)?;
    menu.append(&quit).map_err(display_error)?;

    let icon = Icon::from_rgba(template_icon(), 32, 32).map_err(display_error)?;
    let tray = TrayIconBuilder::new()
      .with_menu(Box::new(menu))
      .with_menu_on_left_click(true)
      .with_icon(icon)
      .with_icon_as_template(true)
      .with_tooltip("RS Board")
      .build()
      .map_err(display_error)?;
    Ok(Self { _icon: tray, capture, recent, restore, settings, quit, events })
  }

  pub fn set_availability(&self, busy: bool, draft_available: bool) {
    self.capture.set_enabled(!busy);
    self.restore.set_enabled(draft_available && !busy);
  }

  pub fn poll_action(&self) -> Option<TrayAction> {
    self.events.try_iter().find_map(|event| {
      if event.id == *self.capture.id() {
        Some(TrayAction::Capture)
      } else if event.id == *self.recent.id() {
        Some(TrayAction::ShowRecent)
      } else if event.id == *self.restore.id() {
        Some(TrayAction::RestoreDraft)
      } else if event.id == *self.settings.id() {
        Some(TrayAction::ShowSettings)
      } else if event.id == *self.quit.id() {
        Some(TrayAction::Quit)
      } else {
        None
      }
    })
  }

  pub fn menu_ids(&self) -> [&MenuId; 5] {
    [self.capture.id(), self.recent.id(), self.restore.id(), self.settings.id(), self.quit.id()]
  }
}

fn template_icon() -> Vec<u8> {
  let mut rgba = vec![0; 32 * 32 * 4];
  for y in 5..27 {
    for x in 4..28 {
      let border = !(7..25).contains(&x) || !(8..24).contains(&y);
      let stroke = (x + y >= 25 && x + y <= 28) && (10..23).contains(&x);
      if border || stroke {
        let index = (y * 32 + x) * 4;
        rgba[index + 3] = 255;
      }
    }
  }
  rgba
}

fn display_error(error: impl std::fmt::Display) -> TrayError {
  TrayError::Create(error.to_string())
}

#[derive(Debug, Error)]
pub enum TrayError {
  #[error("创建菜单栏状态项失败: {0}")]
  Create(String),
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn icon_has_stable_rgba_dimensions_and_visible_pixels() {
    let icon = template_icon();
    assert_eq!(icon.len(), 32 * 32 * 4);
    assert!(icon.chunks_exact(4).any(|pixel| pixel[3] == 255));
  }
}
