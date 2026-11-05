//! Reading, redacting, validating and safely writing `claudear.toml` for the
//! dashboard config editor.
//!
//! Secrets are found by type: every [`SecretValue`](claudear_core::secret::SecretValue)
//! serializes as [`REDACTED`] inside [`with_redaction`], plain-string secrets
//! are listed in [`PLAIN_SECRET_PATHS`], and every string under a table named
//! in [`SECRET_TABLE_NAMES`] is treated as a secret. A kept secret is only
//! restored while the endpoint it is sent to (a sibling `*url` or `*host`)
//! is unchanged, so a masked credential cannot be redirected to a new host.
//!
//! TOML errors never reach a response or a log through their `Display`, which
//! quotes the offending source line: [`describe_toml_error`] reports the
//! message with any echoed value removed, and the line number.
//!
//! Every read and write goes through a [`ConfigFile`]: the path claudear was
//! started with, re-resolved on each access so a re-pointed symlink (or a
//! Kubernetes ConfigMap's `..data` swap) is followed. The resolved file must
//! be a regular file, or a missing one, inside one of the directories fixed
//! at startup: the configured path's own directory, and the directory its
//! symlink pointed into then. A request can never steer a read or write to
//! another file, and a symlink later re-pointed into any other directory is
//! refused until claudear is restarted with it.

use axum::{
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use claudear_config::config::{
    Config, DiscordKnowledgebaseConfig, DiscordSourceConfig, GitHubCodeScanningConfig,
    GitLabConfig, HelpScoutConfig, JiraConfig, LinearConfig, SentryConfig, SlackSourceConfig,
};
use claudear_core::secret::{with_redaction, REDACTED};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use toml::{Table, Value};

const PLAIN_SECRET_PATHS: &[&str] = &[
    "regression.github_token",
    "notifiers.sms.account_sid",
    "notifiers.push.user_key",
    "users.*.push_user_key",
];

const SECRET_TABLE_NAMES: &[&str] = &["env", "headers", "extra"];

const MAP_PATHS: &[&str] = &[
    "agent.providers",
    "agent.providers.*.env",
    "agent.providers.*.extra",
    "agent.providers.*.mcp",
    "agent.providers.*.mcp.*.env",
    "agent.providers.*.mcp.*.headers",
    "notifiers.helpscout.templates",
    "regression.package_names",
    "users",
];

const WILDCARD: &str = "*";

const ENDPOINT_SUFFIXES: &[&str] = &["url", "host"];

const QUOTES: &[char] = &['"', '\'', '`'];

const EXPECTED_CLAUSE: &str = "expected";

const OMITTED_VALUE: &str = "...";

const SIBLING_PREFIX: &str = ".claudear-config-";

const VALIDATION_PREFIX: &str = "claudear-config-validation-";

const READ_ONLY_MESSAGE: &str = "claudear.toml is read-only (for example a read-only bind mount); \
     edit it on the host and restart claudear";

const REVISION_MISMATCH_MESSAGE: &str =
    "claudear.toml changed on disk since it was loaded; reload and reapply";

static WRITE_LOCK: Mutex<()> = Mutex::new(());

const OUTSIDE_DIRECTORY_MESSAGE: &str =
    "claudear.toml now resolves outside its configured directories; restart claudear";

const NOT_A_FILE_MESSAGE: &str = "claudear.toml is not a regular file";

const UNNAMED_MESSAGE: &str = "claudear.toml has no usable file name";

/// The config file claudear was started with: the path as given (absolute,
/// symlinks unresolved) and the canonical directories every access must
/// resolve inside.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    path: PathBuf,
    directories: Vec<PathBuf>,
}

impl ConfigFile {
    /// Keep `path` as given, made absolute, and fix the directories it may
    /// resolve into: its own, and the one its symlink points into now.
    pub fn new(path: &Path) -> Self {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let mut directories = Vec::new();
        if let Ok(directory) = fs::canonicalize(parent_directory(&path)) {
            directories.push(directory);
        }
        if let Ok(target) = fs::canonicalize(&path) {
            let directory = parent_directory(&target).to_path_buf();
            if !directories.contains(&directory) {
                directories.push(directory);
            }
        }
        Self { path, directories }
    }

    /// The file's canonical path now, if it is a regular file, or a missing
    /// one, inside one of its directories.
    fn resolve(&self) -> io::Result<PathBuf> {
        let resolved = match fs::canonicalize(&self.path) {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = self
                    .path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, UNNAMED_MESSAGE))?;
                if name.contains("..") {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, UNNAMED_MESSAGE));
                }
                fs::canonicalize(parent_directory(&self.path))?.join(name)
            }
            Err(error) => return Err(error),
        };
        for directory in &self.directories {
            if resolved.starts_with(directory) {
                return regular_or_missing(resolved);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            OUTSIDE_DIRECTORY_MESSAGE,
        ))
    }

    /// The file's bytes.
    pub fn read(&self) -> io::Result<Vec<u8>> {
        fs::read(self.resolve()?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ConfigErrorCode {
    Invalid,
    ReadOnly,
    RevisionMismatch,
    Unreadable,
}

impl ConfigErrorCode {
    fn status(self) -> StatusCode {
        match self {
            Self::Invalid => StatusCode::BAD_REQUEST,
            Self::ReadOnly | Self::RevisionMismatch => StatusCode::CONFLICT,
            Self::Unreadable => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug)]
pub(super) struct ConfigFileError {
    pub status: StatusCode,
    pub code: ConfigErrorCode,
    pub message: String,
}

impl ConfigFileError {
    pub(super) fn new(code: ConfigErrorCode, message: impl Into<String>) -> Self {
        Self {
            status: code.status(),
            code,
            message: message.into(),
        }
    }

    pub(super) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ConfigErrorCode::Invalid, message)
    }

    pub(super) fn unreadable(message: impl Into<String>) -> Self {
        Self::new(ConfigErrorCode::Unreadable, message)
    }

    pub(super) fn rejected(rejection: &JsonRejection) -> Self {
        Self::invalid(scrub_values(&rejection.body_text()))
    }

    fn read_only() -> Self {
        Self::new(ConfigErrorCode::ReadOnly, READ_ONLY_MESSAGE)
    }
}

#[derive(Serialize)]
struct ConfigErrorBody<'a> {
    error: &'a str,
    code: ConfigErrorCode,
}

impl IntoResponse for ConfigFileError {
    fn into_response(self) -> Response {
        let body = ConfigErrorBody {
            error: &self.message,
            code: self.code,
        };
        (self.status, Json(body)).into_response()
    }
}

#[derive(Debug)]
pub(super) struct RedactedDocument {
    pub content: String,
    pub secret_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OptionalSection {
    pub key: String,
    pub template: String,
}

#[derive(Debug)]
pub(super) struct WriteOutcome {
    pub revision: String,
    pub normalized: Value,
}

/// `error`'s message with every value it echoes removed, and the line of
/// `source` it points at. Use this instead of `error`'s `Display`, which
/// quotes the source line and so any secret on it.
pub(super) fn describe_toml_error(error: &toml::de::Error, source: &str) -> String {
    let message = scrub_values(error.message());
    match error.span() {
        Some(span) => format!("{message} (line {})", line_of(source, span.start)),
        None => message,
    }
}

/// Render `config` as TOML in struct order with every secret replaced by
/// [`REDACTED`], except unset secrets, which stay empty.
pub(super) fn redact(config: &Config) -> Result<RedactedDocument, ConfigFileError> {
    let plain = to_value(config)?;
    let mut masked = with_redaction(|| to_value(config))?;
    for path in PLAIN_SECRET_PATHS {
        visit_path(&mut masked, &segments(path), &mut mask_strings);
    }
    mask_secret_tables(&mut masked);

    let mut secret_paths = Vec::new();
    mark_secrets(&plain, &mut masked, "", &mut secret_paths);

    let masked: Config = masked.try_into().map_err(|error| {
        ConfigFileError::unreadable(format!(
            "Failed to redact config: {}",
            describe_toml_error(&error, "")
        ))
    })?;

    Ok(RedactedDocument {
        content: render(&masked)?,
        secret_paths,
    })
}

/// Replace every [`REDACTED`] sentinel in `incoming` with the value at the same
/// path in `current`. Arrays are restored element by element when both sides
/// have the same length. A sentinel is refused when an endpoint in its scope
/// changed: a `*url` or `*host` key of its own table, or of the parent of the
/// `env`/`headers`/`extra` table it sits in.
fn restore(incoming: &mut Value, current: &Value) -> Result<(), ConfigFileError> {
    restore_value(incoming, current, "", None)
}

/// `incoming` with its sentinels restored from `current`, refusing any it
/// cannot restore. `incoming` comes from [`redact`], which writes every default
/// out, so it is compared with `current` in that same form; `current` is used
/// as written only when it does not deserialize into a [`Config`].
pub(super) fn restore_all(mut incoming: Value, current: &Value) -> Result<Value, ConfigFileError> {
    let normalized = normalize_document(current);
    restore(&mut incoming, normalized.as_ref().unwrap_or(current))?;
    let placeholders = unrestored(&incoming);
    if !placeholders.is_empty() {
        return Err(ConfigFileError::invalid(format!(
            "Config still contains redacted placeholders for: {}. \
             Provide the actual value(s) for these fields.",
            placeholders.join(", ")
        )));
    }
    Ok(incoming)
}

/// Paths that still hold the [`REDACTED`] sentinel, as `a.b` or `a.list[0].c`.
fn unrestored(document: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    collect_sentinels(document, "", &mut paths);
    paths
}

pub(super) fn parse(content: &str) -> Result<Config, ConfigFileError> {
    deserialize(content)
}

/// A TOML document from client input, for merging into the on-disk one.
pub(super) fn parse_document(content: &str) -> Result<Value, ConfigFileError> {
    deserialize(content)
}

fn deserialize<T: DeserializeOwned>(content: &str) -> Result<T, ConfigFileError> {
    toml::from_str(content).map_err(|error| {
        ConfigFileError::invalid(format!(
            "Invalid TOML config: {}",
            describe_toml_error(&error, content)
        ))
    })
}

/// The config as the daemon would deserialize it, without env overrides.
pub(super) fn normalize(content: &str) -> Result<Value, ConfigFileError> {
    to_value(&parse(content)?)
}

fn normalize_document(document: &Value) -> Option<Value> {
    let config: Config = document.clone().try_into().ok()?;
    to_value(&config).ok()
}

pub(super) fn revision(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn regular_or_missing(path: PathBuf) -> io::Result<PathBuf> {
    match fs::metadata(&path) {
        Ok(metadata) if !metadata.is_file() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            NOT_A_FILE_MESSAGE,
        )),
        _ => Ok(path),
    }
}

