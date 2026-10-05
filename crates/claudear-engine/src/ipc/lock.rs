//! The lock that lets only one `claudear start` daemon run at a time.

use claudear_core::error::{Error, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::Path;

const ALREADY_RUNNING: &str =
    "Another claudear daemon is already running. Stop it first with 'claudear stop'";

/// An exclusive lock on a lock file, which one holder at a time can take, across processes.
///
/// Dropping the lock frees it, and so does the kernel as the holder's process exits, before
/// anything reaps that process, so a daemon that keeps its lock holds it exactly while it runs.
///
/// Nothing removes the lock file: a daemon starting after it was removed would lock a new file
/// at the same path while the old one is still held.
#[derive(Debug)]
pub struct Lock {
    file: File,
}

impl Lock {
    /// Take the lock on the file at `path`, creating the file if needed, or fail at once if
    /// another holder has it.
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { file }),
            Err(TryLockError::WouldBlock) => Err(Error::Other(ALREADY_RUNNING.to_string())),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    /// Whether a holder has the lock on the file at `path`. A file that does not exist is not
    /// held; a lock that cannot be checked counts as held, so no daemon passes for stopped
    /// without having exited.
    ///
    /// Checking takes a shared lock for a moment, so checks never mistake each other for a
    /// holder.
    pub fn is_held(path: &Path) -> bool {
        match File::open(path) {
            Ok(file) => file.try_lock_shared().is_err(),
            Err(error) => error.kind() != ErrorKind::NotFound,
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::short_temporary_directory;

    #[test]
    fn test_acquire_refuses_while_another_holder_has_the_lock() {
        let directory = short_temporary_directory();
        let path = directory.path().join("claudear.lock");
        let held = Lock::acquire(&path).expect("take the free lock");

        let error = Lock::acquire(&path).expect_err("a second holder must be refused");
        assert_eq!(error.to_string(), ALREADY_RUNNING);

        drop(held);
        Lock::acquire(&path).expect("take the lock once it is released");
    }

    #[test]
    fn test_is_held_only_while_a_holder_has_the_lock() {
        let directory = short_temporary_directory();
        let path = directory.path().join("claudear.lock");
        assert!(!Lock::is_held(&path), "a missing lock file is not held");

        let held = Lock::acquire(&path).expect("take the free lock");
        assert!(Lock::is_held(&path));

        drop(held);
        assert!(path.exists(), "releasing the lock leaves its file");
        assert!(!Lock::is_held(&path));
        Lock::acquire(&path).expect("checking must not keep the lock");
    }

    #[test]
    fn test_is_held_when_the_lock_cannot_be_checked() {
        let directory = short_temporary_directory();
        let file = directory.path().join("not-a-directory");
        std::fs::write(&file, "").unwrap();

        assert!(
            Lock::is_held(&file.join("claudear.lock")),
            "a lock that cannot be checked must not pass for free"
        );
    }
}
