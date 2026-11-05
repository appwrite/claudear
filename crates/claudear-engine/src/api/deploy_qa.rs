//! Deploy QA status and settings API.
//!
//! Every logged-in user can see the `[deploy_qa]` tracks claudear is running
//! with and their recent release tips. Admins can read and rewrite the
//! `[deploy_qa]` section of `claudear.toml`; nothing reloads at runtime, so a
//! change only applies after a restart and the API reports when one is due.
//!
//! `GET /api/deploy-qa/tips` returns at most [`MAX_TIPS_PAGE`] tips however
//! large a `limit` it is asked for, and [`DEFAULT_TIPS_LIMIT`] without one;
//! `limit=0` returns none.

use super::auth::{AdminUser, AuthUser};
use super::config_file::{self, ConfigFile, ConfigFileError};
use super::routes::ApiState;
use axum::{
    extract::{rejection::JsonRejection, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use claudear_config::config::{DeployQaConfig, DeployQaTagFilter, DeployQaTrackConfig};
use claudear_core::types::{DeployQaTip, DeployQaTipStatus};
use serde::{Deserialize, Serialize};
use std::io;
use std::sync::{Mutex, PoisonError};

const DEFAULT_TIPS_LIMIT: usize = 50;

const MAX_TIPS_PAGE: usize = 100;

const DISCORD_CHANNELS_URL: &str = "https://discord.com/channels";

const SECTION: &str = "deploy_qa";

static STATUS_FAILURES: FailureLog = FailureLog::new();

const SAVED_MESSAGE: &str =
    "Saved to claudear.toml (rewritten in normalised form). Restart claudear to apply.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(super) struct TagFilterKind {
    pub kind: &'static str,
    pub requires_value: bool,
}

const ANY: TagFilterKind = TagFilterKind {
    kind: "any",
    requires_value: false,
};

const SUFFIX: TagFilterKind = TagFilterKind {
    kind: "suffix",
    requires_value: true,
};

const NOT_SUFFIX: TagFilterKind = TagFilterKind {
    kind: "not_suffix",
    requires_value: true,
};

const PREFIX: TagFilterKind = TagFilterKind {
    kind: "prefix",
    requires_value: true,
};

pub(super) const TAG_FILTER_KINDS: &[TagFilterKind] = &[ANY, SUFFIX, NOT_SUFFIX, PREFIX];

pub(super) fn kind(filter: &DeployQaTagFilter) -> &'static str {
    match filter {
        DeployQaTagFilter::Any => ANY.kind,
        DeployQaTagFilter::Suffix(_) => SUFFIX.kind,
        DeployQaTagFilter::NotSuffix(_) => NOT_SUFFIX.kind,
        DeployQaTagFilter::Prefix(_) => PREFIX.kind,
    }
}