/// The file's bytes, read while no write is in progress.
pub(super) async fn read_locked(file: &ConfigFile) -> io::Result<Vec<u8>> {
    let file = file.clone();
    tokio::task::spawn_blocking(move || {
        let _lock = lock_writes();
        file.read()
    })
    .await
    .map_err(io::Error::other)?
}

/// Whether the config could be saved, checked without opening the file for
/// writing (a write-mode open fires change watchers on close).
pub(super) fn writable(file: &ConfigFile) -> bool {
    file.resolve().is_ok_and(|path| is_writable(&path))
}

fn is_writable(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(_) => can_write(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            can_create_in(parent_directory(path))
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn can_write(path: &Path) -> bool {
    has_access(path, libc::W_OK)
}

#[cfg(unix)]
fn can_create_in(directory: &Path) -> bool {
    has_access(directory, libc::W_OK | libc::X_OK)
}

#[cfg(unix)]
fn has_access(path: &Path, mode: libc::c_int) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    unsafe { libc::access(path.as_ptr(), mode) == 0 }
}

#[cfg(not(unix))]
fn can_write(path: &Path) -> bool {
    fs::OpenOptions::new().write(true).open(path).is_ok()
}

#[cfg(not(unix))]
fn can_create_in(directory: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(SIBLING_PREFIX)
        .tempfile_in(directory)
        .is_ok()
}

/// Paths whose values the daemon would see differently from the file:
/// env overrides, resolved instructions and user slugs. Never values.
pub(super) fn runtime_overrides(config_file: &ConfigFile, disk: &Config) -> Vec<String> {
    let Ok(file) = toml::Value::try_from(disk) else {
        return Vec::new();
    };
    let Ok(path) = config_file.resolve() else {
        return Vec::new();
    };
    let Some(loaded) = Config::load(&path)
        .ok()
        .and_then(|loaded| toml::Value::try_from(&loaded).ok())
    else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    collect_differences(Some(&file), Some(&loaded), "", &mut paths);
    paths
}

/// Templates for the optional sections `document` does not define yet.
pub(super) fn optional_sections(document: &Value) -> Vec<OptionalSection> {
    section_templates()
        .iter()
        .filter(|section| lookup(document, &section.key).is_none())
        .cloned()
        .collect()
}

/// Under the write lock, check `expected_revision` against the file, build the
/// new document with `merge` from exactly the checked on-disk document,
/// validate it as daemon startup would (env overrides, instructions files,
/// user slugs), then write it without any of them.
pub(super) async fn write_validated<Merge>(
    file: &ConfigFile,
    expected_revision: Option<&str>,
    merge: Merge,
) -> Result<WriteOutcome, ConfigFileError>
where
    Merge: FnOnce(Value) -> Result<Value, ConfigFileError> + Send + 'static,
{
    let file = file.clone();
    let expected_revision = expected_revision.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        write_validated_blocking(&file, expected_revision.as_deref(), merge)
    })
    .await
    .map_err(|error| ConfigFileError::unreadable(format!("Config write failed: {error}")))?
}

fn write_validated_blocking(
    file: &ConfigFile,
    expected_revision: Option<&str>,
    merge: impl FnOnce(Value) -> Result<Value, ConfigFileError>,
) -> Result<WriteOutcome, ConfigFileError> {
    let _lock = lock_writes();
    let path = file.resolve().map_err(|error| {
        ConfigFileError::unreadable(format!("Failed to read on-disk config: {error}"))
    })?;

    let current = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(ConfigFileError::unreadable(format!(
                "Failed to read on-disk config: {error}"
            )))
        }
    };
    if expected_revision.is_some_and(|expected| revision(&current) != expected) {
        return Err(ConfigFileError::new(
            ConfigErrorCode::RevisionMismatch,
            REVISION_MISMATCH_MESSAGE,
        ));
    }

    let document = merge(disk_document(&current)?)?;
    let parsed: Config = document.try_into().map_err(|error| {
        ConfigFileError::invalid(format!(
            "Invalid TOML config: {}",
            describe_toml_error(&error, "")
        ))
    })?;
    let content = render(&parsed)?;

    validate_as_startup(&path, &parsed)?;

    if !is_writable(&path) {
        return Err(ConfigFileError::read_only());
    }
    persist(&path, &content)?;

    Ok(WriteOutcome {
        revision: revision(content.as_bytes()),
        normalized: normalize(&content)?,
    })
}

fn lock_writes() -> MutexGuard<'static, ()> {
    WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn disk_document(bytes: &[u8]) -> Result<Value, ConfigFileError> {
    let content = std::str::from_utf8(bytes).map_err(|error| {
        ConfigFileError::unreadable(format!("Failed to read on-disk config: {error}"))
    })?;
    toml::from_str(content).map_err(|error| {
        ConfigFileError::unreadable(format!(
            "Invalid on-disk config: {}",
            describe_toml_error(&error, content)
        ))
    })
}

fn validate_as_startup(path: &Path, parsed: &Config) -> Result<(), ConfigFileError> {
    let directory = parent_directory(path);
    let directory = std::path::absolute(directory).unwrap_or_else(|_| directory.to_path_buf());
    let mut candidate = parsed.clone();
    for provider in candidate.agent.providers.values_mut() {
        if let Some(file) = provider.instructions_file.as_mut() {
            if Path::new(file.as_str()).is_relative() {
                *file = directory.join(file.as_str()).to_string_lossy().into_owned();
            }
        }
    }
    let content = render(&candidate)?;
    parse(&content)?;

    let mut temporary = tempfile::Builder::new()
        .prefix(VALIDATION_PREFIX)
        .suffix(".toml")
        .tempfile()
        .map_err(|error| {
            ConfigFileError::unreadable(format!("Failed to stage config for validation: {error}"))
        })?;
    temporary
        .write_all(content.as_bytes())
        .and_then(|()| temporary.flush())
        .map_err(|error| {
            ConfigFileError::unreadable(format!("Failed to stage config for validation: {error}"))
        })?;

    Config::load(temporary.path())
        .and_then(|loaded| loaded.validate())
        .map_err(|error| ConfigFileError::invalid(format!("Config validation failed: {error}")))
}

fn persist(path: &Path, content: &str) -> Result<(), ConfigFileError> {
    match replace_through_sibling(path, content) {
        Ok(()) => Ok(()),
        Err(error) if allows_in_place_write(&error) => overwrite_in_place(path, content),
        Err(error) => Err(ConfigFileError::unreadable(format!(
            "Failed to write config: {error}"
        ))),
    }
}

fn replace_through_sibling(path: &Path, content: &str) -> io::Result<()> {
    let mut temporary = tempfile::Builder::new()
        .prefix(SIBLING_PREFIX)
        .tempfile_in(parent_directory(path))?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().sync_all()?;
    match fs::metadata(path) {
        Ok(metadata) => {
            keep_owner(temporary.as_file(), &metadata);
            fs::set_permissions(temporary.path(), metadata.permissions())?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(unix)]
fn keep_owner(file: &fs::File, original: &fs::Metadata) {
    use std::os::unix::fs::MetadataExt;
    if let Err(error) = std::os::unix::fs::fchown(file, Some(original.uid()), Some(original.gid()))
    {
        tracing::debug!(%error, "Could not keep the owner of claudear.toml");
    }
}

#[cfg(not(unix))]
fn keep_owner(_file: &fs::File, _original: &fs::Metadata) {}

fn overwrite_in_place(path: &Path, content: &str) -> Result<(), ConfigFileError> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|error| {
            if is_read_only(&error) {
                ConfigFileError::read_only()
            } else {
                ConfigFileError::unreadable(format!("Failed to write config: {error}"))
            }
        })?;
    file.write_all(content.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| ConfigFileError::unreadable(format!("Failed to write config: {error}")))
}

fn allows_in_place_write(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied
            | io::ErrorKind::ReadOnlyFilesystem
            | io::ErrorKind::CrossesDevices
            | io::ErrorKind::ResourceBusy
    ) || raw_error_in(error, &IN_PLACE_RAW_ERRORS)
}

fn is_read_only(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
    ) || raw_error_in(error, &READ_ONLY_RAW_ERRORS)
}

#[cfg(unix)]
const IN_PLACE_RAW_ERRORS: [i32; 4] = [libc::EACCES, libc::EROFS, libc::EXDEV, libc::EBUSY];

#[cfg(unix)]
const READ_ONLY_RAW_ERRORS: [i32; 2] = [libc::EACCES, libc::EROFS];

#[cfg(not(unix))]
const IN_PLACE_RAW_ERRORS: [i32; 0] = [];

#[cfg(not(unix))]
const READ_ONLY_RAW_ERRORS: [i32; 0] = [];

