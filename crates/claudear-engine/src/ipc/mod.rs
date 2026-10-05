//! Inter-process communication via Unix socket.
//!
//! Enables the CLI to communicate with a running watcher daemon.

mod client;
mod lock;
mod protocol;
mod server;

pub use client::{print_response, IpcClient};
pub use lock::Lock;
pub use protocol::{IpcCommand, IpcData, IpcResponse, WatcherState};
pub use server::IpcServer;

use std::path::{Path, PathBuf};

/// Returns a private runtime directory for IPC files, scoped to the current user.
///
/// On Linux, prefers `XDG_RUNTIME_DIR` (already user-private, typically mode 0700).
/// Otherwise, creates a subdirectory `claudear-{uid}` under the system temp dir
/// with mode 0700 to prevent other users from accessing the socket/PID files.
fn ipc_runtime_dir() -> PathBuf {
    // On Linux, XDG_RUNTIME_DIR is already user-private
    if !cfg!(target_os = "macos") {
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(xdg);
        }
    }

    // Fallback: create a user-scoped subdirectory in the temp dir with restricted permissions
    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!("claudear-{}", uid));
    if !dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            // SECURITY: Falling back to the system temp dir is unsafe because it is
            // world-readable, which could allow other users to access or tamper with
            // the IPC socket. This should be treated as a critical failure.
            tracing::error!("Failed to create IPC runtime dir {:?}: {} — falling back to world-readable temp dir", dir, e);
            return std::env::temp_dir();
        }
        // Set directory permissions to 0700 (owner-only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            if let Err(e) = std::fs::set_permissions(&dir, perms) {
                tracing::warn!("Failed to set IPC runtime dir permissions: {}", e);
            }
        }
    }
    dir
}

/// Default socket path for the IPC server.
pub fn default_socket_path() -> PathBuf {
    ipc_runtime_dir().join("claudear.sock")
}

/// Default PID file path.
pub fn default_pid_path() -> PathBuf {
    ipc_runtime_dir().join("claudear.pid")
}

/// Default path of the file the running `claudear start` daemon holds its [`Lock`] on.
pub fn default_lock_path() -> PathBuf {
    ipc_runtime_dir().join("claudear.lock")
}

/// Check if a watcher daemon is running.
pub fn is_daemon_running() -> bool {
    is_accepting(&default_socket_path())
}

/// Whether a listener is accepting connections on the socket at `socket_path`.
fn is_accepting(socket_path: &Path) -> bool {
    socket_path.exists() && std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

/// Read the PID stored in `pid_path`, if it holds one.
fn read_pid_file(pid_path: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_path).ok()?.trim().parse().ok()
}

/// Write the current process PID to `pid_path`.
fn write_pid_file(pid_path: &Path) -> std::io::Result<()> {
    std::fs::write(pid_path, std::process::id().to_string())
}

/// A temporary directory under `/tmp`, so socket paths inside it stay within the Unix socket
/// path limit however long `TMPDIR` is.
#[cfg(test)]
fn short_temporary_directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("claudear-ipc")
        .tempdir_in("/tmp")
        .expect("create a temporary directory under /tmp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn test_default_socket_path_ends_with_claudear_sock() {
        let path = default_socket_path();
        assert!(
            path.ends_with("claudear.sock"),
            "Expected socket path to end with 'claudear.sock', got: {:?}",
            path
        );
    }

    #[test]
    fn test_default_pid_path_ends_with_claudear_pid() {
        let path = default_pid_path();
        assert!(
            path.ends_with("claudear.pid"),
            "Expected pid path to end with 'claudear.pid', got: {:?}",
            path
        );
    }

    #[test]
    fn test_ipc_runtime_dir_is_absolute() {
        let dir = ipc_runtime_dir();
        assert!(
            dir.is_absolute(),
            "Expected ipc_runtime_dir to return an absolute path, got: {:?}",
            dir
        );
    }

    #[test]
    fn test_read_pid_file_ignores_missing_and_malformed_files() {
        let directory = short_temporary_directory();
        let pid_path = directory.path().join("claudear.pid");
        assert_eq!(read_pid_file(&pid_path), None);

        std::fs::write(&pid_path, "not a pid").unwrap();
        assert_eq!(read_pid_file(&pid_path), None);

        std::fs::write(&pid_path, "42\n").unwrap();
        assert_eq!(read_pid_file(&pid_path), Some(42));
    }

    #[test]
    fn test_write_pid_file_stores_own_pid() {
        let directory = short_temporary_directory();
        let pid_path = directory.path().join("claudear.pid");

        write_pid_file(&pid_path).expect("write_pid_file should succeed");

        assert_eq!(read_pid_file(&pid_path), Some(std::process::id()));
    }

    #[test]
    fn test_is_accepting_only_while_a_listener_is_bound() {
        let directory = short_temporary_directory();
        let socket_path = directory.path().join("claudear.sock");
        assert!(!is_accepting(&socket_path));

        let listener = UnixListener::bind(&socket_path).unwrap();
        assert!(is_accepting(&socket_path));

        drop(listener);
        assert!(socket_path.exists(), "dropping a listener leaves its file");
        assert!(!is_accepting(&socket_path));
    }
}
