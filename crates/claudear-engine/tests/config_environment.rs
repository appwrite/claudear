//! Config saves validate the way daemon startup does, with `CLAUDEAR_*` env
//! overrides and instructions files, but never write either into the file.
//!
//! This binary holds a single test because it mutates the process environment.

#![cfg(feature = "sqlite")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use claudear_config::config::Config;
use claudear_core::identity::Role;
use claudear_engine::api::create_api_router;
use claudear_storage::{FixAttemptTracker, SqliteTracker};
use http_body_util::BodyExt;
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;

const TOKEN_VARIABLE: &str = "CLAUDEAR_GITHUB_TOKEN";

const ENV_ONLY_TOKEN: &str = "ghp_envonlyfaketoken000000000000000000";

const INSTRUCTIONS_MARKER: &str = "INLINE-MARKER";

const CONFIG_FILE: &str = r#"workspace = "/tmp/repos"

[agent.providers.claude]
instructions_file = "extra.md"

[deploy_qa]
enabled = true

[[deploy_qa.tracks]]
name = "cloud"
repo = "fake-owner/fake-repo"
"#;

struct EnvironmentVariable(&'static str);

impl EnvironmentVariable {
    fn set(name: &'static str, value: &str) -> Self {
        std::env::set_var(name, value);
        Self(name)
    }
}

impl Drop for EnvironmentVariable {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

fn admin_router(path: &Path) -> (Router, String) {
    let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
    let password_hash = bcrypt::hash("testpass", 4).unwrap();
    tracker
        .create_user("admin@test.com", &password_hash, "Admin", Role::Admin)
        .unwrap();
    let session = tracker.create_session(1, "2099-12-31 23:59:59").unwrap();
    let indexing_rx = tracker.subscribe_indexing_progress();
    let router = create_api_router(Config::default(), tracker, path.to_path_buf(), indexing_rx)
        .layer(CookieManagerLayer::new());
    (router, session)
}

fn request(method: &str, session: &str, body: Option<serde_json::Value>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri("/api/config")
        .header("cookie", format!("claudear_session={session}"));
    match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn saves_validate_with_env_overrides_without_inlining_them() {
    let _token = EnvironmentVariable::set(TOKEN_VARIABLE, ENV_ONLY_TOKEN);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claudear.toml");
    std::fs::write(&path, CONFIG_FILE).unwrap();
    std::fs::write(
        directory.path().join("extra.md"),
        format!("{INSTRUCTIONS_MARKER}\n"),
    )
    .unwrap();
    let (router, session) = admin_router(&path);

    let response = router
        .clone()
        .oneshot(request(
            "PUT",
            &session,
            Some(serde_json::json!({ "content": CONFIG_FILE })),
        ))
        .await
        .unwrap();
    let status = response.status();
    let saved = json(response).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["ok"], true);

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(!written.contains(ENV_ONLY_TOKEN), "{written}");
    assert!(!written.contains(INSTRUCTIONS_MARKER), "{written}");
    assert!(
        written.contains("instructions_file = \"extra.md\""),
        "{written}"
    );

    let response = router
        .oneshot(request("GET", &session, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let loaded = json(response).await;
    assert!(!loaded.to_string().contains(ENV_ONLY_TOKEN), "{loaded}");
    assert!(
        !loaded.to_string().contains(INSTRUCTIONS_MARKER),
        "{loaded}"
    );
    let overrides: Vec<&str> = loaded["runtime_overrides"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|path| path.as_str())
        .collect();
    assert!(overrides.contains(&"scm.github.token"), "{overrides:?}");
    assert!(
        overrides.contains(&"agent.providers.claude.instructions"),
        "{overrides:?}"
    );
}