fn raw_error_in(error: &io::Error, codes: &[i32]) -> bool {
    error
        .raw_os_error()
        .is_some_and(|code| codes.contains(&code))
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn to_value(config: &Config) -> Result<Value, ConfigFileError> {
    Value::try_from(config).map_err(serialize_error)
}

/// `config` as TOML in struct order, with the keys of every map-typed table
/// sorted so the same config always renders to the same bytes.
fn render(config: &Config) -> Result<String, ConfigFileError> {
    let text = toml::to_string_pretty(config).map_err(serialize_error)?;
    let mut document: Value = toml::from_str(&text).map_err(|error| {
        ConfigFileError::unreadable(format!(
            "Failed to serialize config: {}",
            describe_toml_error(&error, &text)
        ))
    })?;
    for path in MAP_PATHS {
        visit_path(&mut document, &segments(path), &mut sort_keys);
    }
    toml::to_string_pretty(&document).map_err(serialize_error)
}

fn serialize_error(error: toml::ser::Error) -> ConfigFileError {
    ConfigFileError::unreadable(format!(
        "Failed to serialize config: {}",
        scrub_values(&error.to_string())
    ))
}

fn sort_keys(value: &mut Value) {
    if let Value::Table(table) = value {
        let mut entries: Vec<(String, Value)> = std::mem::take(table).into_iter().collect();
        entries.sort_by(|(left, _), (right, _)| left.cmp(right));
        table.extend(entries);
    }
}

fn segments(path: &str) -> Vec<&str> {
    path.split('.').collect()
}

fn visit_path(value: &mut Value, segments: &[&str], action: &mut impl FnMut(&mut Value)) {
    let Some((first, rest)) = segments.split_first() else {
        action(value);
        return;
    };
    let Some(table) = value.as_table_mut() else {
        return;
    };
    if *first == WILDCARD {
        for (_, child) in table.iter_mut() {
            visit_path(child, rest, action);
        }
    } else if let Some(child) = table.get_mut(*first) {
        visit_path(child, rest, action);
    }
}

/// `message` with the content of every quoted literal before its `expected`
/// clause replaced: serde puts the offending value there (`string "..."`,
/// ``integer `...` ``, ``unknown variant `...` ``), and the expected clause
/// only lists what the type accepts.
fn scrub_values(message: &str) -> String {
    let mut scrubbed = String::with_capacity(message.len());
    let mut rest = message;
    while let Some((start, quote)) = rest.char_indices().find(|(_, c)| QUOTES.contains(c)) {
        if rest[..start].contains(EXPECTED_CLAUSE) {
            break;
        }
        let opened = start + quote.len_utf8();
        scrubbed.push_str(&rest[..opened]);
        scrubbed.push_str(OMITTED_VALUE);
        match closing_quote(&rest[opened..], quote) {
            Some(length) => {
                scrubbed.push(quote);
                rest = &rest[opened + length + quote.len_utf8()..];
            }
            None => return scrubbed,
        }
    }
    scrubbed.push_str(rest);
    scrubbed
}

fn closing_quote(text: &str, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        match character {
            '\\' if quote == '"' && !escaped => escaped = true,
            character if character == quote && !escaped => return Some(index),
            _ => escaped = false,
        }
    }
    None
}

fn line_of(source: &str, offset: usize) -> usize {
    let preceding = &source.as_bytes()[..offset.min(source.len())];
    preceding.iter().filter(|byte| **byte == b'\n').count() + 1
}

fn child_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else {
        format!("{parent}.{key}")
    }
}

fn lookup<'a>(document: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(document, |value, segment| value.as_table()?.get(segment))
}

fn mask_strings(value: &mut Value) {
    match value {
        Value::String(text) => *text = REDACTED.to_string(),
        Value::Array(items) => items.iter_mut().for_each(mask_strings),
        Value::Table(table) => table.iter_mut().for_each(|(_, child)| mask_strings(child)),
        _ => {}
    }
}

