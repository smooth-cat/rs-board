use std::{
  fs::{self, File},
  io::{self, Write},
  path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const SETTINGS_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
  pub version: u32,
  pub global_hotkey: String,
  pub include_cursor: bool,
  pub launch_at_login: bool,
  pub copy_image_after_save: bool,
}

impl Default for Settings {
  fn default() -> Self {
    Self {
      version: SETTINGS_VERSION,
      global_hotkey: "F1".to_owned(),
      include_cursor: false,
      launch_at_login: false,
      copy_image_after_save: true,
    }
  }
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
  }

  #[test]
  fn settings_round_trip() {
    let path = temp_path();
    let settings = Settings { include_cursor: true, ..Settings::default() };
    settings.save(&path).unwrap();
    assert_eq!(Settings::load_or_default(&path).unwrap(), settings);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
  }

  #[test]
  fn missing_settings_use_defaults() {
    assert_eq!(Settings::load_or_default(&temp_path()).unwrap(), Settings::default());
  }
}
