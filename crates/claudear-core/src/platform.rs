//! Platform abstraction layer.
//!
//! Centralises all OS-specific logic so that the rest of the codebase can stay
//! platform-agnostic.  Every function is a no-op or a sensible default on
//! platforms where the underlying primitive does not exist.

use std::path::Path;

// ---------------------------------------------------------------------------
// File permissions
// ---------------------------------------------------------------------------

/// Set restrictive **file** permissions (Unix `0o600` — owner read/write only).
///
/// On Windows this is a no-op; NTFS ACLs are inherited from the parent
/// directory and are typically sufficient for single-user machines.
pub fn set_file_permissions_secure(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Set restrictive **directory** permissions (Unix `0o700` — owner only).
///
/// On Windows this is a no-op.
pub fn set_dir_permissions_secure(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Process detection
// ---------------------------------------------------------------------------

/// Check whether a process with the given PID is still alive.
pub fn is_process_running(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{}", pid)).exists()
    }

    #[cfg(target_os = "macos")]
    {
        // SAFETY: kill with signal 0 doesn't actually send a signal,
        // it just checks if the process exists and we have permission to signal it.
        match i32::try_from(pid) {
            Ok(pid_i32) => unsafe { libc::kill(pid_i32, 0) == 0 },
            Err(_) => false,
        }
    }

    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: OpenProcess with PROCESS_QUERY_LIMITED_INFORMATION is a
        // read-only check.  We immediately close the handle afterwards.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !handle.is_null() {
                CloseHandle(handle);
                true
            } else {
                false
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = pid;
        true // assume running when we cannot check
    }
}

// ---------------------------------------------------------------------------
// Command / binary detection
// ---------------------------------------------------------------------------

/// Check whether a command exists on the system PATH.
///
/// Uses `which` on Unix and `where` on Windows.
pub fn command_exists(binary: &str) -> bool {
    #[cfg(not(windows))]
    let cmd = "which";
    #[cfg(windows)]
    let cmd = "where";

    std::process::Command::new(cmd)
        .arg(binary)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_process_running_own_pid() {
        assert!(is_process_running(std::process::id()));
    }

    #[test]
    fn test_is_process_running_invalid_pid() {
        assert!(!is_process_running(u32::MAX));
    }

    #[test]
    fn test_command_exists_known_binary() {
        #[cfg(not(windows))]
        assert!(command_exists("ls"));
        #[cfg(windows)]
        assert!(command_exists("cmd"));
    }

    #[test]
    fn test_command_exists_nonexistent() {
        assert!(!command_exists("__nonexistent_binary_12345__"));
    }

    /// The outcome that matters is that a secret written through these helpers
    /// is not readable by anyone else, however the platform expresses that.
    #[test]
    fn a_secured_file_is_not_readable_by_other_users() {
        let dir = tempfile::tempdir().expect("temp dir");
        let secret = dir.path().join("token");
        std::fs::write(&secret, "s3cret").expect("write");

        set_file_permissions_secure(&secret).expect("secure the file");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&secret)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o077,
                0,
                "group and other must hold no bits, got {:o}",
                mode
            );
        }

        assert_eq!(std::fs::read_to_string(&secret).expect("read"), "s3cret");
    }

    #[test]
    fn a_secured_directory_is_not_traversable_by_other_users() {
        let parent = tempfile::tempdir().expect("temp dir");
        let dir = parent.path().join("private");
        std::fs::create_dir(&dir).expect("create");

        set_dir_permissions_secure(&dir).expect("secure the dir");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o077,
                0,
                "group and other must hold no bits, got {:o}",
                mode
            );
        }

        std::fs::write(dir.join("inside"), "ok").expect("the owner can still write");
    }
}
