use std::{
  ffi::CString,
  fs::{File, OpenOptions},
  io::Write,
  path::{Path, PathBuf},
};

use uuid::Uuid;

use super::{StorageError, StorageResult};

pub(crate) fn create_staging_dir(parent: &Path, prefix: &str) -> StorageResult<PathBuf> {
  std::fs::create_dir_all(parent)
    .map_err(|error| StorageError::io("creating a staging parent directory", error))?;
  for _ in 0..8 {
    let path = parent.join(format!(".{prefix}-{}", Uuid::new_v4()));
    match std::fs::create_dir(&path) {
      Ok(()) => return Ok(path),
      Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
      Err(error) => return Err(StorageError::io("creating a staging directory", error)),
    }
  }
  Err(StorageError::AlreadyExists(parent.join(prefix)))
}

pub(crate) fn write_new_file(path: &Path, bytes: &[u8]) -> StorageResult<()> {
  let mut file = OpenOptions::new()
    .create_new(true)
    .write(true)
    .open(path)
    .map_err(|error| StorageError::io("creating a file", error))?;
  file.write_all(bytes).map_err(|error| StorageError::io("writing a file", error))?;
  file.sync_all().map_err(|error| StorageError::io("syncing a file", error))?;
  Ok(())
}

pub(crate) fn write_file_atomically(path: &Path, bytes: &[u8]) -> StorageResult<()> {
  let parent = path.parent().ok_or_else(|| {
    StorageError::InvalidManifest("atomic destination has no parent directory".into())
  })?;
  std::fs::create_dir_all(parent)
    .map_err(|error| StorageError::io("creating an atomic file parent", error))?;
  let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("resource");
  let temporary = parent.join(format!(".{file_name}.tmp-{}", Uuid::new_v4()));
  let result = (|| {
    write_new_file(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
      .map_err(|error| StorageError::io("committing an atomic file", error))?;
    sync_directory(parent)?;
    Ok(())
  })();
  if result.is_err() {
    let _ = std::fs::remove_file(&temporary);
  }
  result
}

pub(crate) fn sync_directory(path: &Path) -> StorageResult<()> {
  File::open(path)
    .and_then(|directory| directory.sync_all())
    .map_err(|error| StorageError::io("syncing a directory", error))
}

pub(crate) fn commit_new_directory(staging: &Path, destination: &Path) -> StorageResult<()> {
  if destination.exists() {
    return Err(StorageError::AlreadyExists(destination.to_path_buf()));
  }
  let parent = destination
    .parent()
    .ok_or_else(|| StorageError::InvalidManifest("directory destination has no parent".into()))?;
  sync_directory(staging)?;
  std::fs::rename(staging, destination)
    .map_err(|error| StorageError::io("committing a directory", error))?;
  sync_directory(parent)
}

pub(crate) fn replace_directory(staging: &Path, destination: &Path) -> StorageResult<()> {
  let parent = destination
    .parent()
    .ok_or_else(|| StorageError::InvalidManifest("directory destination has no parent".into()))?;
  sync_directory(staging)?;
  if !destination.exists() {
    std::fs::rename(staging, destination)
      .map_err(|error| StorageError::io("committing a directory", error))?;
    return sync_directory(parent);
  }

  #[cfg(target_os = "macos")]
  {
    rename_swap(staging, destination)?;
    sync_directory(parent)?;
    // After the swap, staging names the previous committed directory. Failure to
    // clean it does not make the new commit fail; startup cleanup removes it.
    let _ = std::fs::remove_dir_all(staging);
    Ok(())
  }

  #[cfg(not(target_os = "macos"))]
  {
    let backup = parent.join(format!(".old-{}", Uuid::new_v4()));
    std::fs::rename(destination, &backup)
      .map_err(|error| StorageError::io("backing up a directory", error))?;
    if let Err(error) = std::fs::rename(staging, destination) {
      let _ = std::fs::rename(&backup, destination);
      return Err(StorageError::io("committing a replacement directory", error));
    }
    sync_directory(parent)?;
    let _ = std::fs::remove_dir_all(backup);
    Ok(())
  }
}

pub(crate) fn remove_path_if_exists(path: &Path) -> StorageResult<()> {
  match std::fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
      std::fs::remove_dir_all(path).map_err(|error| StorageError::io("removing a directory", error))
    }
    Ok(_) => std::fs::remove_file(path).map_err(|error| StorageError::io("removing a file", error)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(StorageError::io("inspecting a path for removal", error)),
  }
}

#[cfg(target_os = "macos")]
fn rename_swap(left: &Path, right: &Path) -> StorageResult<()> {
  use std::os::unix::ffi::OsStrExt;

  const AT_FDCWD: i32 = -2;
  const RENAME_SWAP: u32 = 0x0000_0002;

  unsafe extern "C" {
    fn renameatx_np(
      from_fd: i32,
      from: *const std::ffi::c_char,
      to_fd: i32,
      to: *const std::ffi::c_char,
      flags: u32,
    ) -> i32;
  }

  let left = CString::new(left.as_os_str().as_bytes())
    .map_err(|_| StorageError::InvalidManifest("staging path contains NUL".into()))?;
  let right = CString::new(right.as_os_str().as_bytes())
    .map_err(|_| StorageError::InvalidManifest("destination path contains NUL".into()))?;
  // SAFETY: both C strings live for the duration of the call and are NUL terminated.
  let status =
    unsafe { renameatx_np(AT_FDCWD, left.as_ptr(), AT_FDCWD, right.as_ptr(), RENAME_SWAP) };
  if status == 0 {
    Ok(())
  } else {
    Err(StorageError::io("atomically swapping directories", std::io::Error::last_os_error()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn temporary_directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rs-board-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir(&path).unwrap();
    path
  }

  #[test]
  fn atomic_file_replaces_existing_contents() {
    let root = temporary_directory("atomic-file");
    let path = root.join("value.json");
    std::fs::write(&path, b"old").unwrap();
    write_file_atomically(&path, b"new").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    std::fs::remove_dir_all(root).unwrap();
  }

  #[test]
  fn directory_replacement_exposes_new_tree() {
    let root = temporary_directory("atomic-directory");
    let destination = root.join("latest");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("value"), b"old").unwrap();
    let staging = create_staging_dir(&root, "tmp").unwrap();
    std::fs::write(staging.join("value"), b"new").unwrap();

    replace_directory(&staging, &destination).unwrap();

    assert_eq!(std::fs::read(destination.join("value")).unwrap(), b"new");
    std::fs::remove_dir_all(root).unwrap();
  }
}
