//! The `.env` file that webhook setup and the GitHub App callback write secrets to.

#[cfg(unix)]
mod mount_table;
#[cfg(unix)]
mod refusal;
#[cfg(unix)]
mod replacement;

use abnegate_config::EnvironmentFile;
use claudear_core::error::{Error, Result};
use std::collections::BTreeMap;
use std::error::Error as StandardError;
use std::fs;
use std::fs::File;
use std::io;
use std::iter;
use std::path::Path;
use tempfile::NamedTempFile;

#[cfg(unix)]
use replacement::Replacement;

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

/// Fails the way [`update`] would, without changing anything.
///
/// The file, if it exists, must be readable. Its directory must let a sibling
/// temporary file be renamed over another file and then be synced, which is
/// how [`update`] replaces it. A file a rename cannot replace is refused: a
/// mount point, such as a single file bind-mounted into a container, or
/// another user's file in a sticky directory. When the directory does not
/// exist yet, the nearest existing ancestor must accept a new entry instead,
/// since [`update`] creates the directory there.
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

    let directory = directory_of(path);
    let rehearsal = if directory.is_dir() {
        rehearse_replacement(path, directory)
    } else {
        NamedTempFile::new_in(nearest_existing_ancestor(directory)).map(drop)
    };

    rehearsal
        .map_err(|error| Error::config(format!("Failed to write '{}': {error}", path.display())))
}

fn rehearse_replacement(destination: &Path, directory: &Path) -> io::Result<()> {
    let temporary = NamedTempFile::new_in(directory)?;
    let scratch = NamedTempFile::new_in(directory)?.into_temp_path();

    refuse_unreplaceable(destination, directory, temporary.as_file())?;
    temporary.persist(&scratch)?;

    sync(directory)
}

#[cfg(unix)]
fn refuse_unreplaceable(destination: &Path, directory: &Path, created: &File) -> io::Result<()> {
    match Replacement::inspect(destination, directory, created)?
        .and_then(|replacement| replacement.refusal())
    {
        Some(refusal) => Err(io::Error::other(refusal.to_string())),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn refuse_unreplaceable(_destination: &Path, _directory: &Path, _created: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync(_directory: &Path) -> io::Result<()> {
    Ok(())
}

fn directory_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn nearest_existing_ancestor(directory: &Path) -> &Path {
    directory
        .ancestors()
        .find(|ancestor| !ancestor.as_os_str().is_empty() && ancestor.exists())
        .unwrap_or(Path::new("."))
}

fn describe(error: &(dyn StandardError + 'static)) -> String {
    iter::successors(Some(error), |&error| error.source())
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join(": ")
}
