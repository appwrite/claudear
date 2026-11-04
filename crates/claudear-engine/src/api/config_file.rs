//! Reading, redacting, validating and safely writing `claudear.toml` for the
//! dashboard config editor.
//!
//! Secrets are found by type: every [`SecretValue`](claudear_core::secret::SecretValue)
//! serializes as [`REDACTED`] inside [`with_redaction`], plain-string secrets
//! are listed in [`PLAIN_SECRET_PATHS`], and every string under a table named
//! in [`SECRET_TABLE_NAMES`] is treated as a secret.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use claudear_config::config::{
    Config, DiscordKnowledgebaseConfig, DiscordSourceConfig, GitHubCodeScanningConfig,
    GitLabConfig, HelpScoutConfig, JiraConfig, LinearConfig, SentryConfig, SlackSourceConfig,
};
use claudear_core::secret::{with_redaction, REDACTED};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};
use toml::{Table, Value};

const PLAIN_SECRET_PATHS: &[&str] = &["regression.github_token"];

const SECRET_TABLE_NAMES: &[&str] = &["env", "headers", "extra"];

const SIBLING_PREFIX: &str = ".claudear-config-";

const VALIDATION_PREFIX: &str = "claudear-config-validation-";

const READ_ONLY_MESSAGE: &str = "claudear.toml is read-only (for example a read-only bind mount); \
     edit it on the host and restart claudear";

const REVISION_MISMATCH_MESSAGE: &str =
    "claudear.toml changed on disk since it was loaded; reload and reapply";

static WRITE_LOCK: Mutex<()> = Mutex::new(());

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

/// Render `config` as TOML in struct order with every secret replaced by
/// [`REDACTED`], except unset secrets, which stay empty.
pub(super) fn redact(config: &Config) -> Result<RedactedDocument, ConfigFileError> {
    let plain = to_value(config)?;
    let mut masked = with_redaction(|| to_value(config))?;
    for path in PLAIN_SECRET_PATHS {
        if let Some(leaf) = lookup_mut(&mut masked, path) {
            mask_strings(leaf);
        }
    }
    mask_secret_tables(&mut masked);

    let mut secret_paths = Vec::new();
    mark_secrets(&plain, &mut masked, "", &mut secret_paths);

    let masked: Config = masked.try_into().map_err(|error| {
        ConfigFileError::unreadable(format!("Failed to redact config: {error}"))
    })?;
    let content = toml::to_string_pretty(&masked).map_err(|error| {
        ConfigFileError::unreadable(format!("Failed to serialize config: {error}"))
    })?;

    Ok(RedactedDocument {
        content,
        secret_paths,
    })
}

/// Replace every [`REDACTED`] sentinel in `incoming` with the value at the same
/// path in `current`. Arrays are restored element by element when both sides
/// have the same length.
pub(super) fn restore(incoming: &mut Value, current: &Value) {
    match (incoming, current) {
        (Value::Table(incoming), Value::Table(current)) => {
            for (key, value) in incoming.iter_mut() {
                if let Some(current) = current.get(key) {
                    restore(value, current);
                }
            }
        }
        (Value::Array(incoming), Value::Array(current)) if incoming.len() == current.len() => {
            for (value, current) in incoming.iter_mut().zip(current) {
                restore(value, current);
            }
        }
        (incoming @ Value::String(_), current) if incoming.as_str() == Some(REDACTED) => {
            *incoming = current.clone();
        }
        _ => {}
    }
}

/// Paths that still hold the [`REDACTED`] sentinel, as `a.b` or `a.list[0].c`.
pub(super) fn unrestored(document: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    collect_sentinels(document, "", &mut paths);
    paths
}

pub(super) fn parse(content: &str) -> Result<Config, ConfigFileError> {
    toml::from_str(content)
        .map_err(|error| ConfigFileError::invalid(format!("Invalid TOML config: {error}")))
}

/// The config as the daemon would deserialize it, without env overrides.
pub(super) fn normalize(content: &str) -> Result<Value, ConfigFileError> {
    to_value(&parse(content)?)
}

pub(super) fn revision(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The raw on-disk document; a missing file is an empty table.
pub(super) async fn read_document(path: &Path) -> Result<Value, ConfigFileError> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => toml::from_str(&content).map_err(|error| {
            ConfigFileError::unreadable(format!("Invalid on-disk config: {error}"))
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Value::Table(Table::new())),
        Err(error) => Err(ConfigFileError::unreadable(format!(
            "Failed to read on-disk config: {error}"
        ))),
    }
}

pub(super) fn writable(path: &Path) -> bool {
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => tempfile::Builder::new()
            .prefix(SIBLING_PREFIX)
            .tempfile_in(parent_directory(path))
            .is_ok(),
        Err(_) => false,
    }
}

