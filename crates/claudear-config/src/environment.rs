//! The `.env` file that webhook setup and the GitHub App callback write secrets to.

use abnegate_config::EnvironmentFile;
use claudear_core::error::{Error, Result};
use std::collections::BTreeMap;
use std::error::Error as StandardError;
use std::fs;
use std::io;
use std::iter;
use std::path::Path;
use tempfile::NamedTempFile;

pub const DISCORD_WEBHOOK_URL: &str = "DISCORD_WEBHOOK_URL";
pub const GITHUB_APP_BASE_URL: &str = "GITHUB_APP_BASE_URL";
pub const GITHUB_APP_CLIENT_ID: &str = "GITHUB_APP_CLIENT_ID";
pub const GITHUB_APP_CLIENT_SECRET: &str = "GITHUB_APP_CLIENT_SECRET";
pub const GITHUB_APP_ID: &str = "GITHUB_APP_ID";
pub const GITHUB_APP_PRIVATE_KEY_PATH: &str = "GITHUB_APP_PRIVATE_KEY_PATH";
pub const GITHUB_APP_WEBHOOK_SECRET: &str = "GITHUB_APP_WEBHOOK_SECRET";
pub const GITHUB_WEBHOOK_SECRET: &str = "GITHUB_WEBHOOK_SECRET";
pub const GITLAB_WEBHOOK_SECRET: &str = "GITLAB_WEBHOOK_SECRET";
pub const LINEAR_WEBHOOK_SECRET: &str = "LINEAR_WEBHOOK_SECRET";
pub const SENTRY_CLIENT_SECRET: &str = "SENTRY_CLIENT_SECRET";
pub const TELEGRAM_WEBHOOK_SECRET: &str = "TELEGRAM_WEBHOOK_SECRET";
pub const WHATSAPP_WEBHOOK_VERIFY_TOKEN: &str = "WHATSAPP_WEBHOOK_VERIFY_TOKEN";

/// Every key claudear writes to the `.env` file.
pub const KEYS: &[&str] = &[
    DISCORD_WEBHOOK_URL,
    GITHUB_APP_BASE_URL,
    GITHUB_APP_CLIENT_ID,
    GITHUB_APP_CLIENT_SECRET,
    GITHUB_APP_ID,
    GITHUB_APP_PRIVATE_KEY_PATH,
    GITHUB_APP_WEBHOOK_SECRET,
    GITHUB_WEBHOOK_SECRET,
    GITLAB_WEBHOOK_SECRET,
    LINEAR_WEBHOOK_SECRET,
    SENTRY_CLIENT_SECRET,
    TELEGRAM_WEBHOOK_SECRET,
    WHATSAPP_WEBHOOK_VERIFY_TOKEN,
];

/// Sets every key in `values` in the `.env` file at `path`, keeping its other
/// lines, and replaces the file atomically as readable only by its owner.
///
/// The replacement is renamed into place, so the parent directory must be
/// writable. Failures become [`Error::Config`] carrying the full cause chain.
pub fn update(path: &Path, values: &BTreeMap<String, String>) -> Result<()> {
    EnvironmentFile::new(path)
        .update(values)
        .map_err(|error| Error::config(describe(&error)))
}

/// Fails the way [`update`] would, without changing anything: the file, if it
/// exists, must be readable, and the directory its replacement is renamed from
/// must accept a new file. When that directory does not exist yet, the nearest
/// existing ancestor is checked instead, since [`update`] creates it there.
pub fn probe(path: &Path) -> Result<()> {
    match fs::read_to_string(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(Error::config(format!(
                "Failed to read '{}': {error}",
                path.display()
            )));
        }
    }

    NamedTempFile::new_in(nearest_existing_directory(path))
        .map(drop)
        .map_err(|error| Error::config(format!("Failed to write '{}': {error}", path.display())))
}

fn nearest_existing_directory(path: &Path) -> &Path {
    let current = Path::new(".");
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(current);

    parent
        .ancestors()
        .find(|ancestor| !ancestor.as_os_str().is_empty() && ancestor.exists())
        .unwrap_or(current)
}

fn describe(error: &(dyn StandardError + 'static)) -> String {
    iter::successors(Some(error), |&error| error.source())
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join(": ")
}