#[derive(Debug, Serialize)]
pub(super) struct DeployQaTipView {
    id: i64,
    track: String,
    repo: String,
    tag: String,
    status: DeployQaTipStatus,
    published_at: Option<String>,
    release_url: Option<String>,
    author_login: Option<String>,
    attempt_id: Option<i64>,
    discord_message_url: Option<String>,
    discord_thread_url: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
pub(super) struct DeployQaTrackStatus {
    name: String,
    repo: String,
    tag_filter: String,
    last_seen: Option<DeployQaTipView>,
    in_flight: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct DeployQaStatus {
    enabled: bool,
    restart_required: bool,
    poll_interval_ms: u64,
    timeout_secs: u64,
    skip_if_previous_running: bool,
    discord_channel_id: Option<String>,
    tracks: Vec<DeployQaTrackStatus>,
}

#[derive(Debug, Serialize)]
pub(super) struct DeployQaTipsResponse {
    tips: Vec<DeployQaTipView>,
}

#[derive(Debug, Deserialize)]
pub(super) struct DeployQaTipsQuery {
    #[serde(default)]
    track: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct DeployQaTrackSettings {
    name: String,
    repo: String,
    tag_filter: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct DeployQaSettings {
    enabled: bool,
    poll_interval_ms: u64,
    skip_if_previous_running: bool,
    timeout_secs: u64,
    discord_channel_id: Option<String>,
    github_discord_map_path: Option<String>,
    instructions_path: Option<String>,
    tracks: Vec<DeployQaTrackSettings>,
}

#[derive(Debug, Serialize)]
pub(super) struct DeployQaConfigResponse {
    settings: DeployQaSettings,
    tag_filter_kinds: &'static [TagFilterKind],
    track_name_separator: &'static str,
    restart_required: bool,
    writable: bool,
    revision: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct DeployQaConfigUpdate {
    settings: DeployQaSettings,
    #[serde(default)]
    revision: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct DeployQaSaveResponse {
    ok: bool,
    restart_required: bool,
    revision: String,
    message: &'static str,
}

impl From<&DeployQaConfig> for DeployQaSettings {
    fn from(config: &DeployQaConfig) -> Self {
        Self {
            enabled: config.enabled,
            poll_interval_ms: config.poll_interval_ms,
            skip_if_previous_running: config.skip_if_previous_running,
            timeout_secs: config.timeout_secs,
            discord_channel_id: config.discord_channel_id.clone(),
            github_discord_map_path: config.github_discord_map_path.clone(),
            instructions_path: config.instructions_path.clone(),
            tracks: config
                .tracks
                .iter()
                .map(|track| DeployQaTrackSettings {
                    name: track.name.clone(),
                    repo: track.repo.clone(),
                    tag_filter: track.tag_filter.as_str(),
                })
                .collect(),
        }
    }
}

impl TryFrom<DeployQaSettings> for DeployQaConfig {
    type Error = ConfigFileError;

    fn try_from(settings: DeployQaSettings) -> Result<Self, Self::Error> {
        let tracks = settings
            .tracks
            .into_iter()
            .enumerate()
            .map(|(index, track)| {
                let tag_filter = DeployQaTagFilter::parse(&track.tag_filter).map_err(|error| {
                    ConfigFileError::invalid(format!("tracks[{index}].tag_filter: {error}"))
                })?;
                let kind = kind(&tag_filter);
                if !TAG_FILTER_KINDS.iter().any(|listed| listed.kind == kind) {
                    return Err(ConfigFileError::invalid(format!(
                        "tracks[{index}].tag_filter: kind '{kind}' is not offered by the editor"
                    )));
                }
                Ok(DeployQaTrackConfig {
                    name: track.name,
                    repo: track.repo,
                    tag_filter,
                })
            })
            .collect::<Result<Vec<_>, ConfigFileError>>()?;

        let config = DeployQaConfig {
            enabled: settings.enabled,
            poll_interval_ms: settings.poll_interval_ms,
            skip_if_previous_running: settings.skip_if_previous_running,
            discord_channel_id: present(settings.discord_channel_id),
            github_discord_map_path: present(settings.github_discord_map_path),
            instructions_path: present(settings.instructions_path),
            timeout_secs: settings.timeout_secs,
            tracks,
        };
        config
            .validate()
            .map_err(|error| ConfigFileError::invalid(error.to_string()))?;
        Ok(config)
    }
}

struct DiscordLinks {
    guild_id: Option<String>,
    channel_id: Option<String>,
}

impl DiscordLinks {
    fn new(state: &ApiState) -> Self {
        Self {
            guild_id: present(state.config.discord_merged().guild_id),
            channel_id: present(state.config.deploy_qa.discord_channel_id.clone()),
        }
    }

    fn message(&self, message_id: Option<&str>) -> Option<String> {
        let guild_id = self.guild_id.as_deref()?;
        let channel_id = self.channel_id.as_deref()?;
        let message_id = message_id.filter(|id| !id.is_empty())?;
        Some(format!(
            "{DISCORD_CHANNELS_URL}/{guild_id}/{channel_id}/{message_id}"
        ))
    }

    fn thread(&self, thread_id: Option<&str>) -> Option<String> {
        let guild_id = self.guild_id.as_deref()?;
        let thread_id = thread_id.filter(|id| !id.is_empty())?;
        Some(format!("{DISCORD_CHANNELS_URL}/{guild_id}/{thread_id}"))
    }

    fn view(&self, tip: DeployQaTip) -> DeployQaTipView {
        DeployQaTipView {
            discord_message_url: self.message(tip.discord_message_id.as_deref()),
            discord_thread_url: self.thread(tip.discord_thread_id.as_deref()),
            id: tip.id,
            track: tip.track,
            repo: tip.repo,
            tag: tip.tag,
            status: tip.status,
            published_at: tip.published_at,
            release_url: tip.html_url,
            author_login: tip.author_login,
            attempt_id: tip.attempt_id,
            created_at: tip.created_at,
            updated_at: tip.updated_at,
        }
    }
}

fn present(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

struct DiskSection {
    config: DeployQaConfig,
    revision: String,
}

/// The last failure reported, so a poll repeating it is not logged as new.
struct FailureLog {
    last: Mutex<Option<String>>,
}

impl FailureLog {
    const fn new() -> Self {
        Self {
            last: Mutex::new(None),
        }
    }

    /// Record `message`, returning whether it differs from the last one.
    fn record(&self, message: &str) -> bool {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if last.as_deref() == Some(message) {
            return false;
        }
        *last = Some(message.to_string());
        true
    }

    fn clear(&self) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

async fn read_disk_section(file: &ConfigFile) -> Result<DiskSection, ConfigFileError> {
    let bytes = match config_file::read_locked(file).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(ConfigFileError::unreadable(format!(
                "Failed to read config: {error}"
            )))
        }
    };
    let content = std::str::from_utf8(&bytes)
        .map_err(|error| ConfigFileError::unreadable(format!("Failed to read config: {error}")))?;
    let document: toml::Value = toml::from_str(content).map_err(|error| {
        ConfigFileError::unreadable(format!(
            "Invalid on-disk config: {}",
            config_file::describe_toml_error(&error, content)
        ))
    })?;
    Ok(DiskSection {
        config: section_config(&document)?,
        revision: config_file::revision(&bytes),
    })
}

fn section_config(document: &toml::Value) -> Result<DeployQaConfig, ConfigFileError> {
    match document.get(SECTION) {
        Some(section) => section.clone().try_into().map_err(|error| {
            ConfigFileError::unreadable(format!(
                "Invalid on-disk [{SECTION}]: {}",
                config_file::describe_toml_error(&error, "")
            ))
        }),
        None => Ok(DeployQaConfig::default()),
    }
}

fn section_value(config: &DeployQaConfig) -> Result<toml::Value, ConfigFileError> {
    toml::Value::try_from(config).map_err(|error| {
        ConfigFileError::unreadable(format!("Failed to serialize [{SECTION}]: {error}"))
    })
}

fn restart_required(state: &ApiState, disk: &DeployQaConfig) -> Result<bool, ConfigFileError> {
    Ok(section_value(&state.config.deploy_qa)? != section_value(disk)?)
}

/// `GET /api/deploy-qa` — the running `[deploy_qa]` settings and each track's
/// last-seen tip.
pub(super) async fn status_handler(
    _user: AuthUser,
    State(state): State<ApiState>,
) -> Result<Json<DeployQaStatus>, StatusCode> {
    let restart_required = match read_disk_section(&state.config_file)
        .await
        .and_then(|disk| restart_required(&state, &disk.config))
    {
        Ok(restart_required) => {
            STATUS_FAILURES.clear();
            restart_required
        }
        Err(error) => {
            if STATUS_FAILURES.record(&error.message) {
                tracing::warn!(
                    error = %error.message,
                    "Failed to compare deploy_qa with claudear.toml"
                );
            } else {
                tracing::debug!(
                    error = %error.message,
                    "Failed to compare deploy_qa with claudear.toml"
                );
            }
            false
        }
    };

    let links = DiscordLinks::new(&state);
    let running = &state.config.deploy_qa;
    let tracks = running
        .tracks
        .iter()
        .map(|track| {
            let last_seen = state
                .tracker
                .get_last_seen_deploy_qa_tip(&track.name)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let in_flight = state
                .tracker
                .track_has_in_flight_deploy_qa(&track.name)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(DeployQaTrackStatus {
                name: track.name.clone(),
                repo: track.repo.clone(),
                tag_filter: track.tag_filter.as_str(),
                last_seen: last_seen.map(|tip| links.view(tip)),
                in_flight,
            })
        })
        .collect::<Result<Vec<_>, StatusCode>>()?;

    Ok(Json(DeployQaStatus {
        enabled: running.enabled,
        restart_required,
        poll_interval_ms: running.poll_interval_ms,
        timeout_secs: running.timeout_secs,
        skip_if_previous_running: running.skip_if_previous_running,
        discord_channel_id: running.discord_channel_id.clone(),
        tracks,
    }))
}

/// `GET /api/deploy-qa/tips?track=&limit=` — recent tips, newest first.
pub(super) async fn tips_handler(
    _user: AuthUser,
    State(state): State<ApiState>,
    Query(query): Query<DeployQaTipsQuery>,
) -> Result<Json<DeployQaTipsResponse>, StatusCode> {
    let track = query.track.as_deref().filter(|track| !track.is_empty());
    let links = DiscordLinks::new(&state);
    let tips = state
        .tracker
        .list_deploy_qa_tips(track, tips_page_size(query.limit))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter()
        .map(|tip| links.view(tip))
        .collect();
    Ok(Json(DeployQaTipsResponse { tips }))
}

fn tips_page_size(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_TIPS_LIMIT).min(MAX_TIPS_PAGE)
}

/// `GET /api/deploy-qa/config` — the `[deploy_qa]` section as written in
/// `claudear.toml`, with what the editor needs to validate it.
pub(super) async fn get_config_handler(
    _admin: AdminUser,
    State(state): State<ApiState>,
) -> Result<Json<DeployQaConfigResponse>, ConfigFileError> {
    let disk = read_disk_section(&state.config_file).await?;
    let restart_required = restart_required(&state, &disk.config)?;
    let file = state.config_file.clone();
    let writable = tokio::task::spawn_blocking(move || config_file::writable(&file))
        .await
        .map_err(|error| {
            ConfigFileError::unreadable(format!("Failed to inspect config: {error}"))
        })?;

    Ok(Json(DeployQaConfigResponse {
        settings: DeployQaSettings::from(&disk.config),
        tag_filter_kinds: TAG_FILTER_KINDS,
        track_name_separator: DeployQaTip::ISSUE_ID_SEPARATOR,
        restart_required,
        writable,
        revision: disk.revision,
    }))
}

/// `PUT /api/deploy-qa/config` — validate the settings, then rewrite the
/// `[deploy_qa]` section of `claudear.toml`, leaving every other section as is.
pub(super) async fn put_config_handler(
    admin: AdminUser,
    State(state): State<ApiState>,
    body: Result<Json<DeployQaConfigUpdate>, JsonRejection>,
) -> Response {
    if !state.limiters.api.allow(&admin.0.id) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "error": "Rate limit exceeded" })),
        )
            .into_response();
    }
    let result = match body {
        Ok(Json(update)) => save(&state, update).await,
        Err(rejection) => Err(ConfigFileError::rejected(&rejection)),
    };
    match result {
        Ok(saved) => Json(saved).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn save(
    state: &ApiState,
    update: DeployQaConfigUpdate,
) -> Result<DeployQaSaveResponse, ConfigFileError> {
    let config = DeployQaConfig::try_from(update.settings)?;
    let section = section_value(&config)?;
    let outcome = config_file::write_validated(
        &state.config_file,
        update.revision.as_deref(),
        move |mut document| {
            let table = document.as_table_mut().ok_or_else(|| {
                ConfigFileError::unreadable("Invalid on-disk config: the document is not a table")
            })?;
            table.insert(SECTION.to_string(), section);
            Ok(document)
        },
    )
    .await?;
    let written = section_config(&outcome.normalized)?;

    Ok(DeployQaSaveResponse {
        ok: true,
        restart_required: restart_required(state, &written)?,
        revision: outcome.revision,
        message: SAVED_MESSAGE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_api_router;
    use axum::body::Body;
    use axum::http::Request;
    use claudear_config::config::Config;
    use claudear_core::identity::Role;
    use claudear_storage::{FixAttemptTracker, SqliteTracker};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;
    use tower_cookies::CookieManagerLayer;

    const FAKE_GITHUB_TOKEN: &str = "ghp_fake_deploy_qa_token";

    const GUILD_ID: &str = "111";

    const CHANNEL_ID: &str = "222";

    fn disk_content() -> String {
        format!(
            "workspace = \"/tmp/repos\"\n\n\
             [scm.github]\ntoken = \"{FAKE_GITHUB_TOKEN}\"\n\n\
             [notifiers.discord]\nguild_id = \"{GUILD_ID}\"\n\n\
             [deploy_qa]\nenabled = true\ndiscord_channel_id = \"{CHANNEL_ID}\"\n\n\
             [[deploy_qa.tracks]]\nname = \"cloud\"\nrepo = \"acme/cloud\"\n"
        )
    }

    fn running_config() -> Config {
        let disk: Config = toml::from_str(&disk_content()).unwrap();
        let mut config = Config {
            deploy_qa: disk.deploy_qa,
            ..Config::default()
        };
        config.notifiers.discord.guild_id = Some(GUILD_ID.to_string());
        config
    }

    struct TestApp {
        router: axum::Router,
        tracker: Arc<dyn FixAttemptTracker>,
        token: String,
        path: PathBuf,
        _directory: tempfile::TempDir,
    }

    fn app(role: Role) -> TestApp {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claudear.toml");
        std::fs::write(&path, disk_content()).unwrap();

        let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let password_hash = bcrypt::hash("testpass", 4).unwrap();
        let user_id = tracker
            .create_user("deploy-qa@example.com", &password_hash, "Deploy QA", role)
            .unwrap();
        let token = tracker
            .create_session(user_id, "2099-12-31 23:59:59")
            .unwrap();

        let mut config = running_config();
        config.storage_dir = directory.path().join("storage");
        let router = create_api_router(
            config,
            tracker.clone(),
            path.clone(),
            tracker.subscribe_indexing_progress(),
        )
        .layer(CookieManagerLayer::new());

        TestApp {
            router,
            tracker,
            token,
            path,
            _directory: directory,
        }
    }

    impl TestApp {
        async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
            let response = self.router.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
            (status, json)
        }

        async fn get(&self, uri: &str) -> (StatusCode, Value) {
            self.send(
                Request::builder()
                    .uri(uri)
                    .header("cookie", format!("claudear_session={}", self.token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
        }

        async fn put(&self, uri: &str, body: Value) -> (StatusCode, Value) {
            self.send(
                Request::builder()
                    .method("PUT")
                    .uri(uri)
                    .header("cookie", format!("claudear_session={}", self.token))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
        }

        async fn put_settings(&self, settings: Value) -> (StatusCode, Value) {
            self.put("/api/deploy-qa/config", json!({ "settings": settings }))
                .await
        }
    }

    fn settings_with_tracks(tracks: Value) -> Value {
        json!({
            "enabled": true,
            "poll_interval_ms": 60000,
            "skip_if_previous_running": false,
            "timeout_secs": 900,
            "discord_channel_id": CHANNEL_ID,
            "github_discord_map_path": null,
            "instructions_path": null,
            "tracks": tracks,
        })
    }

    fn track(name: &str, repo: &str, tag_filter: &str) -> DeployQaTrackConfig {
        DeployQaTrackConfig {
            name: name.to_string(),
            repo: repo.to_string(),
            tag_filter: DeployQaTagFilter::parse(tag_filter).unwrap(),
        }
    }

    fn validation_error(tracks: Vec<DeployQaTrackConfig>) -> String {
        DeployQaConfig {
            tracks,
            ..DeployQaConfig::default()
        }
        .validate()
        .unwrap_err()
        .to_string()
    }

    fn samples() -> Vec<DeployQaTagFilter> {
        let samples = vec![
            DeployQaTagFilter::Any,
            DeployQaTagFilter::Suffix("-db".to_string()),
            DeployQaTagFilter::NotSuffix("-db".to_string()),
            DeployQaTagFilter::Prefix("apps/console/".to_string()),
        ];
        for sample in &samples {
            match sample {
                DeployQaTagFilter::Any
                | DeployQaTagFilter::Suffix(_)
                | DeployQaTagFilter::NotSuffix(_)
                | DeployQaTagFilter::Prefix(_) => {}
            }
        }
        samples
    }

    #[test]
    fn every_tag_filter_has_a_listed_kind() {
        for sample in samples() {
            let kind = kind(&sample);
            assert!(
                TAG_FILTER_KINDS.iter().any(|listed| listed.kind == kind),
                "{sample:?} has kind '{kind}', which is missing from TAG_FILTER_KINDS"
            );
        }
    }

    #[test]
    fn every_listed_kind_parses_back_to_itself() {
        for listed in TAG_FILTER_KINDS {
            let raw = if listed.requires_value {
                format!("{}:x", listed.kind)
            } else {
                listed.kind.to_string()
            };
            let filter = DeployQaTagFilter::parse(&raw)
                .unwrap_or_else(|error| panic!("'{raw}' does not parse: {error}"));
            assert_eq!(kind(&filter), listed.kind, "'{raw}' parsed as {filter:?}");
            if listed.requires_value {
                assert!(
                    DeployQaTagFilter::parse(&format!("{}:", listed.kind)).is_err(),
                    "'{}' requires a value but parses without one",
                    listed.kind
                );
            }
        }
    }

    #[tokio::test]
    async fn viewer_reads_status_and_tips_but_not_config() {
        let app = app(Role::Viewer);

        assert_eq!(app.get("/api/deploy-qa").await.0, StatusCode::OK);
        assert_eq!(app.get("/api/deploy-qa/tips").await.0, StatusCode::OK);
        assert_eq!(
            app.get("/api/deploy-qa/config").await.0,
            StatusCode::FORBIDDEN
        );
        let before = std::fs::read_to_string(&app.path).unwrap();
        assert_eq!(
            app.put_settings(settings_with_tracks(json!([]))).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(std::fs::read_to_string(&app.path).unwrap(), before);
    }

    #[tokio::test]
    async fn unauthenticated_requests_are_rejected() {
        let app = app(Role::Admin);
        for (method, uri) in [
            ("GET", "/api/deploy-qa"),
            ("GET", "/api/deploy-qa/tips"),
            ("GET", "/api/deploy-qa/config"),
            ("PUT", "/api/deploy-qa/config"),
        ] {
            let (status, _) = app
                .send(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({ "settings": settings_with_tracks(json!([])) }).to_string(),
                        ))
                        .unwrap(),
                )
                .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        }
    }

    #[tokio::test]
    async fn status_reports_running_tracks_with_last_seen_tip() {
        let app = app(Role::Viewer);
        let mut tip = DeployQaTip::new("cloud", "acme/cloud", "1.2.0");
        tip.published_at = Some("2026-03-01T00:00:00Z".to_string());
        tip.release_body = Some("SECRET-RELEASE-BODY".to_string());
        app.tracker.upsert_deploy_qa_tip(&tip).unwrap();

        let (status, body) = app.get("/api/deploy-qa").await;

        assert_eq!(status, StatusCode::OK);
        assert!(!body.to_string().contains("SECRET-RELEASE-BODY"));
        assert_eq!(body["enabled"], true);
        assert_eq!(body["restart_required"], false);
        assert_eq!(body["poll_interval_ms"], 300_000);
        assert_eq!(body["timeout_secs"], 1800);
        assert_eq!(body["skip_if_previous_running"], true);
        assert_eq!(body["discord_channel_id"], CHANNEL_ID);
        let tracks = body["tracks"].as_array().unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0]["name"], "cloud");
        assert_eq!(tracks[0]["repo"], "acme/cloud");
        assert_eq!(tracks[0]["tag_filter"], "any");
        assert_eq!(tracks[0]["in_flight"], true);
        assert_eq!(tracks[0]["last_seen"]["tag"], "1.2.0");
        assert_eq!(tracks[0]["last_seen"]["status"], "pending");
    }

    #[tokio::test]
    async fn tips_are_newest_first_with_links_and_without_release_body() {
        let app = app(Role::Viewer);
        for (track, tag, published_at) in [
            ("cloud", "1.0.0", "2026-01-01T00:00:00Z"),
            ("edge", "2.0.0", "2026-02-01T00:00:00Z"),
            ("cloud", "1.1.0", "2026-03-01T00:00:00Z"),
        ] {
            let mut tip = DeployQaTip::new(track, format!("acme/{track}"), tag);
            tip.published_at = Some(published_at.to_string());
            tip.html_url = Some(format!("https://github.com/acme/{track}/releases/{tag}"));
            tip.release_body = Some("SECRET-RELEASE-BODY".to_string());
            app.tracker.upsert_deploy_qa_tip(&tip).unwrap();
        }
        let newest = app
            .tracker
            .get_deploy_qa_tip("cloud", "1.1.0")
            .unwrap()
            .unwrap();
        app.tracker
            .update_deploy_qa_discord_ids(newest.id, Some("333"), Some("444"))
            .unwrap();
        app.tracker
            .update_deploy_qa_tip_status(newest.id, DeployQaTipStatus::Verified, Some(7))
            .unwrap();

        let tags = |body: &Value| -> Vec<String> {
            body["tips"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tip| tip["tag"].as_str().unwrap().to_string())
                .collect()
        };

        let (status, body) = app.get("/api/deploy-qa/tips").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(tags(&body), ["1.1.0", "2.0.0", "1.0.0"]);
        assert!(!body.to_string().contains("SECRET-RELEASE-BODY"));
        assert!(!body.to_string().contains("release_body"));
        let first = &body["tips"][0];
        assert_eq!(first["status"], "verified");
        assert_eq!(first["attempt_id"], 7);
        assert_eq!(
            first["release_url"],
            "https://github.com/acme/cloud/releases/1.1.0"
        );
        assert_eq!(
            first["discord_message_url"],
            format!("https://discord.com/channels/{GUILD_ID}/{CHANNEL_ID}/333")
        );
        assert_eq!(
            first["discord_thread_url"],
            format!("https://discord.com/channels/{GUILD_ID}/444")
        );
        let second = &body["tips"][1];
        assert!(second["discord_message_url"].is_null());
        assert!(second["discord_thread_url"].is_null());
        assert!(second["attempt_id"].is_null());
        assert!(second["author_login"].is_null());

        let (_, body) = app.get("/api/deploy-qa/tips?track=cloud").await;
        assert_eq!(tags(&body), ["1.1.0", "1.0.0"]);
        let (_, body) = app.get("/api/deploy-qa/tips?track=").await;
        assert_eq!(tags(&body), ["1.1.0", "2.0.0", "1.0.0"]);
        let (_, body) = app.get("/api/deploy-qa/tips?limit=2").await;
        assert_eq!(tags(&body), ["1.1.0", "2.0.0"]);
        let (_, body) = app.get("/api/deploy-qa/tips?track=cloud&limit=1").await;
        assert_eq!(tags(&body), ["1.1.0"]);
    }

    #[tokio::test]
    async fn tips_pages_are_bounded_however_large_a_limit_is_asked_for() {
        let app = app(Role::Viewer);
        for index in 0..MAX_TIPS_PAGE + 20 {
            let tip = DeployQaTip::new("cloud", "acme/cloud", format!("1.0.{index}"));
            app.tracker.upsert_deploy_qa_tip(&tip).unwrap();
        }
        let page_length = |body: &Value| body["tips"].as_array().unwrap().len();

        for uri in [
            "/api/deploy-qa/tips?limit=1000000",
            "/api/deploy-qa/tips?limit=18446744073709551615",
        ] {
            let (status, body) = app.get(uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(page_length(&body), MAX_TIPS_PAGE, "{uri}");
        }
        let (_, body) = app.get("/api/deploy-qa/tips").await;
        assert_eq!(page_length(&body), DEFAULT_TIPS_LIMIT);
        let (_, body) = app.get("/api/deploy-qa/tips?limit=0").await;
        assert_eq!(page_length(&body), 0);
    }

    #[test]
    fn tips_page_size_never_asks_storage_for_more_than_a_page() {
        assert_eq!(tips_page_size(Some(1_000_000)), MAX_TIPS_PAGE);
        assert_eq!(tips_page_size(Some(usize::MAX)), MAX_TIPS_PAGE);
        assert_eq!(tips_page_size(Some(MAX_TIPS_PAGE)), MAX_TIPS_PAGE);
        assert_eq!(tips_page_size(Some(7)), 7);
        assert_eq!(tips_page_size(Some(0)), 0);
        assert_eq!(tips_page_size(None), DEFAULT_TIPS_LIMIT);
    }

    #[test]
    fn discord_links_are_null_without_a_guild_or_channel() {
        let mut tip = DeployQaTip::new("cloud", "acme/cloud", "1.0.0");
        tip.discord_message_id = Some("333".to_string());
        tip.discord_thread_id = Some("444".to_string());
        let links = |guild_id: Option<&str>, channel_id: Option<&str>| DiscordLinks {
            guild_id: guild_id.map(str::to_string),
            channel_id: channel_id.map(str::to_string),
        };

        let view = links(None, Some(CHANNEL_ID)).view(tip.clone());
        assert!(view.discord_message_url.is_none());
        assert!(view.discord_thread_url.is_none());

        let view = links(Some(GUILD_ID), None).view(tip);
        assert!(view.discord_message_url.is_none());
        assert_eq!(
            view.discord_thread_url.as_deref(),
            Some(format!("https://discord.com/channels/{GUILD_ID}/444").as_str())
        );
    }

    #[tokio::test]
    async fn admin_reads_the_on_disk_section() {
        let app = app(Role::Admin);

        let (status, body) = app.get("/api/deploy-qa/config").await;

        assert_eq!(status, StatusCode::OK);
        assert!(!body.to_string().contains(FAKE_GITHUB_TOKEN));
        assert_eq!(body["restart_required"], false);
        assert_eq!(body["writable"], true);
        assert_eq!(
            body["revision"],
            config_file::revision(disk_content().as_bytes())
        );
        assert_eq!(
            body["track_name_separator"],
            DeployQaTip::ISSUE_ID_SEPARATOR
        );
        assert_eq!(
            body["tag_filter_kinds"],
            serde_json::to_value(TAG_FILTER_KINDS).unwrap()
        );
        assert_eq!(body["tag_filter_kinds"][1]["kind"], SUFFIX.kind);
        assert_eq!(body["tag_filter_kinds"][1]["requires_value"], true);
        assert_eq!(
            body["settings"],
            json!({
                "enabled": true,
                "poll_interval_ms": 300000,
                "skip_if_previous_running": true,
                "timeout_secs": 1800,
                "discord_channel_id": CHANNEL_ID,
                "github_discord_map_path": null,
                "instructions_path": null,
                "tracks": [{ "name": "cloud", "repo": "acme/cloud", "tag_filter": "any" }],
            })
        );
    }

    #[tokio::test]
    async fn admin_save_rewrites_the_section_and_requires_a_restart() {
        let app = app(Role::Admin);
        let (_, loaded) = app.get("/api/deploy-qa/config").await;
        let settings = settings_with_tracks(json!([
            { "name": "cloud", "repo": "acme/cloud", "tag_filter": "not_suffix:-db" },
            { "name": "cloud-db", "repo": "acme/cloud", "tag_filter": "suffix:-db" },
        ]));

        let (status, saved) = app
            .put(
                "/api/deploy-qa/config",
                json!({ "settings": settings, "revision": loaded["revision"] }),
            )
            .await;

        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["ok"], true);
        assert_eq!(saved["restart_required"], true);
        assert_eq!(saved["message"], SAVED_MESSAGE);
        let on_disk = std::fs::read_to_string(&app.path).unwrap();
        assert_eq!(saved["revision"], config_file::revision(on_disk.as_bytes()));
        assert!(on_disk.contains(FAKE_GITHUB_TOKEN), "{on_disk}");

        let written: Config = toml::from_str(&on_disk).unwrap();
        assert_eq!(written.workspace, PathBuf::from("/tmp/repos"));
        assert_eq!(
            written.notifiers.discord.guild_id.as_deref(),
            Some(GUILD_ID)
        );
        assert_eq!(written.deploy_qa.poll_interval_ms, 60000);
        assert_eq!(written.deploy_qa.timeout_secs, 900);
        assert!(!written.deploy_qa.skip_if_previous_running);
        assert_eq!(
            written.deploy_qa.tracks[1].tag_filter,
            DeployQaTagFilter::Suffix("-db".to_string())
        );
        assert_eq!(written.deploy_qa.tracks.len(), 2);

        let (_, status) = app.get("/api/deploy-qa").await;
        assert_eq!(status["restart_required"], true);
        assert_eq!(status["poll_interval_ms"], 300_000);
        let (_, reloaded) = app.get("/api/deploy-qa/config").await;
        assert_eq!(reloaded["restart_required"], true);
        assert_eq!(reloaded["settings"], settings);
        assert_eq!(reloaded["revision"], saved["revision"]);
    }

    #[tokio::test]
    async fn saving_the_running_settings_needs_no_restart() {
        let app = app(Role::Admin);
        let (_, loaded) = app.get("/api/deploy-qa/config").await;

        let (status, saved) = app
            .put(
                "/api/deploy-qa/config",
                json!({ "settings": loaded["settings"], "revision": loaded["revision"] }),
            )
            .await;

        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["restart_required"], false);
    }

    #[tokio::test]
    async fn invalid_tracks_are_rejected_with_the_config_message() {
        let app = app(Role::Admin);
        let before = std::fs::read_to_string(&app.path).unwrap();
        let cases = [
            (
                json!([
                    { "name": "cloud", "repo": "acme/cloud", "tag_filter": "any" },
                    { "name": "cloud", "repo": "acme/edge", "tag_filter": "any" },
                ]),
                validation_error(vec![
                    track("cloud", "acme/cloud", "any"),
                    track("cloud", "acme/edge", "any"),
                ]),
            ),
            (
                json!([{ "name": "cl:oud", "repo": "acme/cloud", "tag_filter": "any" }]),
                validation_error(vec![track("cl:oud", "acme/cloud", "any")]),
            ),
            (
                json!([{ "name": "", "repo": "acme/cloud", "tag_filter": "any" }]),
                validation_error(vec![track("", "acme/cloud", "any")]),
            ),
            (
                json!([{ "name": "cloud", "repo": "acme/cloud/extra", "tag_filter": "any" }]),
                validation_error(vec![track("cloud", "acme/cloud/extra", "any")]),
            ),
            (
                json!([{ "name": "cloud", "repo": "acme/cloud", "tag_filter": "suffix:" }]),
                DeployQaTagFilter::parse("suffix:").unwrap_err(),
            ),
        ];

        for (tracks, expected) in cases {
            let (status, body) = app.put_settings(settings_with_tracks(tracks)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(body["code"], "invalid");
            let error = body["error"].as_str().unwrap();
            assert!(error.contains(&expected), "'{error}' lacks '{expected}'");
        }
        assert_eq!(std::fs::read_to_string(&app.path).unwrap(), before);
    }

    #[tokio::test]
    async fn disabled_settings_are_still_validated() {
        let app = app(Role::Admin);
        let mut settings = settings_with_tracks(json!([
            { "name": "", "repo": "acme/cloud", "tag_filter": "any" },
        ]));
        settings["enabled"] = json!(false);

        let (status, body) = app.put_settings(settings).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        let expected = validation_error(vec![track("", "acme/cloud", "any")]);
        assert!(body["error"].as_str().unwrap().contains(&expected));
    }

    #[tokio::test]
    async fn a_stale_revision_is_rejected() {
        let app = app(Role::Admin);
        let before = std::fs::read_to_string(&app.path).unwrap();

        let (status, body) = app
            .put(
                "/api/deploy-qa/config",
                json!({ "settings": settings_with_tracks(json!([])), "revision": "stale" }),
            )
            .await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "revision_mismatch");
        assert_eq!(std::fs::read_to_string(&app.path).unwrap(), before);
    }

    #[tokio::test]
    async fn a_malformed_body_is_invalid() {
        let app = app(Role::Admin);

        let (status, body) = app
            .put(
                "/api/deploy-qa/config",
                json!({ "settings": { "enabled": "yes" } }),
            )
            .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid");
        assert!(body["error"].is_string());
    }

    const UNQUOTED_SECRET_FILE: &str =
        "workspace = \"/tmp/repos\"\n[scm.github]\ntoken = fake-secret-unquoted\n";

    #[tokio::test]
    async fn an_unparseable_disk_file_is_reported_without_its_lines() {
        let app = app(Role::Admin);
        std::fs::write(&app.path, UNQUOTED_SECRET_FILE).unwrap();

        let (status, body) = app.get("/api/deploy-qa/config").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["code"], "unreadable");
        assert!(!body.to_string().contains("fake-secret-unquoted"), "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("(line 3)"),
            "{body}"
        );

        let (status, body) = app.put_settings(settings_with_tracks(json!([]))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!body.to_string().contains("fake-secret-unquoted"), "{body}");
        assert_eq!(
            std::fs::read_to_string(&app.path).unwrap(),
            UNQUOTED_SECRET_FILE
        );

        let (status, body) = app.get("/api/deploy-qa").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["restart_required"], false);
    }

    #[tokio::test]
    async fn an_invalid_disk_section_is_reported_without_its_values() {
        let app = app(Role::Admin);
        let content = disk_content().replace(
            "repo = \"acme/cloud\"\n",
            "repo = \"acme/cloud\"\ntag_filter = \"fake-secret-filter\"\n",
        );
        std::fs::write(&app.path, &content).unwrap();

        let (status, body) = app.get("/api/deploy-qa/config").await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!body.to_string().contains("fake-secret-filter"), "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("tag_filter"),
            "{body}"
        );
    }