/// Paths whose values the daemon would see differently from the file:
/// env overrides, resolved instructions and user slugs. Never values.
pub(super) fn runtime_overrides(path: &Path, disk: &Config) -> Vec<String> {
    let Ok(file) = toml::Value::try_from(disk) else {
        return Vec::new();
    };
    let Some(loaded) = Config::load(path)
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

/// Validate `document` exactly as daemon startup would (env overrides,
/// instructions files, user slugs), then write it without any of them.
pub(super) async fn write_validated(
    path: &Path,
    document: Value,
    expected_revision: Option<&str>,
) -> Result<WriteOutcome, ConfigFileError> {
    let path = path.to_path_buf();
    let expected_revision = expected_revision.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        write_validated_blocking(&path, document, expected_revision.as_deref())
    })
    .await
    .map_err(|error| ConfigFileError::unreadable(format!("Config write failed: {error}")))?
}

fn write_validated_blocking(
    path: &Path,
    document: Value,
    expected_revision: Option<&str>,
) -> Result<WriteOutcome, ConfigFileError> {
    let _lock = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    if let Some(expected) = expected_revision {
        let current = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(ConfigFileError::unreadable(format!(
                    "Failed to read on-disk config: {error}"
                )))
            }
        };
        if revision(&current) != expected {
            return Err(ConfigFileError::new(
                ConfigErrorCode::RevisionMismatch,
                REVISION_MISMATCH_MESSAGE,
            ));
        }
    }

    let placeholders = unrestored(&document);
    if !placeholders.is_empty() {
        return Err(ConfigFileError::invalid(format!(
            "Config still contains redacted placeholders for: {}. \
             Provide the actual value(s) for these fields.",
            placeholders.join(", ")
        )));
    }

    let parsed: Config = document
        .try_into()
        .map_err(|error| ConfigFileError::invalid(format!("Invalid TOML config: {error}")))?;
    let content = toml::to_string_pretty(&parsed).map_err(|error| {
        ConfigFileError::unreadable(format!("Failed to serialize config: {error}"))
    })?;

    validate_as_startup(&path, &parsed)?;

    if !writable(&path) {
        return Err(ConfigFileError::read_only());
    }
    persist(&path, &content)?;

    Ok(WriteOutcome {
        revision: revision(content.as_bytes()),
        normalized: normalize(&content)?,
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
    let content = toml::to_string_pretty(&candidate).map_err(|error| {
        ConfigFileError::unreadable(format!("Failed to serialize config: {error}"))
    })?;

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
        Ok(metadata) => fs::set_permissions(temporary.path(), metadata.permissions())?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

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
    Value::try_from(config).map_err(|error| {
        ConfigFileError::unreadable(format!("Failed to serialize config: {error}"))
    })
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

fn lookup_mut<'a>(document: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    path.split('.').try_fold(document, |value, segment| {
        value.as_table_mut()?.get_mut(segment)
    })
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
                if SECRET_TABLE_NAMES.contains(&key.as_str()) {
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

fn is_sentinel(value: &Value) -> bool {
    value.as_str() == Some(REDACTED)
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
mod tests {
    use super::*;
    use claudear_config::config::McpServerConfig;
    use claudear_core::secret::SecretValue;
    use std::path::PathBuf;

    const NON_SECRET_FIELDS: &[(&str, &str)] = &[
        ("max_tokens", "a generation length limit, not a credential"),
        (
            "private_key_path",
            "a filesystem path to the key, not the key itself",
        ),
        (
            "user_key",
            "a Pushover user identifier or a [users] slug that resolves to one",
        ),
        (
            "push_user_key",
            "a Pushover user identifier from the [users] registry",
        ),
        ("project_keys", "Jira project keys such as PROJ"),
    ];

    const PLAIN_SECRET_FIELD_NAMES: &[(&str, &str)] =
        &[("github_token", "regression.github_token")];

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
    ];

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

        restore(&mut incoming, &on_disk);

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
        restore(&mut incoming, &current);
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

        let outcome = write_validated(&path, valid_document(), Some(&expected))
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

        let error = write_validated(&path, valid_document(), Some("stale"))
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

        let error = write_validated(&path, document, None).await.unwrap_err();

        assert_eq!(error.code, ConfigErrorCode::Invalid);
        assert!(error.message.contains("issues.linear.api_key"));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn write_validated_rejects_a_config_that_fails_validation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        let document: Value = toml::from_str("workspace = \"/tmp/repos\"\n").unwrap();

        let error = write_validated(&path, document, None).await.unwrap_err();

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

        write_validated(&path, valid_document(), None)
            .await
            .unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_validated_writes_through_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("real.toml");
        let link = directory.path().join("claudear.toml");
        fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_validated(&link, valid_document(), None)
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

        assert!(!writable(&path));
        let error = write_validated(&path, valid_document(), None)
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
        assert!(writable(&directory.path().join("claudear.toml")));
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

        let overrides = runtime_overrides(&path, &parse(content).unwrap());

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
        assert!(runtime_overrides(&path, &Config::default()).is_empty());
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
}