fn mask_secret_tables(value: &mut Value) {
    match value {
        Value::Table(table) => {
            for (key, child) in table.iter_mut() {
                if is_secret_table(key) {
                    mask_strings(child);
                } else {
                    mask_secret_tables(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask_secret_tables),
        _ => {}
    }
}

fn is_secret_table(key: &str) -> bool {
    SECRET_TABLE_NAMES.contains(&key)
}

fn is_sentinel(value: &Value) -> bool {
    value.as_str() == Some(REDACTED)
}

fn is_endpoint(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ENDPOINT_SUFFIXES.iter().any(|suffix| key.ends_with(suffix))
}

fn restore_value(
    incoming: &mut Value,
    current: &Value,
    path: &str,
    changed_endpoint: Option<&str>,
) -> Result<(), ConfigFileError> {
    match (incoming, current) {
        (Value::Table(incoming), Value::Table(current)) => {
            restore_table(incoming, current, path, changed_endpoint)
        }
        (Value::Array(incoming), Value::Array(current)) if incoming.len() == current.len() => {
            for (index, (value, current)) in incoming.iter_mut().zip(current).enumerate() {
                let scope = if value.is_table() {
                    None
                } else {
                    changed_endpoint
                };
                restore_value(value, current, &format!("{path}[{index}]"), scope)?;
            }
            Ok(())
        }
        (incoming, current) if is_sentinel(incoming) => match changed_endpoint {
            Some(endpoint) => Err(ConfigFileError::invalid(format!(
                "Re-enter {path} when changing {endpoint}"
            ))),
            None => {
                *incoming = current.clone();
                Ok(())
            }
        },
        _ => Ok(()),
    }
}

fn restore_table(
    incoming: &mut Table,
    current: &Table,
    path: &str,
    inherited_endpoint: Option<&str>,
) -> Result<(), ConfigFileError> {
    let changed = changed_endpoint(incoming, current, path);
    let endpoint = changed.as_deref().or(inherited_endpoint);
    for (key, value) in incoming.iter_mut() {
        let Some(current) = current.get(key) else {
            continue;
        };
        let scope = match value {
            Value::Table(_) if !is_secret_table(key) && inherited_endpoint.is_none() => None,
            _ => endpoint,
        };
        restore_value(value, current, &child_path(path, key), scope)?;
    }
    Ok(())
}

fn changed_endpoint(incoming: &Table, current: &Table, path: &str) -> Option<String> {
    incoming
        .keys()
        .chain(current.keys().filter(|key| !incoming.contains_key(*key)))
        .filter(|key| is_endpoint(key))
        .find(|key| {
            let edited = incoming.get(*key);
            !edited.is_some_and(is_sentinel) && edited != current.get(*key)
        })
        .map(|key| child_path(path, key))
}

fn mark_secrets(plain: &Value, masked: &mut Value, path: &str, secret_paths: &mut Vec<String>) {
    match (plain, masked) {
        (Value::Table(plain), Value::Table(masked)) => {
            for (key, masked) in masked.iter_mut() {
                if let Some(plain) = plain.get(key) {
                    mark_secrets(plain, masked, &child_path(path, key), secret_paths);
                }
            }
        }
        (Value::Array(plain), Value::Array(masked)) => {
            for (index, (plain, masked)) in plain.iter().zip(masked.iter_mut()).enumerate() {
                mark_secrets(plain, masked, &format!("{path}[{index}]"), secret_paths);
            }
        }
        (plain, masked) if is_sentinel(masked) && !is_sentinel(plain) => {
            secret_paths.push(path.to_string());
            if plain.as_str() == Some("") {
                *masked = Value::String(String::new());
            }
        }
        _ => {}
    }
}

fn collect_sentinels(value: &Value, path: &str, paths: &mut Vec<String>) {
    match value {
        Value::Table(table) => {
            for (key, child) in table {
                collect_sentinels(child, &child_path(path, key), paths);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_sentinels(child, &format!("{path}[{index}]"), paths);
            }
        }
        value if is_sentinel(value) => paths.push(path.to_string()),
        _ => {}
    }
}

fn collect_differences(
    left: Option<&Value>,
    right: Option<&Value>,
    path: &str,
    paths: &mut Vec<String>,
) {
    match (left, right) {
        (Some(Value::Table(left)), Some(Value::Table(right))) => {
            let keys = left
                .keys()
                .chain(right.keys().filter(|key| !left.contains_key(*key)));
            for key in keys {
                collect_differences(left.get(key), right.get(key), &child_path(path, key), paths);
            }
        }
        (Some(Value::Array(left)), Some(Value::Array(right))) if left.len() == right.len() => {
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                collect_differences(Some(left), Some(right), &format!("{path}[{index}]"), paths);
            }
        }
        (left, right) if left != right => paths.push(path.to_string()),
        _ => {}
    }
}

fn section_templates() -> &'static [OptionalSection] {
    static TEMPLATES: OnceLock<Vec<OptionalSection>> = OnceLock::new();
    TEMPLATES.get_or_init(|| {
        [
            ("issues.linear", Value::try_from(LinearConfig::default())),
            ("issues.sentry", Value::try_from(SentryConfig::default())),
            ("issues.jira", Value::try_from(JiraConfig::default())),
            (
                "issues.discord",
                Value::try_from(DiscordSourceConfig::default()),
            ),
            (
                "issues.slack",
                Value::try_from(SlackSourceConfig::default()),
            ),
            (
                "issues.helpscout",
                Value::try_from(HelpScoutConfig::default()),
            ),
            (
                "issues.github_code_scanning",
                Value::try_from(GitHubCodeScanningConfig::default()),
            ),
            ("scm.gitlab", Value::try_from(GitLabConfig::default())),
            (
                "knowledgebase.discord",
                Value::try_from(DiscordKnowledgebaseConfig::default()),
            ),
        ]
        .into_iter()
        .filter_map(|(key, value)| {
            let template = render_section(key, value.ok()?)?;
            Some(OptionalSection {
                key: key.to_string(),
                template,
            })
        })
        .collect()
    })
}

fn render_section(key: &str, value: Value) -> Option<String> {
    let nested = key.rsplit('.').fold(value, |inner, segment| {
        let mut table = Table::new();
        table.insert(segment.to_string(), inner);
        Value::Table(table)
    });
    toml::to_string_pretty(&nested).ok()
}

#[cfg(test)]
pub(super) struct HeldWriteLock {
    release: std::sync::mpsc::Sender<()>,
    holder: std::thread::JoinHandle<()>,
}

#[cfg(test)]
impl HeldWriteLock {
    pub(super) fn acquire() -> Self {
        let (locked, acquired) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _lock = lock_writes();
            let _ = locked.send(());
            let _ = released.recv();
        });
        acquired.recv().expect("the write lock holder started");
        Self { release, holder }
    }

    pub(super) fn release(self) {
        let _ = self.release.send(());
        self.holder.join().expect("the write lock holder finished");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_config::config::{
        HelpScoutMailboxConfig, MailboxProfile, McpServerConfig, UserConfig,
    };
    use claudear_core::secret::SecretValue;
    use std::path::PathBuf;

    const NON_SECRET_FIELDS: &[(&str, &str)] = &[
        ("max_tokens", "a generation length limit, not a credential"),
        (
            "private_key_path",
            "a filesystem path to the key, not the key itself",
        ),
        ("project_keys", "Jira project keys such as PROJ"),
    ];

    const PLAIN_SECRET_FIELD_NAMES: &[(&str, &str)] = &[
        ("github_token", "regression.github_token"),
        ("user_key", "notifiers.push.user_key"),
        ("push_user_key", "users.*.push_user_key"),
    ];

    const FAKE_SECRETS: &[(&str, &str)] = &[
        ("scm.github.token", "fake-github-token-0001"),
        ("scm.github.app.private_key", "fake-pem-line-0002"),
        ("notifiers.discord.webhook_url", "fake-webhook-0003"),
        ("issues.helpscout.app_id", "fake-helpscout-app-0004"),
        ("regression.github_token", "fake-regression-token-0005"),
        (
            "agent.providers.claude.env.OPENAI_KEY",
            "fake-openai-key-0006",
        ),
        (
            "agent.providers.claude.mcp.x.headers.Authorization",
            "fake-mcp-header-0007",
        ),
        (
            "agent.providers.claude.extra.secret_thing",
            "fake-extra-secret-0008",
        ),
        (
            "agent.providers.claude.extra.nested.list[0]",
            "fake-extra-list-0009",
        ),
        ("notifiers.sms.account_sid", "fake-twilio-sid-0010"),
        ("notifiers.push.user_key", "fake-pushover-user-0011"),
        ("users.jake.push_user_key", "fake-pushover-user-0012"),
    ];

    fn lookup_mut<'a>(document: &'a mut Value, path: &str) -> Option<&'a mut Value> {
        path.split('.').try_fold(document, |value, segment| {
            value.as_table_mut()?.get_mut(segment)
        })
    }

    fn rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).expect("readable config source directory") {
            let path = entry.expect("readable directory entry").path();
            if path.is_dir() {
                rust_files(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }

    #[test]
    fn secret_named_config_fields_are_typed_or_registered() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../claudear-config/src");
        let mut files = Vec::new();
        rust_files(&source, &mut files);
        assert!(
            files.iter().any(|file| file.ends_with("config.rs")),
            "expected to scan claudear-config sources under {}",
            source.display()
        );

        let field = regex_lite::Regex::new(r"(?m)^\s*pub\s+(\w+)\s*:\s*([^\n]+?),?\s*$")
            .expect("valid field regex");
        let secret_name = regex_lite::Regex::new("token|secret|password|key|credential")
            .expect("valid name regex");
        let mut scanned = 0;
        for file in &files {
            let content = fs::read_to_string(file).expect("readable config source");
            for captures in field.captures_iter(&content) {
                let name = &captures[1];
                let field_type = &captures[2];
                if !secret_name.is_match(&name.to_lowercase()) || field_type.contains("SecretValue")
                {
                    continue;
                }
                scanned += 1;
                if NON_SECRET_FIELDS.iter().any(|(known, _)| *known == name) {
                    continue;
                }
                let plain = PLAIN_SECRET_FIELD_NAMES
                    .iter()
                    .find(|(known, _)| *known == name);
                match plain {
                    Some((_, path)) => assert!(
                        PLAIN_SECRET_PATHS.contains(path),
                        "{name} maps to {path}, which is missing from PLAIN_SECRET_PATHS"
                    ),
                    None => panic!(
                        "config field `{name}: {field_type}` in {} looks like a secret but is \
                         not a SecretValue. Make it a SecretValue, or add its path to \
                         PLAIN_SECRET_PATHS and PLAIN_SECRET_FIELD_NAMES, or list it in \
                         NON_SECRET_FIELDS with the reason it is not a secret.",
                        file.display()
                    ),
                }
            }
        }
        assert!(
            scanned > 0,
            "the guard matched no fields; the regex is stale"
        );
    }

    fn config_with_fake_secrets() -> Config {
        let mut config = Config {
            workspace: "/tmp/repos".into(),
            ..Config::default()
        };
        config.scm.github.token = Some(SecretValue::new("fake-github-token-0001"));
        config.scm.github.app.app_id = Some(424_242);
        config.scm.github.app.private_key = Some(SecretValue::new(
            "-----BEGIN FAKE KEY-----\nfake-pem-line-0002\n-----END FAKE KEY-----\n",
        ));
        config.scm.github.app.private_key_path = Some(PathBuf::from("/keys/fake-app.pem"));
        config.notifiers.discord.webhook_url = Some(SecretValue::new(
            "https://discord.invalid/api/webhooks/fake-webhook-0003",
        ));
        config.issues.helpscout = Some(HelpScoutConfig {
            app_id: SecretValue::new("fake-helpscout-app-0004"),
            ..HelpScoutConfig::default()
        });
        config.regression.github_token = Some("fake-regression-token-0005".to_string());
        config.notifiers.sms.account_sid = Some("fake-twilio-sid-0010".to_string());
        config.notifiers.push.user_key = Some("fake-pushover-user-0011".to_string());
        config.users.insert(
            "jake".to_string(),
            UserConfig {
                push_user_key: Some("fake-pushover-user-0012".to_string()),
                ..UserConfig::default()
            },
        );
        config.chat.max_tokens = 4321;
        config.mcp_server.require_auth = true;
        config.issues.jira = Some(JiraConfig {
            auth_mode: "bearer".to_string(),
            ..JiraConfig::default()
        });
        config.issues.slack = Some(SlackSourceConfig {
            app_id: Some("A0FAKEAPP".to_string()),
            ..SlackSourceConfig::default()
        });

        let provider = config.agent.default_provider_config_mut();
        provider
            .env
            .insert("OPENAI_KEY".to_string(), "fake-openai-key-0006".to_string());
        provider.extra.insert(
            "secret_thing".to_string(),
            serde_json::json!("fake-extra-secret-0008"),
        );
        provider.extra.insert(
            "nested".to_string(),
            serde_json::json!({ "list": ["fake-extra-list-0009"] }),
        );
        let mut server = McpServerConfig {
            url: Some("https://mcp.invalid".to_string()),
            ..McpServerConfig::default()
        };
        server.headers.insert(
            "Authorization".to_string(),
            "Bearer fake-mcp-header-0007".to_string(),
        );
        provider.mcp.insert("x".to_string(), server);
        config
    }

    #[test]
    fn redact_hides_every_secret_and_lists_its_path() {
        let document = redact(&config_with_fake_secrets()).unwrap();
        for (path, fake) in FAKE_SECRETS {
            assert!(
                !document.content.contains(fake),
                "{fake} leaked into content:\n{}",
                document.content
            );
            assert!(
                document.secret_paths.iter().any(|listed| listed == path),
                "{path} missing from {:?}",
                document.secret_paths
            );
        }
        assert!(!document.content.contains("BEGIN FAKE KEY"));
    }

    #[test]
    fn redact_leaves_non_secrets_visible() {
        let document = redact(&config_with_fake_secrets()).unwrap();
        let content: Value = toml::from_str(&document.content).unwrap();
        let expectations = [
            ("chat.max_tokens", Value::Integer(4321)),
            ("mcp_server.require_auth", Value::Boolean(true)),
            ("issues.jira.auth_mode", Value::String("bearer".to_string())),
            (
                "issues.slack.app_id",
                Value::String("A0FAKEAPP".to_string()),
            ),
            (
                "scm.github.app.private_key_path",
                Value::String("/keys/fake-app.pem".to_string()),
            ),
            ("scm.github.app.app_id", Value::Integer(424_242)),
        ];
        for (path, expected) in expectations {
            assert_eq!(lookup(&content, path), Some(&expected), "{path}");
            assert!(
                !document.secret_paths.iter().any(|listed| listed == path),
                "{path} must not be redacted"
            );
        }
    }

    fn config_with_security_inbox() -> Config {
        let mut config = config_with_fake_secrets();
        if let Some(helpscout) = config.issues.helpscout.as_mut() {
            helpscout.mailboxes = vec![HelpScoutMailboxConfig {
                id: "249891".to_string(),
                profile: MailboxProfile::Security,
                reply_as: None,
                trigger_tags: None,
                trigger_status: Some("active".to_string()),
            }];
            helpscout.security.dashboard_url = Some("https://claudear.example.com".to_string());
            helpscout.security.owner_mention = Some("<@1234>".to_string());
            helpscout.security.default_repository = Some("appwrite/appwrite".to_string());
        }
        config
    }

    #[test]
    fn redact_leaves_the_security_inbox_visible_and_restores_it() {
        let config = config_with_security_inbox();
        let document = redact(&config).unwrap();
        let content: Value = toml::from_str(&document.content).unwrap();
        let expectations = [
            (
                "issues.helpscout.security.dashboard_url",
                Value::String("https://claudear.example.com".to_string()),
            ),
            (
                "issues.helpscout.security.owner_mention",
                Value::String("<@1234>".to_string()),
            ),
            (
                "issues.helpscout.security.reward.campaign",
                Value::String("SecurityResearchers".to_string()),
            ),
            (
                "issues.helpscout.security.deployment.configuration_repository",
                Value::String("appwrite-labs/application-configuration".to_string()),
            ),
        ];
        for (path, expected) in expectations {
            assert_eq!(lookup(&content, path), Some(&expected), "{path}");
        }
        assert_eq!(
            content["issues"]["helpscout"]["mailboxes"][0]["profile"],
            Value::String("security".to_string())
        );
        let hidden: Vec<&String> = document
            .secret_paths
            .iter()
            .filter(|path| {
                path.starts_with("issues.helpscout.security")
                    || path.starts_with("issues.helpscout.mailboxes")
            })
            .collect();
        assert!(
            hidden.is_empty(),
            "security inbox values redacted: {hidden:?}"
        );

        let on_disk: Value = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        let incoming: Value = toml::from_str(&document.content).unwrap();
        let restored: Config = restore_all(incoming, &on_disk).unwrap().try_into().unwrap();
        let helpscout = restored.issues.helpscout.expect("helpscout");
        let original = config.issues.helpscout.expect("helpscout");
        assert_eq!(helpscout.mailboxes, original.mailboxes);
        assert_eq!(helpscout.security, original.security);
    }

    #[test]
    fn redact_shows_unset_secrets_as_empty_and_lists_them() {
        let document = redact(&config_with_fake_secrets()).unwrap();
        let content: Value = toml::from_str(&document.content).unwrap();
        assert_eq!(
            lookup(&content, "issues.helpscout.app_secret"),
            Some(&Value::String(String::new()))
        );
        assert!(document
            .secret_paths
            .iter()
            .any(|listed| listed == "issues.helpscout.app_secret"));
    }

    #[test]
    fn redact_keeps_struct_order() {
        let document = redact(&config_with_fake_secrets()).unwrap();
        let position = |needle: &str| {
            document
                .content
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} missing:\n{}", document.content))
        };
        assert!(position("[agent") < position("[scm."));
        assert!(position("[scm.") < position("[issues."));
        assert!(position("[issues.") < position("[notifiers."));
    }

    #[test]
    fn redacted_content_restores_to_the_original_secrets() {
        let config = config_with_fake_secrets();
        let on_disk: Value = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        let mut incoming: Value = toml::from_str(&redact(&config).unwrap().content).unwrap();

        restore(&mut incoming, &on_disk).unwrap();

        assert!(
            unrestored(&incoming).is_empty(),
            "{:?}",
            unrestored(&incoming)
        );
        for (path, fake) in FAKE_SECRETS {
            let text = toml::to_string(&incoming).unwrap();
            assert!(text.contains(fake), "{path} was not restored");
        }
        let restored: Config = incoming.try_into().unwrap();
        assert_eq!(
            restored.regression.github_token.as_deref(),
            Some("fake-regression-token-0005")
        );
    }

    fn merged(incoming: &str, current: &str) -> Value {
        let mut incoming: Value = toml::from_str(incoming).expect("valid incoming toml");
        let current: Value = toml::from_str(current).expect("valid current toml");
        restore(&mut incoming, &current).expect("restorable");
        incoming
    }

    #[test]
    fn restore_replaces_sentinel() {
        let value = merged(
            "api_token = \"[REDACTED]\"",
            "api_token = \"fake-real-token\"",
        );
        assert_eq!(value["api_token"].as_str(), Some("fake-real-token"));
    }

    #[test]
    fn restore_keeps_edited_value() {
        let value = merged("api_token = \"fake-new\"", "api_token = \"fake-old\"");
        assert_eq!(value["api_token"].as_str(), Some("fake-new"));
    }

    #[test]
    fn restore_leaves_non_secret_values_untouched() {
        let value = merged(
            "workspace = \"/tmp/repos\"\npoll_interval_ms = 300000",
            "workspace = \"/other\"\npoll_interval_ms = 1",
        );
        assert_eq!(value["workspace"].as_str(), Some("/tmp/repos"));
        assert_eq!(value["poll_interval_ms"].as_integer(), Some(300000));
    }

    #[test]
    fn restore_nested_table() {
        let value = merged(
            "[notifiers.helpscout]\napi_key = \"[REDACTED]\"\nmailbox = \"support\"",
            "[notifiers.helpscout]\napi_key = \"fake-hs\"\nmailbox = \"old\"",
        );
        assert_eq!(
            value["notifiers"]["helpscout"]["api_key"].as_str(),
            Some("fake-hs")
        );
        assert_eq!(
            value["notifiers"]["helpscout"]["mailbox"].as_str(),
            Some("support")
        );
    }

    #[test]
    fn restore_arrays_element_by_element_when_lengths_match() {
        let value = merged(
            "tokens = [\"[REDACTED]\", \"keep\"]",
            "tokens = [\"fake-real-0\", \"stale-1\"]",
        );
        let items = value["tokens"].as_array().expect("array");
        assert_eq!(items[0].as_str(), Some("fake-real-0"));
        assert_eq!(items[1].as_str(), Some("keep"));
        assert!(unrestored(&value).is_empty());
    }

    #[test]
    fn restore_skips_arrays_whose_lengths_differ() {
        let value = merged(
            "tokens = [\"[REDACTED]\", \"keep\"]",
            "tokens = [\"fake-real-0\"]",
        );
        assert_eq!(value["tokens"][0].as_str(), Some(REDACTED));
        assert_eq!(unrestored(&value), vec!["tokens[0]"]);
    }

    #[test]
    fn restore_restores_arrays_of_tables() {
        let value = merged(
            "[[servers]]\nname = \"a\"\ntoken = \"[REDACTED]\"",
            "[[servers]]\nname = \"a\"\ntoken = \"fake-server-token\"",
        );
        assert_eq!(
            value["servers"][0]["token"].as_str(),
            Some("fake-server-token")
        );
    }

    #[test]
    fn restore_missing_on_disk_key_keeps_sentinel() {
        let value = merged("api_token = \"[REDACTED]\"", "other = \"x\"");
        assert_eq!(value["api_token"].as_str(), Some(REDACTED));
        assert_eq!(unrestored(&value), vec!["api_token"]);
    }

    #[test]
    fn unrestored_reports_paths() {
        let value: Value = toml::from_str(
            "top = \"[REDACTED]\"\n[notifiers.helpscout]\napi_key = \"[REDACTED]\"\n\
             [[list]]\nsecret = \"[REDACTED]\"",
        )
        .unwrap();
        let mut paths = unrestored(&value);
        paths.sort();
        assert_eq!(
            paths,
            vec!["list[0].secret", "notifiers.helpscout.api_key", "top"]
        );
    }

    #[test]
    fn every_optional_section_template_parses_back_into_config() {
        let sections = optional_sections(&Value::Table(Table::new()));
        assert_eq!(sections.len(), 9, "{sections:?}");
        for section in sections {
            let normalized = normalize(&section.template)
                .unwrap_or_else(|error| panic!("{}: {}", section.key, error.message));
            assert!(
                lookup(&normalized, &section.key).is_some(),
                "{} template did not define its section:\n{}",
                section.key,
                section.template
            );
        }
    }

    #[test]
    fn optional_sections_skip_sections_already_present() {
        let document = normalize("[issues.linear]\napi_key = \"\"\n").unwrap();
        let keys: Vec<String> = optional_sections(&document)
            .into_iter()
            .map(|section| section.key)
            .collect();
        assert!(!keys.contains(&"issues.linear".to_string()));
        assert!(keys.contains(&"scm.gitlab".to_string()));
    }

    #[test]
    fn revision_is_sha256_hex() {
        assert_eq!(
            revision(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(revision(b"a"), revision(b"b"));
    }

    fn valid_document() -> Value {
        let mut config = Config {
            workspace: "/tmp/repos".into(),
            ..Config::default()
        };
        config.issues.linear = Some(LinearConfig {
            api_key: SecretValue::new("fake-linear-key"),
            ..LinearConfig::default()
        });
        Value::try_from(&config).unwrap()
    }

    #[tokio::test]
    async fn write_validated_writes_normalized_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        fs::write(&path, "# comment\n").unwrap();
        let expected = revision(&fs::read(&path).unwrap());

        let outcome = write_validated(&ConfigFile::new(&path), Some(&expected), |_| {
            Ok(valid_document())
        })
        .await
        .unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("fake-linear-key"));
        assert_eq!(outcome.revision, revision(written.as_bytes()));
        assert_eq!(outcome.normalized, normalize(&written).unwrap());
        let leftovers: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(SIBLING_PREFIX)
            })
            .collect();
        assert!(leftovers.is_empty());
    }

    #[tokio::test]
    async fn write_validated_rejects_a_stale_revision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        fs::write(&path, "workspace = \"/tmp/repos\"\n").unwrap();

        let error = write_validated(&ConfigFile::new(&path), Some("stale"), |_| {
            Ok(valid_document())
        })
        .await
        .unwrap_err();

        assert_eq!(error.code, ConfigErrorCode::RevisionMismatch);
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "workspace = \"/tmp/repos\"\n"
        );
    }

    #[tokio::test]
    async fn write_validated_rejects_unrestored_placeholders() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        let mut document = valid_document();
        *lookup_mut(&mut document, "issues.linear.api_key").unwrap() =
            Value::String(REDACTED.to_string());

        let error = write_validated(&ConfigFile::new(&path), None, move |current| {
            restore_all(document, &current)
        })
        .await
        .unwrap_err();

        assert_eq!(error.code, ConfigErrorCode::Invalid);
        assert!(error.message.contains("issues.linear.api_key"));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn write_validated_rejects_a_config_that_fails_validation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        let document: Value = toml::from_str("workspace = \"/tmp/repos\"\n").unwrap();

        let error = write_validated(&ConfigFile::new(&path), None, move |_| Ok(document))
            .await
            .unwrap_err();

        assert_eq!(error.code, ConfigErrorCode::Invalid);
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.starts_with("Config validation failed"));
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_validated_keeps_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        write_validated(&ConfigFile::new(&path), None, |_| Ok(valid_document()))
            .await
            .unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_to_another_directory_named_at_startup_is_followed() {
        let elsewhere = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("real.toml");
        let link = directory.path().join("claudear.toml");
        fs::write(&target, "workspace = \"/tmp/repos\"\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let file = ConfigFile::new(&link);

        assert_eq!(
            read_locked(&file).await.unwrap(),
            fs::read(&target).unwrap()
        );
        write_validated(&file, None, |_| Ok(valid_document()))
            .await
            .unwrap();
        assert!(fs::read_to_string(&target)
            .unwrap()
            .contains("fake-linear-key"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_re_pointed_within_its_directory_is_followed() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("a.toml");
        let second = directory.path().join("b.toml");
        let link = directory.path().join("claudear.toml");
        fs::write(&first, "# a\n").unwrap();
        fs::write(&second, "# b\n").unwrap();
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let file = ConfigFile::new(&link);

        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&second, &link).unwrap();

        assert_eq!(read_locked(&file).await.unwrap(), b"# b\n");
        write_validated(&file, None, |_| Ok(valid_document()))
            .await
            .unwrap();
        assert!(fs::read_to_string(&second)
            .unwrap()
            .contains("fake-linear-key"));
        assert_eq!(fs::read_to_string(&first).unwrap(), "# a\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_config_map_update_is_followed_after_the_old_data_is_deleted() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let mount = directory.path();
        let first = mount.join("..t1");
        let second = mount.join("..t2");
        fs::create_dir(&first).unwrap();
        fs::write(first.join("claudear.toml"), "# t1\n").unwrap();
        symlink("..t1", mount.join("..data")).unwrap();
        symlink("..data/claudear.toml", mount.join("claudear.toml")).unwrap();
        let file = ConfigFile::new(&mount.join("claudear.toml"));
        assert_eq!(read_locked(&file).await.unwrap(), b"# t1\n");

        fs::create_dir(&second).unwrap();
        fs::write(second.join("claudear.toml"), "# t2\n").unwrap();
        symlink("..t2", mount.join("..data_tmp")).unwrap();
        fs::rename(mount.join("..data_tmp"), mount.join("..data")).unwrap();
        fs::remove_dir_all(&first).unwrap();

        assert_eq!(read_locked(&file).await.unwrap(), b"# t2\n");
        write_validated(&file, None, |_| Ok(valid_document()))
            .await
            .unwrap();
        assert!(fs::read_to_string(second.join("claudear.toml"))
            .unwrap()
            .contains("fake-linear-key"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_config_file_swapped_for_a_symlink_out_of_its_directory_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        let outside = elsewhere.path().join("outside.toml");
        fs::write(&path, "workspace = \"/tmp/repos\"\n").unwrap();
        fs::write(&outside, "untouched").unwrap();
        let file = ConfigFile::new(&path);

        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();

        let error = read_locked(&file).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!writable(&file));
        assert!(runtime_overrides(&file, &Config::default()).is_empty());
        write_validated(&file, None, |_| Ok(valid_document()))
            .await
            .unwrap_err();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
    }

    #[tokio::test]
    async fn a_missing_config_file_is_created_in_its_directory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        let file = ConfigFile::new(&path);

        let error = read_locked(&file).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(writable(&file));
        write_validated(&file, None, |_| Ok(valid_document()))
            .await
            .unwrap();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("fake-linear-key"));
    }

    #[tokio::test]
    async fn a_directory_is_not_a_config_file() {
        let directory = tempfile::tempdir().unwrap();
        let file = ConfigFile::new(directory.path());

        let error = read_locked(&file).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!writable(&file));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_validated_writes_through_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("real.toml");
        let link = directory.path().join("claudear.toml");
        fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_validated(&ConfigFile::new(&link), None, |_| Ok(valid_document()))
            .await
            .unwrap();

        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(fs::read_to_string(&target)
            .unwrap()
            .contains("fake-linear-key"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_validated_refuses_a_read_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        fs::write(&path, "workspace = \"/tmp/repos\"\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        if fs::OpenOptions::new().write(true).open(&path).is_ok() {
            return;
        }

        assert!(!writable(&ConfigFile::new(&path)));
        let error = write_validated(&ConfigFile::new(&path), None, |_| Ok(valid_document()))
            .await
            .unwrap_err();

        assert_eq!(error.code, ConfigErrorCode::ReadOnly);
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "workspace = \"/tmp/repos\"\n"
        );
    }

    #[test]
    fn writable_reports_a_missing_file_in_a_writable_directory() {
        let directory = tempfile::tempdir().unwrap();
        assert!(writable(&ConfigFile::new(
            &directory.path().join("claudear.toml")
        )));
    }

    #[test]
    fn in_place_fallback_covers_mount_errors() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::ReadOnlyFilesystem,
            io::ErrorKind::CrossesDevices,
            io::ErrorKind::ResourceBusy,
        ] {
            assert!(allows_in_place_write(&io::Error::from(kind)), "{kind:?}");
        }
        assert!(!allows_in_place_write(&io::Error::from(
            io::ErrorKind::StorageFull
        )));
        assert!(is_read_only(&io::Error::from(
            io::ErrorKind::ReadOnlyFilesystem
        )));
        assert!(!is_read_only(&io::Error::from(io::ErrorKind::ResourceBusy)));
    }

    #[cfg(unix)]
    #[test]
    fn in_place_fallback_covers_raw_mount_errors() {
        for code in [libc::EACCES, libc::EROFS, libc::EXDEV, libc::EBUSY] {
            assert!(allows_in_place_write(&io::Error::from_raw_os_error(code)));
        }
        assert!(is_read_only(&io::Error::from_raw_os_error(libc::EROFS)));
    }

    #[test]
    fn runtime_overrides_reports_resolved_instructions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        fs::write(directory.path().join("extra.md"), "fake instructions").unwrap();
        let content = "workspace = \"/tmp/repos\"\n\n\
                       [agent.providers.claude]\ninstructions_file = \"extra.md\"\n";
        fs::write(&path, content).unwrap();

        let overrides = runtime_overrides(&ConfigFile::new(&path), &parse(content).unwrap());

        assert!(
            overrides.contains(&"agent.providers.claude.instructions".to_string()),
            "{overrides:?}"
        );
        assert!(overrides
            .iter()
            .all(|path| !path.contains("fake instructions")));
    }

    #[test]
    fn runtime_overrides_is_empty_when_the_file_cannot_load() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.toml");
        assert!(runtime_overrides(&ConfigFile::new(&path), &Config::default()).is_empty());
    }

    #[test]
    fn error_body_carries_message_and_code() {
        let response = ConfigFileError::new(ConfigErrorCode::ReadOnly, "nope").into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = serde_json::to_value(ConfigErrorBody {
            error: "nope",
            code: ConfigErrorCode::RevisionMismatch,
        })
        .unwrap();
        assert_eq!(body["error"], "nope");
        assert_eq!(body["code"], "revision_mismatch");
    }

    const UNQUOTED_SECRET_FILE: &str = "[scm.github]\ntoken = fake-secret-unquoted\n";

    fn assert_omits(message: &str, forbidden: &str) {
        assert!(
            !message.contains(forbidden),
            "the message repeats a value it must omit"
        );
    }

    #[test]
    fn describe_toml_error_omits_the_source_line() {
        let error = toml::from_str::<Value>(UNQUOTED_SECRET_FILE).unwrap_err();
        assert!(error.to_string().contains("fake-secret-unquoted"));

        let described = describe_toml_error(&error, UNQUOTED_SECRET_FILE);

        assert_omits(&described, "fake-secret-unquoted");
        assert!(described.ends_with("(line 2)"), "{described}");
        assert!(described.starts_with(error.message()), "{described}");
    }

    #[test]
    fn parse_errors_omit_the_source_line() {
        let error = parse(UNQUOTED_SECRET_FILE).unwrap_err();
        assert_eq!(error.code, ConfigErrorCode::Invalid);
        assert_omits(&error.message, "fake-secret-unquoted");

        let error = disk_document(UNQUOTED_SECRET_FILE.as_bytes()).unwrap_err();
        assert_omits(&error.message, "fake-secret-unquoted");
        assert!(error.message.contains("(line 2)"), "{}", error.message);
    }

    #[test]
    fn type_errors_omit_the_offending_value() {
        let content = "poll_interval_ms = \"fake-secret-in-number\"\n";

        let error = parse(content).unwrap_err();
        assert_omits(&error.message, "fake-secret-in-number");
        assert!(error.message.contains("expected u64"), "{}", error.message);
        assert!(error.message.contains("(line 1)"), "{}", error.message);

        let document: Value = toml::from_str(content).unwrap();
        let error = document.try_into::<Config>().unwrap_err();
        assert!(error.message().contains("fake-secret-in-number"));
        assert_omits(
            &describe_toml_error(&error, content),
            "fake-secret-in-number",
        );
    }

    #[test]
    fn scrub_values_removes_quoted_values_before_the_expected_clause() {
        let cases = [
            (
                "invalid type: string \"fake \\\"quoted\\\" secret\", expected u64",
                "invalid type: string \"...\", expected u64",
            ),
            (
                "invalid type: integer `424242`, expected a string",
                "invalid type: integer `...`, expected a string",
            ),
            (
                "unknown variant `fake-variant`, expected one of `stdio`, `http`",
                "unknown variant `...`, expected one of `stdio`, `http`",
            ),
            (
                "unknown tag_filter 'fake-filter' (expected any | suffix:<text>)",
                "unknown tag_filter '...' (expected any | suffix:<text>)",
            ),
            (
                "invalid string, expected `\"`, `'`",
                "invalid string, expected `\"`, `'`",
            ),
            (
                "custom failure for 'fake-unterminated",
                "custom failure for '...",
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(scrub_values(message), expected, "{message}");
        }
    }

    #[tokio::test]
    async fn write_validated_reports_type_errors_without_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        let mut document = valid_document();
        document.as_table_mut().unwrap().insert(
            "poll_interval_ms".to_string(),
            Value::String("fake-typed-secret-0001".to_string()),
        );

        let error = write_validated(&ConfigFile::new(&path), None, move |_| Ok(document))
            .await
            .unwrap_err();

        assert_eq!(error.code, ConfigErrorCode::Invalid);
        assert_omits(&error.message, "fake-typed-secret-0001");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn write_validated_reports_an_unparseable_disk_file_without_its_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        fs::write(&path, UNQUOTED_SECRET_FILE).unwrap();

        let error = write_validated(&ConfigFile::new(&path), None, |_| Ok(valid_document()))
            .await
            .unwrap_err();

        assert_eq!(error.code, ConfigErrorCode::Unreadable);
        assert_omits(&error.message, "fake-secret-unquoted");
        assert_eq!(fs::read_to_string(&path).unwrap(), UNQUOTED_SECRET_FILE);
    }

    #[tokio::test]
    async fn write_validated_merges_against_the_checked_document() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        let base = render(&valid_document().try_into::<Config>().unwrap()).unwrap();
        fs::write(&path, &base).unwrap();
        let expected = revision(base.as_bytes());

        write_validated(&ConfigFile::new(&path), Some(&expected), move |current| {
            assert_eq!(current, toml::from_str::<Value>(&base).unwrap());
            Ok(current)
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn concurrent_merges_keep_every_update() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        let base = render(&valid_document().try_into::<Config>().unwrap()).unwrap();
        fs::write(&path, &base).unwrap();

        let writers: Vec<_> = (0..8)
            .map(|index| {
                let path = path.clone();
                tokio::spawn(async move {
                    write_validated(&ConfigFile::new(&path), None, move |mut current| {
                        let templates = ["notifiers", "helpscout", "templates"].into_iter().fold(
                            &mut current,
                            |value, segment| {
                                value
                                    .as_table_mut()
                                    .unwrap()
                                    .entry(segment)
                                    .or_insert_with(|| Value::Table(Table::new()))
                            },
                        );
                        templates.as_table_mut().unwrap().insert(
                            format!("inbox-{index}"),
                            Value::String(format!("template {index}")),
                        );
                        Ok(current)
                    })
                    .await
                })
            })
            .collect();
        for writer in writers {
            writer.await.unwrap().unwrap();
        }

        let written = parse(&fs::read_to_string(&path).unwrap()).unwrap();
        let mut inboxes: Vec<String> = written.notifiers.helpscout.templates.into_keys().collect();
        inboxes.sort();
        let expected: Vec<String> = (0..8).map(|index| format!("inbox-{index}")).collect();
        assert_eq!(inboxes, expected);
    }

    fn restore_error(incoming: &str, current: &str) -> String {
        let mut incoming: Value = toml::from_str(incoming).expect("valid incoming toml");
        let current: Value = toml::from_str(current).expect("valid current toml");
        restore(&mut incoming, &current)
            .expect_err("restore should refuse")
            .message
    }

    #[test]
    fn restore_refuses_a_secret_when_its_endpoint_changes() {
        let cases = [
            (
                "[scm.gitlab]\nbase_url = \"https://evil.invalid\"\ntoken = \"[REDACTED]\"",
                "[scm.gitlab]\nbase_url = \"https://gitlab.com\"\ntoken = \"fake-gitlab\"",
                "Re-enter scm.gitlab.token when changing scm.gitlab.base_url",
            ),
            (
                "[agent.providers.x]\napi_url = \"https://evil.invalid\"\napi_key = \"[REDACTED]\"",
                "[agent.providers.x]\napi_url = \"https://api.invalid\"\napi_key = \"fake-key\"",
                "Re-enter agent.providers.x.api_key when changing agent.providers.x.api_url",
            ),
            (
                "[notifiers.email]\nsmtp_host = \"evil.invalid\"\nsmtp_password = \"[REDACTED]\"",
                "[notifiers.email]\nsmtp_host = \"smtp.invalid\"\nsmtp_password = \"fake-pw\"",
                "Re-enter notifiers.email.smtp_password when changing notifiers.email.smtp_host",
            ),
            (
                "[issues.jira]\napi_token = \"[REDACTED]\"",
                "[issues.jira]\nbase_url = \"https://jira.invalid\"\napi_token = \"fake-jira\"",
                "Re-enter issues.jira.api_token when changing issues.jira.base_url",
            ),
            (
                "[mcp.x]\nurl = \"https://evil.invalid\"\n[mcp.x.headers]\nAuthorization = \"[REDACTED]\"",
                "[mcp.x]\nurl = \"https://mcp.invalid\"\n[mcp.x.headers]\nAuthorization = \"fake-bearer\"",
                "Re-enter mcp.x.headers.Authorization when changing mcp.x.url",
            ),
            (
                "[mcp.x]\nurl = \"https://evil.invalid\"\n[mcp.x.env]\nTOKEN = \"[REDACTED]\"",
                "[mcp.x]\nurl = \"https://mcp.invalid\"\n[mcp.x.env]\nTOKEN = \"fake-env\"",
                "Re-enter mcp.x.env.TOKEN when changing mcp.x.url",
            ),
            (
                "[servers]\nimap_host = \"evil.invalid\"\ntokens = [\"[REDACTED]\"]",
                "[servers]\nimap_host = \"imap.invalid\"\ntokens = [\"fake-0\"]",
                "Re-enter servers.tokens[0] when changing servers.imap_host",
            ),
        ];
        for (incoming, current, expected) in cases {
            assert_eq!(restore_error(incoming, current), expected);
        }
    }

    #[test]
    fn restore_keeps_a_secret_when_its_endpoint_is_unchanged() {
        let value = merged(
            "[scm.gitlab]\nbase_url = \"https://gitlab.com\"\ntoken = \"[REDACTED]\"\nlabel = \"new\"",
            "[scm.gitlab]\nbase_url = \"https://gitlab.com\"\ntoken = \"fake-gitlab\"\nlabel = \"old\"",
        );
        assert_eq!(
            value["scm"]["gitlab"]["token"].as_str(),
            Some("fake-gitlab")
        );
    }

    #[test]
    fn restore_treats_a_kept_secret_endpoint_as_unchanged() {
        let value = merged(
            "[notifiers.discord]\nwebhook_url = \"[REDACTED]\"\ntoken = \"[REDACTED]\"",
            "[notifiers.discord]\nwebhook_url = \"https://discord.invalid/fake\"\ntoken = \"fake-bot\"",
        );
        assert!(unrestored(&value).is_empty(), "{value:?}");
        assert_eq!(
            value["notifiers"]["discord"]["token"].as_str(),
            Some("fake-bot")
        );
    }

    #[test]
    fn restore_scopes_an_endpoint_to_its_own_table() {
        let value = merged(
            "[mcp.x]\nurl = \"https://mcp.invalid\"\n[mcp.x.headers]\nAuthorization = \"[REDACTED]\"\n\
             [mcp.y]\nurl = \"https://new.invalid\"",
            "[mcp.x]\nurl = \"https://mcp.invalid\"\n[mcp.x.headers]\nAuthorization = \"fake-bearer\"\n\
             [mcp.y]\nurl = \"https://old.invalid\"",
        );
        assert_eq!(
            value["mcp"]["x"]["headers"]["Authorization"].as_str(),
            Some("fake-bearer")
        );

        let value = merged(
            "[gateway]\npublic_url = \"https://new.invalid\"\n[gateway.child]\ntoken = \"[REDACTED]\"",
            "[gateway]\npublic_url = \"https://old.invalid\"\n[gateway.child]\ntoken = \"fake-child\"",
        );
        assert_eq!(
            value["gateway"]["child"]["token"].as_str(),
            Some("fake-child")
        );
    }

    #[tokio::test]
    async fn restore_all_refuses_a_redirected_secret_before_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        let mut on_disk = valid_document();
        lookup_mut(&mut on_disk, "issues.linear")
            .and_then(Value::as_table_mut)
            .unwrap()
            .insert(
                "api_url".to_string(),
                Value::String("https://api.linear.invalid".to_string()),
            );
        let base = toml::to_string_pretty(&on_disk).unwrap();
        fs::write(&path, &base).unwrap();
        let mut incoming = on_disk.clone();
        let linear = lookup_mut(&mut incoming, "issues.linear")
            .and_then(Value::as_table_mut)
            .unwrap();
        linear.insert(
            "api_url".to_string(),
            Value::String("https://evil.invalid".to_string()),
        );
        linear.insert("api_key".to_string(), Value::String(REDACTED.to_string()));

        let error = write_validated(&ConfigFile::new(&path), None, move |current| {
            restore_all(incoming, &current)
        })
        .await
        .unwrap_err();

        assert_eq!(error.code, ConfigErrorCode::Invalid);
        assert_eq!(
            error.message,
            "Re-enter issues.linear.api_key when changing issues.linear.api_url"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), base);
    }

    const GITLAB_WITHOUT_BASE_URL: &str = "workspace = \"/tmp/repos\"\n\n\
         [issues.linear]\napi_key = \"fake-linear-key\"\n\n\
         [scm.gitlab]\ntoken = \"fake-gitlab-token\"\n";

    const JIRA_WITHOUT_BASE_URL: &str = "workspace = \"/tmp/repos\"\n\n\
         [issues.linear]\napi_key = \"fake-linear-key\"\n\n\
         [issues.jira]\napi_token = \"fake-jira-token\"\n";

    async fn save_redacted(
        disk: &str,
        edit: impl FnOnce(&mut Value),
    ) -> (Result<WriteOutcome, ConfigFileError>, String) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        fs::write(&path, disk).unwrap();
        let mut incoming = parse_document(&redact(&parse(disk).unwrap()).unwrap().content).unwrap();
        edit(&mut incoming);

        let outcome = write_validated(&ConfigFile::new(&path), None, move |current| {
            restore_all(incoming, &current)
        })
        .await;

        (outcome, fs::read_to_string(&path).unwrap())
    }

    #[tokio::test]
    async fn restore_all_keeps_a_secret_whose_omitted_endpoint_renders_as_its_default() {
        let (outcome, written) = save_redacted(GITLAB_WITHOUT_BASE_URL, |_| {}).await;

        outcome.expect("an unchanged redacted config saves");
        let saved = parse(&written).unwrap();
        let gitlab = saved.scm.gitlab.expect("gitlab section kept");
        assert_eq!(
            gitlab.token.as_ref().map(SecretValue::expose),
            Some("fake-gitlab-token")
        );
        assert_eq!(gitlab.base_url, "https://gitlab.com");
    }

    #[tokio::test]
    async fn restore_all_keeps_a_secret_whose_omitted_endpoint_defaults_to_empty() {
        let (outcome, written) = save_redacted(JIRA_WITHOUT_BASE_URL, |_| {}).await;

        outcome.expect("an unchanged redacted config saves");
        let saved = parse(&written).unwrap();
        let jira = saved.issues.jira.expect("jira section kept");
        assert_eq!(jira.api_token.expose(), "fake-jira-token");
        assert_eq!(jira.base_url, "");
    }

    #[tokio::test]
    async fn restore_all_refuses_a_secret_when_an_omitted_endpoint_is_changed() {
        let (outcome, written) = save_redacted(GITLAB_WITHOUT_BASE_URL, |incoming| {
            *lookup_mut(incoming, "scm.gitlab.base_url").unwrap() =
                Value::String("https://evil.invalid".to_string());
        })
        .await;

        let error = outcome.expect_err("a redirected secret is refused");
        assert_eq!(error.code, ConfigErrorCode::Invalid);
        assert_eq!(
            error.message,
            "Re-enter scm.gitlab.token when changing scm.gitlab.base_url"
        );
        assert_eq!(written, GITLAB_WITHOUT_BASE_URL);
    }

    #[tokio::test]
    async fn restore_all_restores_from_a_disk_file_that_does_not_deserialize() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        fs::write(
            &path,
            "workspace = \"/tmp/repos\"\n\n\
             [issues.linear]\napi_key = \"fake-linear-key\"\nmax_concurrent = \"many\"\n",
        )
        .unwrap();
        let mut incoming = valid_document();
        *lookup_mut(&mut incoming, "issues.linear.api_key").unwrap() =
            Value::String(REDACTED.to_string());

        write_validated(&ConfigFile::new(&path), None, move |current| {
            restore_all(incoming, &current)
        })
        .await
        .expect("a corrected config replaces one that does not deserialize");

        let saved = parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved.issues.linear.unwrap().api_key.expose(),
            "fake-linear-key"
        );
    }

    #[cfg(unix)]
    #[test]
    fn writable_does_not_open_the_file() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        let fifo = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let (sender, receiver) = std::sync::mpsc::channel();
        let probe = path.clone();
        std::thread::spawn(move || sender.send(is_writable(&probe)));

        let result = receiver.recv_timeout(std::time::Duration::from_secs(5));
        assert_eq!(
            result,
            Ok(true),
            "is_writable() blocked opening the file for writing"
        );
        assert!(
            !writable(&ConfigFile::new(&path)),
            "a fifo is not a config file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn writable_reports_a_missing_file_in_a_read_only_directory() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let read_only = tempfile::Builder::new()
            .tempfile_in(directory.path())
            .is_err();

        let result = writable(&ConfigFile::new(&directory.path().join("claudear.toml")));

        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(result, !read_only);
    }

    fn config_with_maps(order: &[&str]) -> Config {
        let mut config = Config {
            workspace: "/tmp/repos".into(),
            ..Config::default()
        };
        for name in order {
            let provider = config
                .agent
                .providers
                .entry((*name).to_string())
                .or_default();
            for key in order {
                provider
                    .env
                    .insert(format!("{}_KEY", key.to_uppercase()), (*key).to_string());
            }
            config
                .notifiers
                .helpscout
                .templates
                .insert(format!("inbox-{name}"), (*name).to_string());
            config
                .users
                .insert((*name).to_string(), UserConfig::default());
        }
        config
    }

    #[test]
    fn render_is_deterministic_and_sorts_map_keys() {
        let names = ["delta", "alpha", "foxtrot", "charlie", "echo", "bravo"];
        let mut reversed = names;
        reversed.reverse();

        let first = render(&config_with_maps(&names)).unwrap();
        let second = render(&config_with_maps(&reversed)).unwrap();

        assert_eq!(first, second);
        let mut sorted = names;
        sorted.sort_unstable();
        let positions = |pattern: fn(&str) -> String| -> Vec<usize> {
            sorted
                .iter()
                .map(|name| {
                    first
                        .find(&pattern(name))
                        .unwrap_or_else(|| panic!("{} missing:\n{first}", pattern(name)))
                })
                .collect()
        };
        let patterns: [fn(&str) -> String; 4] = [
            |name| format!("[agent.providers.{name}]\n"),
            |name| format!("\n{}_KEY = ", name.to_uppercase()),
            |name| format!("inbox-{name} = "),
            |name| format!("[users.{name}]\n"),
        ];
        for pattern in patterns {
            let found = positions(pattern);
            assert!(found.is_sorted(), "{found:?} not sorted:\n{first}");
        }
        assert_eq!(
            redact(&config_with_maps(&names)).unwrap().content,
            redact(&config_with_maps(&reversed)).unwrap().content
        );
    }

    #[test]
    fn render_matches_struct_serialization_for_a_config_without_maps() {
        let config = valid_document().try_into::<Config>().unwrap();
        assert_eq!(
            render(&config).unwrap(),
            toml::to_string_pretty(&config).unwrap()
        );
    }

    #[test]
    fn every_hash_map_config_field_is_sorted_on_render() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../claudear-config/src/config.rs");
        let content = fs::read_to_string(&source).expect("readable config source");
        let field = regex_lite::Regex::new(r"(?m)^\s*pub\s+(\w+)\s*:\s*[\w:]*HashMap<")
            .expect("valid field regex");
        let names: Vec<&str> = field
            .captures_iter(&content)
            .map(|captures| captures.get(1).unwrap().as_str())
            .collect();
        assert!(
            !names.is_empty(),
            "the guard matched no fields; the regex is stale"
        );
        for name in names {
            assert!(
                MAP_PATHS
                    .iter()
                    .any(|path| path.rsplit('.').next() == Some(name)),
                "HashMap field `{name}` has no entry in MAP_PATHS, so its keys render in \
                 random order"
            );
        }
    }

    #[tokio::test]
    async fn reads_wait_for_an_in_progress_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        fs::write(&path, "workspace = \"/tmp/repos\"\n").unwrap();
        let held = HeldWriteLock::acquire();

        let read = tokio::spawn({
            let path = path.clone();
            async move { read_locked(&ConfigFile::new(&path)).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !read.is_finished(),
            "the read did not wait for the write lock"
        );

        held.release();
        assert_eq!(
            read.await.unwrap().unwrap(),
            b"workspace = \"/tmp/repos\"\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_validated_keeps_the_file_owner() {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        fs::write(&path, "").unwrap();
        let original = fs::metadata(&path).unwrap();
        let directory_group = fs::metadata(directory.path()).unwrap().gid();
        if let Some(group) = supplementary_groups()
            .into_iter()
            .find(|group| *group != original.gid() && *group != directory_group)
        {
            std::os::unix::fs::chown(&path, None, Some(group)).unwrap();
        }
        let before = fs::metadata(&path).unwrap();

        write_validated(&ConfigFile::new(&path), None, |_| Ok(valid_document()))
            .await
            .unwrap();

        let after = fs::metadata(&path).unwrap();
        assert_eq!(after.uid(), before.uid());
        assert_eq!(after.gid(), before.gid());
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("fake-linear-key"));
    }

    #[cfg(unix)]
    fn supplementary_groups() -> Vec<libc::gid_t> {
        // SAFETY: a zero-length query writes nothing and returns the group count.
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        let mut groups = vec![libc::gid_t::default(); usize::try_from(count).unwrap_or(0)];
        // SAFETY: `groups` has room for `count` entries.
        let filled = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        groups.truncate(usize::try_from(filled).unwrap_or(0));
        groups
    }
}
