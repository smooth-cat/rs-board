use std::{
  ffi::CString,
  fs::{File, OpenOptions},
  io::Write,
  path::{Path, PathBuf},
};

use uuid::Uuid;

#[cfg(test)]
use std::cell::RefCell;

use super::{StorageError, StorageResult};
use crate::performance::{PerformanceContext, PerformanceDetails, PerformanceTimer};

#[derive(Clone, Copy)]
pub(crate) struct AtomicTrace<'a> {
  pub context: &'a PerformanceContext,
  pub workflow: &'static str,
  pub resource: &'static str,
}

impl<'a> AtomicTrace<'a> {
  pub fn new(
    context: &'a PerformanceContext,
    workflow: &'static str,
    resource: &'static str,
  ) -> Self {
    Self { context, workflow, resource }
  }

  fn with_resource(self, resource: &'static str) -> Self {
    Self { resource, ..self }
  }

  fn details(self) -> PerformanceDetails {
    PerformanceDetails::default().workflow(self.workflow).resource(self.resource)
  }
}

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
  write_new_file_impl(path, bytes, None)
}

pub(crate) fn write_new_file_traced(
  path: &Path,
  bytes: &[u8],
  trace: AtomicTrace<'_>,
) -> StorageResult<()> {
  write_new_file_impl(path, bytes, Some(trace))
}

fn write_new_file_impl(
  path: &Path,
  bytes: &[u8],
  trace: Option<AtomicTrace<'_>>,
) -> StorageResult<()> {
  let details = trace.map(|trace| trace.details().byte_count(bytes.len()));
  let mut file = measured(trace, "persistence.file.open", details, || {
    OpenOptions::new()
      .create_new(true)
      .write(true)
      .open(path)
      .map_err(|error| StorageError::io("creating a file", error))
  })?;
  measured(trace, "persistence.file.write", details, || {
    file.write_all(bytes).map_err(|error| StorageError::io("writing a file", error))
  })?;
  measured(trace, "persistence.file.sync", details, || {
    file.sync_all().map_err(|error| StorageError::io("syncing a file", error))
  })?;
  Ok(())
}

pub(crate) fn write_file_atomically(path: &Path, bytes: &[u8]) -> StorageResult<()> {
  write_file_atomically_impl(path, bytes, None)
}

pub(crate) fn write_file_atomically_traced(
  path: &Path,
  bytes: &[u8],
  trace: AtomicTrace<'_>,
) -> StorageResult<()> {
  write_file_atomically_impl(path, bytes, Some(trace))
}

fn write_file_atomically_impl(
  path: &Path,
  bytes: &[u8],
  trace: Option<AtomicTrace<'_>>,
) -> StorageResult<()> {
  let parent = path.parent().ok_or_else(|| {
    StorageError::InvalidManifest("atomic destination has no parent directory".into())
  })?;
  std::fs::create_dir_all(parent)
    .map_err(|error| StorageError::io("creating an atomic file parent", error))?;
  let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("resource");
  let temporary = parent.join(format!(".{file_name}.tmp-{}", Uuid::new_v4()));
  let result = (|| {
    write_new_file_impl(&temporary, bytes, trace)?;
    measured(trace, "persistence.atomic_file.rename", trace.map(AtomicTrace::details), || {
      std::fs::rename(&temporary, path)
        .map_err(|error| StorageError::io("committing an atomic file", error))
    })?;
    sync_directory_impl(parent, trace.map(|trace| trace.with_resource("parent_directory")))?;
    Ok(())
  })();
  if result.is_err() {
    let _ = std::fs::remove_file(&temporary);
  }
  result
}

pub(crate) fn sync_directory(path: &Path) -> StorageResult<()> {
  sync_directory_impl(path, None)
}

fn sync_directory_impl(path: &Path, trace: Option<AtomicTrace<'_>>) -> StorageResult<()> {
  measured(trace, "persistence.directory.sync", trace.map(AtomicTrace::details), || {
    File::open(path)
      .and_then(|directory| directory.sync_all())
      .map_err(|error| StorageError::io("syncing a directory", error))
  })
}

pub(crate) fn commit_new_directory(staging: &Path, destination: &Path) -> StorageResult<()> {
  commit_new_directory_impl(staging, destination, None)
}

pub(crate) fn commit_new_directory_traced(
  staging: &Path,
  destination: &Path,
  trace: AtomicTrace<'_>,
) -> StorageResult<()> {
  commit_new_directory_impl(staging, destination, Some(trace))
}

