//! Values and stores the API tests share, generated so no credential is a literal.

use claudear_storage::{FixAttemptTracker, SqliteTracker};
use std::path::Path;
use std::sync::Arc;

const PASSWORD_LENGTH: usize = 24;

/// How storage reports a handle another person already holds.
const DUPLICATE_HANDLE: &str = "UNIQUE constraint failed: user_handles.field, user_handles.value";

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

/// A file-backed tracker in `directory` whose every identity write fails the way storage
/// reports a handle another person holds: what a writer in another process causes after
/// the API's up-front registry check passed.
pub(crate) fn tracker_refusing_handle_claims(directory: &Path) -> Arc<dyn FixAttemptTracker> {
    let path = directory.join("claudear.db");
    let tracker = SqliteTracker::new(&path).expect("file tracker");
    rusqlite::Connection::open(&path)
        .expect("open database")
        .execute_batch(&format!(
            "CREATE TRIGGER refuse_handle_claims BEFORE INSERT ON user_identities
             BEGIN SELECT RAISE(ABORT, '{DUPLICATE_HANDLE}'); END;"
        ))
        .expect("create trigger");
    Arc::new(tracker)
}
