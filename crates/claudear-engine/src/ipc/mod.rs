//! Inter-process communication via Unix socket.
//!
//! Enables the CLI to communicate with a running watcher daemon.

mod client;
mod protocol;
mod server;

pub use client::{print_response, IpcClient};
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

/// Check if a watcher daemon is running.
pub fn is_daemon_running() -> bool {
    is_accepting(&default_socket_path())
}

/// Get the PID of the running daemon, if any.
pub fn get_daemon_pid() -> Option<u32> {
    read_pid_file(&default_pid_path())
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

/// Remove the socket and PID files a crashed daemon left behind.
fn cleanup_stale_files(socket_path: &Path, pid_path: &Path) {
    match read_pid_file(pid_path) {
        Some(pid) if is_process_running(pid) => {}
        Some(pid) => {
            tracing::info!("Cleaning up stale files from previous run (PID {})", pid);
            let _ = std::fs::remove_file(pid_path);
            let _ = std::fs::remove_file(socket_path);
        }
        None if socket_path.exists() && !is_accepting(socket_path) => {
            tracing::info!("Cleaning up stale socket file");
            let _ = std::fs::remove_file(socket_path);
        }
        None => {}
    }
}

/// Check if a process with the given PID is running.
fn is_process_running(pid: u32) -> bool {
    // Try to check /proc on Linux
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{}", pid)).exists()
    }

    // On macOS/BSD, use kill(pid, 0) to check if process exists
    #[cfg(target_os = "macos")]
    {
        // SAFETY: kill with signal 0 doesn't actually send a signal,
        // it just checks if the process exists and we have permission to signal it.
        // Returns 0 if process exists, -1 if not (with errno set to ESRCH).
        match i32::try_from(pid) {
            Ok(pid_i32) => unsafe { libc::kill(pid_i32, 0) == 0 },
            Err(_) => false, // PID exceeds i32::MAX, cannot be valid
        }
    }

    // Fallback for other platforms
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid; // Suppress unused warning
                     // Assume running if we can't check
        true
    }
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

    #[test]
    fn test_is_process_running_with_own_pid() {
        // Our own process is guaranteed to be running and we have permission to signal it.
        let own_pid = std::process::id();
        assert!(
            is_process_running(own_pid),
            "Our own PID ({}) should be reported as running",
            own_pid
        );
    }

    #[test]
    fn test_is_process_running_with_invalid_pid() {
        // u32::MAX is extremely unlikely to be a valid PID on any system.
        assert!(
            !is_process_running(u32::MAX),
            "PID u32::MAX should not be reported as running"
        );
    }

    #[test]
    fn test_cleanup_stale_files_removes_files_of_a_dead_daemon() {
        let directory = short_temporary_directory();
        let socket_path = directory.path().join("claudear.sock");
        let pid_path = directory.path().join("claudear.pid");
        drop(UnixListener::bind(&socket_path).unwrap());
        std::fs::write(&pid_path, u32::MAX.to_string()).unwrap();

        cleanup_stale_files(&socket_path, &pid_path);

        assert!(!socket_path.exists(), "stale socket should be removed");
        assert!(!pid_path.exists(), "stale PID file should be removed");
    }

    #[test]
    fn test_cleanup_stale_files_keeps_files_of_a_live_daemon() {
        let directory = short_temporary_directory();
        let socket_path = directory.path().join("claudear.sock");
        let pid_path = directory.path().join("claudear.pid");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        write_pid_file(&pid_path).unwrap();

        cleanup_stale_files(&socket_path, &pid_path);

        assert!(socket_path.exists(), "live socket should be kept");
        assert_eq!(read_pid_file(&pid_path), Some(std::process::id()));
    }

    #[test]
    fn test_cleanup_stale_files_removes_a_socket_without_pid_once_nothing_listens() {
        let directory = short_temporary_directory();
        let socket_path = directory.path().join("claudear.sock");
        let pid_path = directory.path().join("claudear.pid");
        let listener = UnixListener::bind(&socket_path).unwrap();

        cleanup_stale_files(&socket_path, &pid_path);
        assert!(
            socket_path.exists(),
            "a socket that still accepts should be kept"
        );

        drop(listener);
        cleanup_stale_files(&socket_path, &pid_path);
        assert!(
            !socket_path.exists(),
            "a socket nothing listens on should be removed"
        );
    }
}