fn commit_new_directory_impl(
  staging: &Path,
  destination: &Path,
  trace: Option<AtomicTrace<'_>>,
) -> StorageResult<()> {
  if destination.exists() {
    return Err(StorageError::AlreadyExists(destination.to_path_buf()));
  }
  let parent = destination
    .parent()
    .ok_or_else(|| StorageError::InvalidManifest("directory destination has no parent".into()))?;
  sync_directory_impl(staging, trace.map(|trace| trace.with_resource("staging_directory")))?;
  measured(trace, "persistence.directory.rename", trace.map(AtomicTrace::details), || {
    std::fs::rename(staging, destination)
      .map_err(|error| StorageError::io("committing a directory", error))
  })?;
  sync_directory_impl(parent, trace.map(|trace| trace.with_resource("parent_directory")))
}

#[cfg(test)]
pub(crate) fn replace_directory(staging: &Path, destination: &Path) -> StorageResult<()> {
  replace_directory_impl(staging, destination, None)
}

pub(crate) fn replace_directory_traced(
  staging: &Path,
  destination: &Path,
  trace: AtomicTrace<'_>,
) -> StorageResult<()> {
  replace_directory_impl(staging, destination, Some(trace))
}

fn replace_directory_impl(
  staging: &Path,
  destination: &Path,
  trace: Option<AtomicTrace<'_>>,
) -> StorageResult<()> {
  let parent = destination
    .parent()
    .ok_or_else(|| StorageError::InvalidManifest("directory destination has no parent".into()))?;
  sync_directory_impl(staging, trace.map(|trace| trace.with_resource("staging_directory")))?;
  if !destination.exists() {
    measured(trace, "persistence.directory.rename", trace.map(AtomicTrace::details), || {
      std::fs::rename(staging, destination)
        .map_err(|error| StorageError::io("committing a directory", error))
    })?;
    return sync_directory_impl(parent, trace.map(|trace| trace.with_resource("parent_directory")));
  }

  #[cfg(target_os = "macos")]
  {
    measured(trace, "persistence.directory.swap", trace.map(AtomicTrace::details), || {
      rename_swap(staging, destination)
    })?;
    sync_directory_impl(parent, trace.map(|trace| trace.with_resource("parent_directory")))?;
    // After the swap, staging names the previous committed directory. Failure to
    // clean it does not make the new commit fail; startup cleanup removes it.
    let _ = std::fs::remove_dir_all(staging);
    Ok(())
  }

  #[cfg(not(target_os = "macos"))]
  {
    let backup = parent.join(format!(".old-{}", Uuid::new_v4()));
    measured(trace, "persistence.directory.rename", trace.map(AtomicTrace::details), || {
      std::fs::rename(destination, &backup)
        .map_err(|error| StorageError::io("backing up a directory", error))
    })?;
    if let Err(error) =
      measured(trace, "persistence.directory.rename", trace.map(AtomicTrace::details), || {
        std::fs::rename(staging, destination)
          .map_err(|error| StorageError::io("committing a replacement directory", error))
      })
    {
      let _ = std::fs::rename(&backup, destination);
      return Err(error);
    }
    sync_directory_impl(parent, trace.map(|trace| trace.with_resource("parent_directory")))?;
    let _ = std::fs::remove_dir_all(backup);
    Ok(())
  }
}

fn measured<T>(
  trace: Option<AtomicTrace<'_>>,
  stage: &'static str,
  details: Option<PerformanceDetails>,
  operation: impl FnOnce() -> StorageResult<T>,
) -> StorageResult<T> {
  #[cfg(test)]
  if let Some(trace) = trace
    && injected_fault_matches(stage, trace.resource)
  {
    return Err(StorageError::IncompleteCommit(format!(
      "injected storage fault at {stage} for {}",
      trace.resource
    )));
  }
  let timer =
    trace.map(|trace| PerformanceTimer::start(stage, *trace.context, details.unwrap_or_default()));
  let result = operation();
  if let Some(timer) = timer {
    match &result {
      Ok(_) => timer.finish_ok(),
      Err(error) => timer.finish_error(error),
    }
  }
  result
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct InjectedFault {
  stage: &'static str,
  resource: &'static str,
}

#[cfg(test)]
thread_local! {
  static INJECTED_FAULT: RefCell<Option<InjectedFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
struct FaultReset(Option<InjectedFault>);

#[cfg(test)]
impl Drop for FaultReset {
  fn drop(&mut self) {
    INJECTED_FAULT.with(|slot| *slot.borrow_mut() = self.0);
  }
}

#[cfg(test)]
pub(crate) fn with_injected_fault<T>(
  stage: &'static str,
  resource: &'static str,
  operation: impl FnOnce() -> T,
) -> T {
  let previous = INJECTED_FAULT.with(|slot| slot.replace(Some(InjectedFault { stage, resource })));
  let _reset = FaultReset(previous);
  operation()
}

#[cfg(test)]
fn injected_fault_matches(stage: &'static str, resource: &'static str) -> bool {
  INJECTED_FAULT.with(|slot| {
    slot.borrow().is_some_and(|fault| fault.stage == stage && fault.resource == resource)
  })
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
