use std::{
  fs::File,
  io::{Read, Seek, SeekFrom},
  path::Path,
};

use super::{ResourceName, StorageError, StorageResult, open_regular_file, open_regular_path};

pub(crate) const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_METADATA_BYTES: u64 = 64 * 1024;

pub(crate) fn read_named_file(
  directory: &Path,
  name: &ResourceName,
  maximum_bytes: u64,
) -> StorageResult<Vec<u8>> {
  let file = open_regular_file(directory, name)?;
  read_bounded(file, maximum_bytes)
}

pub(crate) fn read_regular_path(path: &Path, maximum_bytes: u64) -> StorageResult<Vec<u8>> {
  let file = open_regular_path(path)?;
  read_bounded(file, maximum_bytes)
}

pub(crate) fn read_bounded(mut file: File, maximum_bytes: u64) -> StorageResult<Vec<u8>> {
  let length = file
    .metadata()
    .map_err(|error| StorageError::io("inspecting a file before reading", error))?
    .len();
  if length > maximum_bytes {
    return Err(StorageError::InvalidManifest(format!(
      "file exceeds the {maximum_bytes} byte limit"
    )));
  }
  file.seek(SeekFrom::Start(0)).map_err(|error| StorageError::io("seeking a file", error))?;
  let capacity = usize::try_from(length).unwrap_or(0);
  let mut bytes = Vec::with_capacity(capacity);
  file
    .take(maximum_bytes + 1)
    .read_to_end(&mut bytes)
    .map_err(|error| StorageError::io("reading a file", error))?;
  if bytes.len() as u64 > maximum_bytes {
    return Err(StorageError::InvalidManifest(format!(
      "file exceeds the {maximum_bytes} byte limit"
    )));
  }
  Ok(bytes)
}

pub(crate) fn require_plain_directory(path: &Path) -> StorageResult<()> {
  let metadata = std::fs::symlink_metadata(path)
    .map_err(|error| StorageError::io("inspecting a directory", error))?;
  if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
    return Err(StorageError::InvalidResourceType(
      path.file_name().and_then(|name| name.to_str()).unwrap_or("directory").to_owned(),
    ));
  }
  Ok(())
}

pub(crate) fn reject_managed_destination(root: &Path, destination: &Path) -> StorageResult<()> {
  require_plain_directory(destination)?;
  let canonical_root = root
    .canonicalize()
    .map_err(|error| StorageError::io("resolving the application data directory", error))?;
  let canonical_destination = destination
    .canonicalize()
    .map_err(|error| StorageError::io("resolving an export directory", error))?;
  if canonical_destination.starts_with(&canonical_root) {
    return Err(StorageError::ManagedDestination);
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::io::Write;

  use uuid::Uuid;

  use super::*;

  fn temporary_directory(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("rs-board-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir(&path).unwrap();
    path
  }

  #[test]
  fn bounded_read_rejects_oversized_files() {
    let root = temporary_directory("bounded-read");
    let path = root.join("manifest");
    let mut file = File::create(&path).unwrap();
    file.write_all(&[0; 9]).unwrap();
    drop(file);
    assert!(read_regular_path(&path, 8).is_err());
    std::fs::remove_dir_all(root).unwrap();
  }

  #[cfg(unix)]
  #[test]
  fn regular_file_reader_rejects_symlinks() {
    use std::os::unix::fs::symlink;

    let root = temporary_directory("symlink-read");
    std::fs::write(root.join("target"), b"content").unwrap();
    symlink(root.join("target"), root.join("link")).unwrap();
    assert!(read_regular_path(&root.join("link"), 64).is_err());
    std::fs::remove_dir_all(root).unwrap();
  }

  #[test]
  fn managed_export_destination_is_rejected() {
    let root = temporary_directory("managed-root");
    let nested = root.join("documents");
    std::fs::create_dir(&nested).unwrap();
    assert!(reject_managed_destination(&root, &nested).is_err());
    std::fs::remove_dir_all(root).unwrap();
  }
}
