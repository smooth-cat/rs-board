use std::{
  ffi::OsStr,
  fs::{File, Metadata},
  path::{Component, Path},
};

use super::{StorageError, StorageResult};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResourceName(String);

impl ResourceName {
  pub fn new(value: impl Into<String>) -> StorageResult<Self> {
    let value = value.into();
    let path = Path::new(&value);
    let mut components = path.components();
    let is_single_normal =
      matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if value.is_empty()
      || value == "."
      || value == ".."
      || value.contains('/')
      || value.contains('\\')
      || value.contains('\0')
      || !is_single_normal
    {
      return Err(StorageError::UnsafeResourceName(value));
    }
    Ok(Self(value))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }

  pub fn as_os_str(&self) -> &OsStr {
    OsStr::new(&self.0)
  }
}

impl std::fmt::Display for ResourceName {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.0)
  }
}

pub fn open_regular_file(directory: &Path, name: &ResourceName) -> StorageResult<File> {
  let path = directory.join(name.as_os_str());
  open_regular_path_with_label(&path, &name.to_string())
}

pub fn open_regular_path(path: &Path) -> StorageResult<File> {
  let label = path.file_name().and_then(|name| name.to_str()).unwrap_or("resource");
  open_regular_path_with_label(path, label)
}

fn open_regular_path_with_label(path: &Path, label: &str) -> StorageResult<File> {
  let before = std::fs::symlink_metadata(path).map_err(|error| {
    if error.kind() == std::io::ErrorKind::NotFound {
      StorageError::MissingResource(label.to_owned())
    } else {
      StorageError::io("inspecting a resource", error)
    }
  })?;
  if !before.file_type().is_file() {
    return Err(StorageError::InvalidResourceType(label.to_owned()));
  }

  let file = File::open(path).map_err(|error| StorageError::io("opening a resource", error))?;
  let opened =
    file.metadata().map_err(|error| StorageError::io("inspecting an opened resource", error))?;
  if !same_file(&before, &opened) {
    return Err(StorageError::ResourceChanged(label.to_owned()));
  }
  Ok(file)
}

#[cfg(unix)]
fn same_file(left: &Metadata, right: &Metadata) -> bool {
  use std::os::unix::fs::MetadataExt;
  left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(left: &Metadata, right: &Metadata) -> bool {
  left.len() == right.len()
    && left.modified().ok() == right.modified().ok()
    && right.file_type().is_file()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn resource_name_accepts_only_one_plain_component() {
    assert!(ResourceName::new("document.png").is_ok());
    for unsafe_name in [
      "",
      ".",
      "..",
      "../document.png",
      "/tmp/document.png",
      "nested/document.png",
      "nested\\document.png",
      "document.png\0ignored",
    ] {
      assert!(ResourceName::new(unsafe_name).is_err(), "{unsafe_name:?}");
    }
  }
}