    #[test]
    fn failure_log_reports_each_new_failure_once() {
        let log = FailureLog::new();
        assert!(log.record("unreadable"));
        assert!(!log.record("unreadable"));
        assert!(log.record("invalid"));
        assert!(log.record("unreadable"));
        log.clear();
        assert!(log.record("unreadable"));
    }

    #[tokio::test]
    async fn config_read_waits_for_an_in_progress_write() {
        let app = app(Role::Admin);
        let held = config_file::HeldWriteLock::acquire();

        let request = tokio::spawn(
            app.router.clone().oneshot(
                Request::builder()
                    .uri("/api/deploy-qa/config")
                    .header("cookie", format!("claudear_session={}", app.token))
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !request.is_finished(),
            "GET /api/deploy-qa/config read the file during a write"
        );

        held.release();
        assert_eq!(request.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_save_keeps_a_change_written_while_it_waited() {
        let app = app(Role::Admin);
        let held = config_file::HeldWriteLock::acquire();

        let request = tokio::spawn(
            app.router.clone().oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/deploy-qa/config")
                    .header("cookie", format!("claudear_session={}", app.token))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "settings": settings_with_tracks(json!([
                            { "name": "edge", "repo": "acme/edge", "tag_filter": "any" },
                        ])) })
                        .to_string(),
                    ))
                    .unwrap(),
            ),
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::fs::write(
            &app.path,
            disk_content().replace("/tmp/repos", "/tmp/changed-meanwhile"),
        )
        .unwrap();
        held.release();

        assert_eq!(request.await.unwrap().unwrap().status(), StatusCode::OK);
        let written: Config = toml::from_str(&std::fs::read_to_string(&app.path).unwrap()).unwrap();
        assert_eq!(written.workspace, PathBuf::from("/tmp/changed-meanwhile"));
        assert_eq!(written.deploy_qa.tracks[0].name, "edge");
    }
}
