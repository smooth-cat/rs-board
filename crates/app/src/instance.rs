use std::{
  fs,
  hash::{Hash, Hasher},
  io::{Read, Write},
  path::{Path, PathBuf},
  sync::{
    Arc,
    mpsc::{self, Receiver},
  },
  thread,
};

use single_instance::SingleInstance;
use thiserror::Error;

#[cfg(not(target_os = "macos"))]
const INSTANCE_NAME: &str = "com.linjiajian.rs-board";
#[cfg(target_os = "macos")]
const INSTANCE_LOCK_FILE: &str = ".instance.lock";

pub enum InstanceRole {
  Primary(InstanceBridge),
  Secondary,
}

pub struct InstanceBridge {
  _guard: SingleInstance,
  receiver: Receiver<Vec<PathBuf>>,
  socket_path: PathBuf,
}

impl InstanceBridge {
  pub fn acquire(
    app_data_dir: &Path,
    startup_files: Vec<PathBuf>,
  ) -> Result<InstanceRole, InstanceError> {
    Self::acquire_with_waker(app_data_dir, startup_files, || {})
  }

  pub fn acquire_with_waker(
    app_data_dir: &Path,
    startup_files: Vec<PathBuf>,
    wake: impl Fn() + Send + Sync + 'static,
  ) -> Result<InstanceRole, InstanceError> {
    let guard = acquire_instance_guard(app_data_dir)?;
    let socket_path = instance_socket_path(app_data_dir);
    if !guard.is_single() {
      forward_to_primary(&socket_path, &startup_files)?;
      return Ok(InstanceRole::Secondary);
    }

    #[cfg(unix)]
    let receiver = start_listener(&socket_path, Arc::new(wake))?;
    #[cfg(not(unix))]
    let (_sender, receiver) = mpsc::channel();

    Ok(InstanceRole::Primary(Self { _guard: guard, receiver, socket_path }))
  }

  pub fn try_recv(&self) -> Option<Vec<PathBuf>> {
    self.receiver.try_recv().ok()
  }
}

fn acquire_instance_guard(app_data_dir: &Path) -> Result<SingleInstance, InstanceError> {
  fs::create_dir_all(app_data_dir)?;

  // macOS 版 single-instance 会把名称直接当作锁文件路径，因此必须使用当前用户
  // Application Support 下的绝对路径，不能依赖 Finder 或终端启动时的工作目录。
  #[cfg(target_os = "macos")]
  let instance_name = app_data_dir.join(INSTANCE_LOCK_FILE).to_string_lossy().into_owned();
  #[cfg(not(target_os = "macos"))]
  let instance_name = INSTANCE_NAME.to_owned();

  SingleInstance::new(&instance_name).map_err(|error| InstanceError::Lock(error.to_string()))
}

fn instance_socket_path(app_data_dir: &Path) -> PathBuf {
  let mut hasher = std::collections::hash_map::DefaultHasher::new();
  app_data_dir.hash(&mut hasher);
  std::env::temp_dir().join(format!("rs-board-{:016x}.sock", hasher.finish()))
}

impl Drop for InstanceBridge {
  fn drop(&mut self) {
    #[cfg(unix)]
    let _ = fs::remove_file(&self.socket_path);
  }
}

#[cfg(unix)]
fn start_listener(
  socket_path: &Path,
  wake: Arc<dyn Fn() + Send + Sync>,
) -> Result<Receiver<Vec<PathBuf>>, InstanceError> {
  use std::os::unix::net::UnixListener;

  match fs::remove_file(socket_path) {
    Ok(()) => {}
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
    Err(error) => return Err(error.into()),
  }
  let listener = UnixListener::bind(socket_path)?;
  let (sender, receiver) = mpsc::channel();
  thread::Builder::new().name("rs-board-instance-listener".into()).spawn(move || {
    for connection in listener.incoming() {
      let Ok(mut stream) = connection else {
        continue;
      };
      let mut bytes = Vec::new();
      if stream.read_to_end(&mut bytes).is_ok()
        && let Ok(paths) = serde_json::from_slice::<Vec<PathBuf>>(&bytes)
      {
        let _ = sender.send(paths);
        wake();
      }
    }
  })?;
  Ok(receiver)
}

#[cfg(unix)]
fn forward_to_primary(socket_path: &Path, files: &[PathBuf]) -> Result<(), InstanceError> {
  use std::{os::unix::net::UnixStream, time::Duration};

  let bytes = serde_json::to_vec(files)?;
  for attempt in 0..10 {
    match UnixStream::connect(socket_path) {
      Ok(mut stream) => {
        stream.write_all(&bytes)?;
        return Ok(());
      }
      Err(error) if attempt < 9 => {
        let _ = error;
        thread::sleep(Duration::from_millis(50));
      }
      Err(error) => return Err(error.into()),
    }
  }
  unreachable!("the retry loop always returns")
}

#[cfg(not(unix))]
fn forward_to_primary(_socket_path: &Path, _files: &[PathBuf]) -> Result<(), InstanceError> {
  Ok(())
}

#[derive(Debug, Error)]
pub enum InstanceError {
  #[error("无法创建单实例锁: {0}")]
  Lock(String),
  #[error("单实例通信失败: {0}")]
  Io(#[from] std::io::Error),
  #[error("单实例消息格式失败: {0}")]
  Json(#[from] serde_json::Error),
}

#[cfg(all(test, unix))]
mod tests {
  use super::*;
  use std::time::{Duration, Instant};
  use uuid::Uuid;

  #[test]
  fn listener_receives_a_file_group() {
    let root = std::env::temp_dir().join(format!("rs-board-instance-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let socket = instance_socket_path(&root);
    let receiver = match start_listener(&socket, Arc::new(|| {})) {
      Ok(receiver) => receiver,
      Err(InstanceError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
        let _ = fs::remove_dir_all(root);
        eprintln!(
          "skipping Unix socket listener test because this sandbox forbids socket bind: {error}"
        );
        return;
      }
      Err(error) => panic!("failed to start instance listener: {error}"),
    };
    let expected = vec![PathBuf::from("one.rsboard"), PathBuf::from("two.rsboard")];
    forward_to_primary(&socket, &expected).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    let actual = loop {
      if let Ok(paths) = receiver.try_recv() {
        break paths;
      }
      assert!(Instant::now() < deadline);
      thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(actual, expected);
    fs::remove_file(socket).unwrap();
    fs::remove_dir_all(root).unwrap();
  }

  #[test]
  fn socket_path_stays_below_the_macos_unix_limit() {
    let deliberately_long = PathBuf::from("a".repeat(500));
    let socket = instance_socket_path(&deliberately_long);
    assert!(socket.as_os_str().len() < 104);
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn instance_lock_is_created_inside_the_app_data_directory() {
    let root = std::env::temp_dir().join(format!("rs-board-instance-lock-{}", Uuid::new_v4()));
    let lock_path = root.join(INSTANCE_LOCK_FILE);

    let primary = acquire_instance_guard(&root).unwrap();
    assert!(primary.is_single());
    assert!(lock_path.is_file());

    let secondary = acquire_instance_guard(&root).unwrap();
    assert!(!secondary.is_single());

    drop(secondary);
    drop(primary);
    fs::remove_dir_all(root).unwrap();
  }
}
