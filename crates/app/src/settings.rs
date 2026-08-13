use std::{
  fs::{self, File},
  io::{self, Write},
  path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use common::ColorRgba;

const SETTINGS_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
  pub version: u32,
  pub global_hotkey: String,
  pub include_cursor: bool,
  pub launch_at_login: bool,
  pub copy_image_after_save: bool,
  #[serde(default = "default_global_color")]
  pub global_color: ColorRgba,
  pub tool_styles: ToolDefaultStyles,
}

impl Default for Settings {
  fn default() -> Self {
    Self {
      version: SETTINGS_VERSION,
      global_hotkey: "F1".to_owned(),
      include_cursor: false,
      launch_at_login: false,
      copy_image_after_save: true,
      global_color: default_global_color(),
      tool_styles: ToolDefaultStyles::default(),
    }
  }
}

fn default_global_color() -> ColorRgba {
  ColorRgba::RED
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefaultStyle {
  pub color_rgba: ColorRgba,
  pub width_px: f32,
  pub font_size_px: f32,
  pub hardness: f32,
}

impl ToolDefaultStyle {
  pub const fn new(color_rgba: ColorRgba, width_px: f32, font_size_px: f32, hardness: f32) -> Self {
    Self { color_rgba, width_px, font_size_px, hardness }
  }
}

impl Default for ToolDefaultStyle {
  fn default() -> Self {
    Self::new(ColorRgba::RED, 8.0, 24.0, 1.0)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolDefaultStyles {
  #[serde(default = "default_rectangle_style")]
  pub rectangle: ToolDefaultStyle,
  #[serde(default = "default_arrow_style")]
  pub arrow: ToolDefaultStyle,
  #[serde(default = "default_text_style")]
  pub text: ToolDefaultStyle,
  #[serde(default = "default_stroke_style")]
  pub stroke: ToolDefaultStyle,
  #[serde(default = "default_sequence_style")]
  pub sequence: ToolDefaultStyle,
}

impl Default for ToolDefaultStyles {
  fn default() -> Self {
    Self {
      rectangle: default_rectangle_style(),
      arrow: default_arrow_style(),
      text: default_text_style(),
      stroke: default_stroke_style(),
      sequence: default_sequence_style(),
    }
  }
}

fn default_rectangle_style() -> ToolDefaultStyle {
  ToolDefaultStyle::new(ColorRgba::RED, 8.0, 36.0, 1.0)
}

fn default_arrow_style() -> ToolDefaultStyle {
  ToolDefaultStyle::new(ColorRgba::RED, 8.0, 24.0, 1.0)
}

fn default_text_style() -> ToolDefaultStyle {
  ToolDefaultStyle::new(ColorRgba::RED, 8.0, 36.0, 1.0)
}

fn default_stroke_style() -> ToolDefaultStyle {
  ToolDefaultStyle::new(ColorRgba::RED, 8.0, 24.0, 0.0)
}

fn default_sequence_style() -> ToolDefaultStyle {
  ToolDefaultStyle::new(ColorRgba::RED, 8.0, 24.0, 1.0)
}

impl Settings {
  pub fn app_data_dir() -> Result<PathBuf, SettingsError> {
    // 由系统目录 API 按当前登录用户解析；macOS 下对应
    // ~/Library/Application Support/com.linjiajian.RS-Board。
    ProjectDirs::from("com", "linjiajian", "RS Board")
      .map(|dirs| dirs.data_dir().to_path_buf())
      .ok_or(SettingsError::DataDirectoryUnavailable)
  }

  pub fn default_path() -> Result<PathBuf, SettingsError> {
    Ok(Self::app_data_dir()?.join("settings.json"))
  }

  pub fn load_or_default(path: &Path) -> Result<Self, SettingsError> {
    match fs::read(path) {
      Ok(bytes) => {
        let settings: Self = serde_json::from_slice(&bytes)?;
        if settings.version != SETTINGS_VERSION {
          return Err(SettingsError::UnsupportedVersion(settings.version));
        }
        Ok(settings)
      }
      Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
      Err(error) => Err(error.into()),
    }
  }

  pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
    if self.version != SETTINGS_VERSION {
      return Err(SettingsError::UnsupportedVersion(self.version));
    }
    let parent = path.parent().ok_or(SettingsError::InvalidPath)?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(".settings.json.tmp");
    let result = (|| {
      let mut file = File::create(&temporary)?;
      serde_json::to_writer_pretty(&mut file, self)?;
      file.write_all(b"\n")?;
      file.sync_all()?;
      fs::rename(&temporary, path)?;
      File::open(parent)?.sync_all()?;
      Ok::<_, SettingsError>(())
    })();
    if result.is_err() {
      let _ = fs::remove_file(&temporary);
    }
    result
  }
}

#[derive(Debug, Error)]
pub enum SettingsError {
  #[error("无法确定应用数据目录")]
  DataDirectoryUnavailable,
  #[error("设置路径无效")]
  InvalidPath,
  #[error("不支持的设置版本: {0}")]
  UnsupportedVersion(u32),
  #[error("读写设置失败: {0}")]
  Io(#[from] io::Error),
  #[error("设置格式无效: {0}")]
  Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
  use super::*;
  use uuid::Uuid;

  fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!("rs-board-settings-{}", Uuid::new_v4())).join("settings.json")
  }

  #[test]
  fn defaults_match_the_mvp() {
    let settings = Settings::default();
    assert_eq!(settings.global_hotkey, "F1");
    assert!(!settings.include_cursor);
    assert!(!settings.launch_at_login);
    assert!(settings.copy_image_after_save);
    assert_eq!(settings.global_color, ColorRgba::RED);
    assert_eq!(
      settings.tool_styles.rectangle,
      ToolDefaultStyle::new(ColorRgba::RED, 8.0, 36.0, 1.0)
    );
    assert_eq!(settings.tool_styles.arrow, ToolDefaultStyle::new(ColorRgba::RED, 8.0, 24.0, 1.0));
    assert_eq!(settings.tool_styles.text, ToolDefaultStyle::new(ColorRgba::RED, 8.0, 36.0, 1.0));
    assert_eq!(settings.tool_styles.stroke, ToolDefaultStyle::new(ColorRgba::RED, 8.0, 24.0, 0.0));
    assert_eq!(
      settings.tool_styles.sequence,
      ToolDefaultStyle::new(ColorRgba::RED, 8.0, 24.0, 1.0)
    );
  }

  #[test]
  fn settings_round_trip() {
    let path = temp_path();
    let settings =
      Settings { include_cursor: true, global_color: ColorRgba::BLUE, ..Settings::default() };
    settings.save(&path).unwrap();

    let serialized: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
      serialized.get("global_color"),
      Some(&serde_json::to_value(ColorRgba::BLUE).unwrap())
    );
    assert_eq!(Settings::load_or_default(&path).unwrap(), settings);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
  }

  #[test]
  fn missing_settings_use_defaults() {
    assert_eq!(Settings::load_or_default(&temp_path()).unwrap(), Settings::default());
  }

  #[test]
  fn legacy_settings_without_tool_styles_fill_defaults() {
    let path = temp_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
      &path,
      br#"{
  "version": 1,
  "global_hotkey": "F2",
  "include_cursor": true,
  "launch_at_login": false,
  "copy_image_after_save": false
}"#,
    )
    .unwrap();

    let loaded = Settings::load_or_default(&path).unwrap();

    assert_eq!(loaded.global_hotkey, "F2");
    assert!(loaded.include_cursor);
    assert!(!loaded.copy_image_after_save);
    assert_eq!(loaded.tool_styles, ToolDefaultStyles::default());
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
  }

  #[test]
  fn legacy_settings_without_global_color_preserve_existing_values() {
    let path = temp_path();
    let expected = Settings {
      global_hotkey: "F2".to_owned(),
      include_cursor: true,
      launch_at_login: true,
      copy_image_after_save: false,
      global_color: ColorRgba::RED,
      tool_styles: ToolDefaultStyles {
        rectangle: ToolDefaultStyle::new(ColorRgba::BLUE, 12.0, 48.0, 0.75),
        arrow: ToolDefaultStyle::new(ColorRgba::GREEN, 4.0, 18.0, 0.5),
        text: ToolDefaultStyle::new(ColorRgba::WHITE, 6.0, 30.0, 1.0),
        stroke: ToolDefaultStyle::new(ColorRgba::YELLOW, 16.0, 20.0, 0.25),
        sequence: ToolDefaultStyle::new(ColorRgba::BLACK, 10.0, 42.0, 0.9),
      },
      ..Settings::default()
    };
    let mut legacy_json = serde_json::to_value(&expected).unwrap();
    assert!(legacy_json.as_object_mut().unwrap().remove("global_color").is_some());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec_pretty(&legacy_json).unwrap()).unwrap();

    let loaded = Settings::load_or_default(&path).unwrap();

    assert_eq!(loaded.global_color, ColorRgba::RED);
    assert_eq!(loaded, expected);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
  }

  #[test]
  fn saving_global_color_does_not_rewrite_legacy_tool_colors() {
    let path = temp_path();
    let mut settings = Settings::default();
    settings.tool_styles.rectangle.color_rgba = ColorRgba::BLUE;
    settings.tool_styles.arrow.color_rgba = ColorRgba::GREEN;
    settings.tool_styles.text.color_rgba = ColorRgba::WHITE;
    settings.tool_styles.stroke.color_rgba = ColorRgba::YELLOW;
    settings.tool_styles.sequence.color_rgba = ColorRgba::BLACK;
    let legacy_tool_styles = settings.tool_styles;

    settings.global_color = ColorRgba::GREEN;
    settings.save(&path).unwrap();
    let loaded = Settings::load_or_default(&path).unwrap();

    assert_eq!(loaded.global_color, ColorRgba::GREEN);
    assert_eq!(loaded.tool_styles, legacy_tool_styles);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
  }
}
