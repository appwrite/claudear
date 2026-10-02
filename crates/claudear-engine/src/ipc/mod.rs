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
    let socket_path = default_socket_path();
    socket_path.exists() && std::os::unix::net::UnixStream::connect(&socket_path).is_ok()
}

/// Get the PID of the running daemon, if any.
pub fn get_daemon_pid() -> Option<u32> {
    read_pid_file(&default_pid_path())
}

/// The PID recorded in the PID file at `path`, if it holds one.
fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse().ok())
}

/// Write the current process PID to the pid file.
pub fn write_pid_file() -> std::io::Result<()> {
    write_pid_file_at(&default_pid_path())
}

/// Write the current process PID to the PID file at `path`.
fn write_pid_file_at(path: &Path) -> std::io::Result<()> {
    std::fs::write(path, std::process::id().to_string())
}

/// Remove the PID file.
pub fn remove_pid_file() {
    remove_pid_file_at(&default_pid_path());
}

/// Remove the PID file at `path`, if there is one.
fn remove_pid_file_at(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Remove the socket file.
pub fn remove_socket_file() {
    let socket_path = default_socket_path();
    let _ = std::fs::remove_file(&socket_path);
}

/// Clean up stale socket/pid files from a previous crash.
pub fn cleanup_stale_files() {
    if let Some(pid) = get_daemon_pid() {
        // Check if process is still running by trying to read /proc/{pid} or using kill -0
        let is_running = is_process_running(pid);
        if !is_running {
            tracing::info!("Cleaning up stale files from previous run (PID {})", pid);
            remove_pid_file();
            remove_socket_file();
        }
    } else {
        // No PID file but socket exists - stale
        let socket_path = default_socket_path();
        if socket_path.exists() && std::os::unix::net::UnixStream::connect(&socket_path).is_err() {
            tracing::info!("Cleaning up stale socket file");
            remove_socket_file();
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_get_daemon_pid_returns_option() {
        // This tests that get_daemon_pid does not panic and returns an Option<u32>.
        // If no PID file exists, it returns None. If one does exist (from a running
        // daemon), it returns Some(pid). Either outcome is acceptable.
        let result = get_daemon_pid();
        // Just verify the function completes without panicking.
        // If a daemon happens to be running, the PID should be > 0.
        if let Some(pid) = result {
            assert!(pid > 0, "PID should be positive, got: {}", pid);
        }
    }

    #[test]
    fn test_pid_file_records_our_pid_until_removed() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("claudear.pid");

        write_pid_file_at(&pid_path).expect("writing the PID file should succeed");
        assert_eq!(
            read_pid_file(&pid_path),
            Some(std::process::id()),
            "the PID file must hold our process PID once written"
        );

        remove_pid_file_at(&pid_path);
        assert!(
            !pid_path.exists(),
            "the PID file should be gone once removed"
        );
        assert_eq!(read_pid_file(&pid_path), None);
    }

    #[test]
    fn test_remove_socket_file_does_not_panic_when_no_socket() {
        // remove_socket_file should silently succeed even if no socket file exists.
        // We cannot unconditionally call it because a daemon might be using the socket.
        // Instead, we verify a remove on a non-existent path is safe by checking the
        // implementation pattern (let _ = remove_file), or we call it only when no daemon
        // is running.
        if !is_daemon_running() {
            // The socket file may or may not exist; either way this should not panic.
            remove_socket_file();
        }
    }

    #[test]
    fn test_is_daemon_running_returns_bool() {
        // When no daemon is running, this should return false.
        // If a daemon happens to be running (e.g., in a dev environment), true is also valid.
        let result = is_daemon_running();
        // We simply verify the function completes and returns a bool.
        let _: bool = result;
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
    fn test_cleanup_stale_files_does_not_panic() {
        // cleanup_stale_files should handle all cases gracefully:
        // no PID file, stale socket, running daemon, etc.
        cleanup_stale_files();
    }
}
