//! Values and stores the API tests share, generated so no credential is a literal.

use claudear_core::identity::IdentityField;
use claudear_storage::{FixAttemptTracker, SqliteTracker};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

const PASSWORD_LENGTH: usize = 24;

/// A fresh random password the API accepts (8-72 bytes).
pub(crate) fn password() -> String {
    password_with_length(PASSWORD_LENGTH)
}

/// A fresh random lowercase password of exactly `length` bytes.
pub(crate) fn password_with_length(length: usize) -> String {
    (0..length)
        .map(|_| char::from(b'a' + rand::random::<u8>() % 26))
        .collect()
}

/// One file-backed database that several processes (the daemon and a standalone
/// dashboard) open, each through its own tracker.
pub(crate) struct SharedDatabase {
    _directory: TempDir,
    path: PathBuf,
}

impl SharedDatabase {
    pub(crate) fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("claudear.db");
        Self {
            _directory: directory,
            path,
        }
    }

    /// A tracker on its own connection, as another process would open the database.
    pub(crate) fn tracker(&self) -> Arc<dyn FixAttemptTracker> {
        Arc::new(SqliteTracker::new(&self.path).expect("file tracker"))
    }

    /// Takes the database write lock until the returned connection commits or drops, so
    /// writers from every tracker wait (up to the busy timeout) after their reads.
    pub(crate) fn hold_writes(&self) -> rusqlite::Connection {
        let connection = rusqlite::Connection::open(&self.path).expect("open database");
        connection
            .execute_batch("BEGIN IMMEDIATE")
            .expect("take the write lock");
        connection
    }

    /// Records `user_id` as the holder of a handle without touching the identities any
    /// registry loads: a claim another process committed after this one's registry reload.
    pub(crate) fn claim_handle(&self, user_id: i64, field: IdentityField, value: &str) {
        rusqlite::Connection::open(&self.path)
            .expect("open database")
            .execute(
                "INSERT INTO user_handles (user_id, field, value) VALUES (?1, ?2, ?3)",
                rusqlite::params![user_id, field.as_str(), field.comparison_key(value)],
            )
            .expect("claim handle");
    }
}
