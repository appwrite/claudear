//! An in-memory dashboard API with one admin and one viewer session, for
//! endpoint tests that need to compare what each role sees.

use super::routes::create_api_router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use claudear_config::config::Config;
use claudear_storage::{FixAttemptTracker, SqliteTracker};
use http_body_util::BodyExt;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;

const SESSION_EXPIRY: &str = "2099-12-31 23:59:59";
pub const ADMIN_EMAIL: &str = "admin@example.com";

pub struct Harness {
    pub tracker: Arc<dyn FixAttemptTracker>,
    router: Router,
    pub admin: String,
    pub viewer: String,
}

impl Harness {
    pub fn start() -> Self {
        let tracker: Arc<dyn FixAttemptTracker> =
            Arc::new(SqliteTracker::in_memory().expect("in-memory tracker"));
        let password_hash = bcrypt::hash("test-password", 4).expect("hash");
        let admin_id = tracker
            .create_user(ADMIN_EMAIL, &password_hash, "Admin", "admin")
            .expect("admin user");
        let viewer_id = tracker
            .create_user("viewer@example.com", &password_hash, "Viewer", "viewer")
            .expect("viewer user");
        let admin = tracker
            .create_session(admin_id, SESSION_EXPIRY)
            .expect("admin session");
        let viewer = tracker
            .create_session(viewer_id, SESSION_EXPIRY)
            .expect("viewer session");
        let config = Config {
            storage_dir: std::env::temp_dir().join("claudear-api-test-storage"),
            ..Config::default()
        };
        let indexing = tracker.subscribe_indexing_progress();
        let router = create_api_router(
            config,
            tracker.clone(),
            PathBuf::from("claudear.toml"),
            indexing,
        )
        .layer(CookieManagerLayer::new());
        Self {
            tracker,
            router,
            admin,
            viewer,
        }
    }

    pub async fn get(&self, uri: &str, session: Option<&str>) -> (StatusCode, serde_json::Value) {
        self.send(request("GET", uri, session, None)).await
    }

    pub async fn post(
        &self,
        uri: &str,
        session: Option<&str>,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        self.send(request("POST", uri, session, Some(body))).await
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("response body")
            .to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }
}

fn request(method: &str, uri: &str, session: Option<&str>, body: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(session) = session {
        builder = builder.header("cookie", format!("claudear_session={session}"));
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    builder
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .expect("request")
}
