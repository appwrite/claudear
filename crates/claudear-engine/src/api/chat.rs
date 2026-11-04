//! Chat wiring for the dashboard API: state construction and the session
//! guard layered over the integrations chat router. The guard hands the
//! signed-in user to the chat handlers as a [`ChatViewer`], which they scope
//! every session to; see `claudear_integrations::chat::routes` for the policy.

use super::auth::AuthUser;
use super::routes::ApiState;
use axum::extract::Request;
use axum::http::uri::Authority;
use axum::http::{header, StatusCode, Uri};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use claudear_analysis::feedback::EmbeddingClient;
use claudear_analysis::repo::code_index::CodeSearchService;
use claudear_config::config::Config;
use claudear_core::types::ChatViewer;
use claudear_integrations::chat::{ChatService, ChatState};
use claudear_storage::FixAttemptTracker;
use serde::Serialize;
use std::sync::Arc;

const STATUS_PATH: &str = "/api/chat/status";
const WEBSOCKET_PATH: &str = "/api/chat/ws";
const X_FORWARDED_HOST: &str = "x-forwarded-host";
const FORWARDED_HOST_PARAMETER: &str = "host";

/// Build the chat state when `[chat]` and `[llm]` are enabled and an
/// embedding client is available for code search.
pub fn chat_state(
    config: &Config,
    tracker: Arc<dyn FixAttemptTracker>,
    embedding_client: Option<Arc<EmbeddingClient>>,
) -> Option<ChatState> {
    if !config.chat.enabled {
        return None;
    }
    if !config.llm.enabled {
        tracing::warn!("[chat] is enabled but [llm] is not; chat is disabled");
        return None;
    }
    let Some(embedding_client) = embedding_client else {
        tracing::warn!("[chat] is enabled but no embedding client is available; chat is disabled");
        return None;
    };

    let search = CodeSearchService::new(tracker.clone(), embedding_client);
    let chat_service = ChatService::new(
        config.chat.clone(),
        config.llm.clone(),
        search,
        tracker.clone(),
    );
    Some(ChatState {
        chat_service: Arc::new(chat_service),
        tracker,
    })
}

#[derive(Clone, Copy, Debug, Serialize)]
struct ChatStatus {
    enabled: bool,
}

/// Merge `chat` into `routes` with a dashboard session required on every
/// chat route, and report whether it was mounted at `STATUS_PATH`, which is
/// mounted either way so the dashboard can check before probing chat routes.
pub(super) fn mount(routes: Router, chat: Option<Router>, state: ApiState) -> Router {
    let status = ChatStatus {
        enabled: chat.is_some(),
    };
    let routes = routes.merge(
        Router::new()
            .route(
                STATUS_PATH,
                get(move |_user: AuthUser| async move { Json(status) }),
            )
            .with_state(state.clone()),
    );
    match chat {
        Some(chat) => routes.merge(chat.route_layer(from_fn_with_state(state, require_session))),
        None => routes,
    }
}

/// Reject requests without a session, and cross-origin websocket upgrades,
/// and pass the signed-in user on to the chat handlers.
async fn require_session(
    user: AuthUser,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if request.uri().path() == WEBSOCKET_PATH && !is_same_origin(&request) {
        return Err(StatusCode::FORBIDDEN);
    }
    request.extensions_mut().insert(ChatViewer {
        user_id: user.id,
        is_admin: user.is_admin(),
    });
    Ok(next.run(request).await)
}

/// A request without `Origin` is not from a browser page and passes; one with
/// it must name the host the request was sent to.
fn is_same_origin(request: &Request) -> bool {
    let headers = request.headers();
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Some(origin) = origin
        .to_str()
        .ok()
        .and_then(|origin| origin.parse::<Uri>().ok())
        .and_then(|uri| uri.authority().cloned())
    else {
        return false;
    };

    let header_value = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let x_forwarded_host = header_value(X_FORWARDED_HOST)
        .and_then(|value| value.split(',').next())
        .map(str::trim);
    let forwarded = header_value(header::FORWARDED.as_str()).and_then(forwarded_host);
    let target = request.uri().authority().map(Authority::as_str);

    [
        header_value(header::HOST.as_str()),
        x_forwarded_host,
        forwarded,
        target,
    ]
    .into_iter()
    .flatten()
    .any(|host| host.eq_ignore_ascii_case(origin.as_str()))
}

/// The `host` parameter of the first, client-side element of an RFC 7239
/// `Forwarded` header.
fn forwarded_host(value: &str) -> Option<&str> {
    value
        .split(',')
        .next()?
        .split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case(FORWARDED_HOST_PARAMETER))
        .map(|(_, host)| host.trim().trim_matches('"'))
        .filter(|host| !host.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{apply_dashboard_middleware, create_api_router_full};
    use axum::body::Body;
    use claudear_analysis::feedback::EmbeddingConfig;
    use claudear_config::config::{ChatConfig, LlmModelConfig};
    use claudear_integrations::chat::{create_chat_router, CHAT_SESSION_NOT_FOUND};
    use claudear_storage::SqliteTracker;
    use futures::{SinkExt, StreamExt};
    use serde_json::Value;
    use std::path::PathBuf;
    use std::time::Instant;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;
    use tower::ServiceExt;

    const ROUTES: &[&str] = &[
        "/api/chat/ws",
        "/api/chat/models",
        "/api/chat/sessions",
        "/api/chat/sessions/session-1",
    ];

    fn tracker() -> Arc<dyn FixAttemptTracker> {
        Arc::new(SqliteTracker::in_memory().unwrap())
    }

    fn session(tracker: &Arc<dyn FixAttemptTracker>) -> String {
        let password_hash = bcrypt::hash("testpass", 4).unwrap();
        let user_id = tracker
            .create_user("viewer@example.com", &password_hash, "Viewer", "viewer")
            .unwrap();
        tracker
            .create_session(user_id, "2099-12-31 23:59:59")
            .unwrap()
    }

    fn enabled_config() -> Config {
        let mut config = Config::default();
        config.chat.enabled = true;
        config.llm.enabled = true;
        config
    }

    fn api_state(tracker: &Arc<dyn FixAttemptTracker>) -> ApiState {
        let config = Config::default();
        ApiState {
            storage_dir: config.storage_dir.clone(),
            config,
            tracker: tracker.clone(),
            start_time: Instant::now(),
            config_path: PathBuf::from("claudear.toml"),
            indexing_rx: tracker.subscribe_indexing_progress(),
            chat_service: None,
            startup_config: None,
        }
    }

    fn stub_router(tracker: &Arc<dyn FixAttemptTracker>) -> Router {
        let chat = Router::new()
            .route("/api/chat/ws", get(|| async { StatusCode::OK }))
            .route("/api/chat/models", get(|| async { StatusCode::OK }))
            .route("/api/chat/sessions", get(|| async { StatusCode::OK }))
            .route(
                "/api/chat/sessions/{id}",
                get(|| async { StatusCode::OK }).delete(|| async { StatusCode::OK }),
            );
        apply_dashboard_middleware(mount(Router::new(), Some(chat), api_state(tracker)), false)
    }

    async fn get_status(router: &Router, uri: &str, headers: &[(&str, &str)]) -> StatusCode {
        let mut request = axum::http::Request::builder().uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn every_chat_route_requires_a_session() {
        let tracker = tracker();
        let token = session(&tracker);
        let router = stub_router(&tracker);
        let cookie = format!("claudear_session={token}");

        for route in ROUTES {
            assert_eq!(
                get_status(&router, route, &[]).await,
                StatusCode::UNAUTHORIZED,
                "{route} without a session"
            );
            assert_eq!(
                get_status(&router, route, &[("cookie", "claudear_session=forged")]).await,
                StatusCode::UNAUTHORIZED,
                "{route} with an unknown session"
            );
            assert_eq!(
                get_status(&router, route, &[("cookie", &cookie)]).await,
                StatusCode::OK,
                "{route} with a session"
            );
        }
    }

    #[tokio::test]
    async fn cross_origin_websocket_is_forbidden() {
        let tracker = tracker();
        let token = session(&tracker);
        let router = stub_router(&tracker);
        let cookie = format!("claudear_session={token}");

        let status = get_status(
            &router,
            WEBSOCKET_PATH,
            &[
                ("cookie", &cookie),
                ("host", "127.0.0.1:3199"),
                ("origin", "http://evil.example"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let status = get_status(
            &router,
            WEBSOCKET_PATH,
            &[
                ("cookie", &cookie),
                ("host", "127.0.0.1:3199"),
                ("origin", "http://127.0.0.1:3199"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn cross_origin_rest_route_is_not_origin_checked() {
        let tracker = tracker();
        let token = session(&tracker);
        let router = stub_router(&tracker);
        let cookie = format!("claudear_session={token}");

        let status = get_status(
            &router,
            "/api/chat/sessions",
            &[
                ("cookie", &cookie),
                ("host", "127.0.0.1:3199"),
                ("origin", "http://evil.example"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    async fn chat_status(router: &Router, headers: &[(&str, &str)]) -> (StatusCode, String) {
        let mut request = axum::http::Request::builder().uri(STATUS_PATH);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn status_reports_whether_chat_is_mounted() {
        let tracker = tracker();
        let token = session(&tracker);
        let cookie = format!("claudear_session={token}");
        let without_chat =
            apply_dashboard_middleware(mount(Router::new(), None, api_state(&tracker)), false);

        assert_eq!(
            chat_status(&without_chat, &[("cookie", &cookie)]).await,
            (StatusCode::OK, r#"{"enabled":false}"#.to_string())
        );
        assert_eq!(
            chat_status(&stub_router(&tracker), &[("cookie", &cookie)]).await,
            (StatusCode::OK, r#"{"enabled":true}"#.to_string())
        );
    }

    #[tokio::test]
    async fn status_is_mounted_by_the_api_router_without_chat() {
        let storage = tempfile::tempdir().unwrap();
        let tracker = tracker();
        let token = session(&tracker);
        let mut config = enabled_config();
        config.storage_dir = storage.path().to_path_buf();
        let router = apply_dashboard_middleware(
            create_api_router_full(
                config,
                tracker.clone(),
                PathBuf::from("claudear.toml"),
                tracker.subscribe_indexing_progress(),
                None,
                None,
            ),
            false,
        );
        let cookie = format!("claudear_session={token}");

        assert_eq!(
            chat_status(&router, &[("cookie", &cookie)]).await,
            (StatusCode::OK, r#"{"enabled":false}"#.to_string())
        );
    }

    #[tokio::test]
    async fn status_requires_a_session() {
        let tracker = tracker();
        for router in [
            apply_dashboard_middleware(mount(Router::new(), None, api_state(&tracker)), false),
            stub_router(&tracker),
        ] {
            assert_eq!(chat_status(&router, &[]).await.0, StatusCode::UNAUTHORIZED);
            assert_eq!(
                chat_status(&router, &[("cookie", "claudear_session=forged")])
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
        }
    }

    #[tokio::test]
    async fn mount_without_chat_adds_no_chat_routes() {
        let tracker = tracker();
        let token = session(&tracker);
        let router =
            apply_dashboard_middleware(mount(Router::new(), None, api_state(&tracker)), false);
        let cookie = format!("claudear_session={token}");

        for route in ROUTES {
            assert_eq!(
                get_status(&router, route, &[("cookie", &cookie)]).await,
                StatusCode::NOT_FOUND,
                "{route} without chat"
            );
        }
    }

    #[tokio::test]
    async fn websocket_behind_a_host_rewriting_proxy_needs_a_forwarded_host() {
        let tracker = tracker();
        let token = session(&tracker);
        let router = stub_router(&tracker);
        let cookie = format!("claudear_session={token}");
        let proxied = |forwarding: &'static [(&'static str, &'static str)]| {
            let mut headers = vec![
                ("cookie", cookie.as_str()),
                ("host", "127.0.0.1:3100"),
                ("origin", "https://claudear.example.com"),
            ];
            headers.extend_from_slice(forwarding);
            headers
        };

        assert_eq!(
            get_status(&router, WEBSOCKET_PATH, &proxied(&[])).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get_status(
                &router,
                WEBSOCKET_PATH,
                &proxied(&[(
                    "forwarded",
                    "for=192.0.2.60;proto=https;host=claudear.example.com"
                )])
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            get_status(
                &router,
                WEBSOCKET_PATH,
                &proxied(&[("x-forwarded-host", "claudear.example.com")])
            )
            .await,
            StatusCode::OK
        );
    }

    #[test]
    fn forwarded_host_reads_the_first_element_host_parameter() {
        for (value, expected) in [
            ("host=claudear.example.com", Some("claudear.example.com")),
            (
                "for=192.0.2.60;proto=https;host=claudear.example.com",
                Some("claudear.example.com"),
            ),
            (
                "for=\"[2001:db8::1]\"; Host=\"Claudear.example.com:8443\"",
                Some("Claudear.example.com:8443"),
            ),
            (
                "host=claudear.example.com, host=proxy.internal",
                Some("claudear.example.com"),
            ),
            ("for=192.0.2.60, host=proxy.internal", None),
            ("for=192.0.2.60;proto=https", None),
            ("host=", None),
            ("host=\"\"", None),
            ("", None),
        ] {
            assert_eq!(forwarded_host(value), expected, "Forwarded: {value}");
        }
    }

    #[test]
    fn same_origin_accepts_the_forwarded_host() {
        let same_origin = |forwarded: &str, origin: &str| {
            let request = axum::http::Request::builder()
                .uri(WEBSOCKET_PATH)
                .header("host", "127.0.0.1:3100")
                .header("forwarded", forwarded)
                .header("origin", origin)
                .body(Body::empty())
                .unwrap();
            is_same_origin(&request)
        };

        assert!(same_origin(
            "proto=https;host=claudear.example.com",
            "https://claudear.example.com"
        ));
        assert!(same_origin(
            "host=\"claudear.example.com:8443\"",
            "https://claudear.example.com:8443"
        ));
        assert!(!same_origin(
            "host=claudear.example.com, host=evil.example",
            "https://evil.example"
        ));
        assert!(!same_origin(
            "host=claudear.example.com",
            "https://evil.example"
        ));
        assert!(!same_origin(
            "for=192.0.2.60;proto=https",
            "https://claudear.example.com"
        ));
    }

    #[test]
    fn same_origin_compares_origin_authority_to_host() {
        let same_origin = |uri: &str, headers: &[(&str, &str)]| {
            let mut request = axum::http::Request::builder().uri(uri);
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            is_same_origin(&request.body(Body::empty()).unwrap())
        };

        assert!(same_origin(WEBSOCKET_PATH, &[("host", "127.0.0.1:3199")]));
        assert!(same_origin(
            WEBSOCKET_PATH,
            &[
                ("host", "Claudear.example.com"),
                ("origin", "https://claudear.example.com"),
            ]
        ));
        assert!(same_origin(
            WEBSOCKET_PATH,
            &[
                ("host", "127.0.0.1:3100"),
                ("x-forwarded-host", "claudear.example.com, proxy.internal"),
                ("origin", "https://claudear.example.com"),
            ]
        ));
        assert!(same_origin(
            "https://claudear.example.com/api/chat/ws",
            &[("origin", "https://claudear.example.com")]
        ));
        assert!(!same_origin(
            WEBSOCKET_PATH,
            &[
                ("host", "127.0.0.1:3199"),
                ("origin", "http://127.0.0.1:3200")
            ]
        ));
        assert!(!same_origin(
            WEBSOCKET_PATH,
            &[("host", "127.0.0.1:3199"), ("origin", "null")]
        ));
        assert!(!same_origin(
            WEBSOCKET_PATH,
            &[("origin", "http://127.0.0.1:3199")]
        ));
    }

    #[test]
    fn chat_state_is_none_when_chat_is_disabled() {
        let mut config = enabled_config();
        config.chat.enabled = false;
        assert!(chat_state(&config, tracker(), None).is_none());
    }

    #[test]
    fn chat_state_is_none_when_llm_is_disabled() {
        let mut config = enabled_config();
        config.llm.enabled = false;
        assert!(chat_state(&config, tracker(), None).is_none());
    }

    #[test]
    fn chat_state_is_none_without_an_embedding_client() {
        assert!(chat_state(&enabled_config(), tracker(), None).is_none());
    }

    #[tokio::test]
    #[ignore = "loads the ONNX embedding model"]
    async fn chat_enabled_router_requires_a_session() {
        let embedding_client = EmbeddingClient::new(EmbeddingConfig {
            pool_size: 1,
            ..EmbeddingConfig::from_env()
        })
        .map(Arc::new)
        .expect("embedding model loads");
        let storage = tempfile::tempdir().unwrap();
        let tracker = tracker();
        let token = session(&tracker);
        let mut config = enabled_config();
        config.storage_dir = storage.path().to_path_buf();
        let chat = chat_state(&config, tracker.clone(), Some(embedding_client));
        assert!(chat.is_some());

        let router = apply_dashboard_middleware(
            create_api_router_full(
                config,
                tracker.clone(),
                PathBuf::from("claudear.toml"),
                tracker.subscribe_indexing_progress(),
                None,
                chat,
            ),
            false,
        );
        let cookie = format!("claudear_session={token}");

        for route in ["/api/chat/sessions", "/api/chat/models"] {
            assert_eq!(
                get_status(&router, route, &[]).await,
                StatusCode::UNAUTHORIZED,
                "{route} without a session"
            );
            assert_eq!(
                get_status(&router, route, &[("cookie", &cookie)]).await,
                StatusCode::OK,
                "{route} with a session"
            );
        }
    }

    const CSRF_TOKEN: &str = "chat-test-csrf";

    struct SignedIn {
        id: i64,
        cookie: String,
    }

    fn sign_in(tracker: &Arc<dyn FixAttemptTracker>, email: &str, role: &str) -> SignedIn {
        let password_hash = bcrypt::hash("testpass", 4).unwrap();
        let id = tracker
            .create_user(email, &password_hash, "Chat User", role)
            .unwrap();
        let token = tracker.create_session(id, "2099-12-31 23:59:59").unwrap();
        SignedIn {
            id,
            cookie: format!("claudear_session={token}; claudear_csrf={CSRF_TOKEN}"),
        }
    }

    fn chat_router(tracker: &Arc<dyn FixAttemptTracker>) -> Router {
        let search = CodeSearchService::new(tracker.clone(), Arc::new(EmbeddingClient::unloaded()));
        let chat_service = ChatService::new(
            ChatConfig::default(),
            LlmModelConfig::default(),
            search,
            tracker.clone(),
        );
        let chat = create_chat_router(ChatState {
            chat_service: Arc::new(chat_service),
            tracker: tracker.clone(),
        });
        apply_dashboard_middleware(mount(Router::new(), Some(chat), api_state(tracker)), false)
    }

    fn owned_session(tracker: &Arc<dyn FixAttemptTracker>, id: &str, owner: Option<i64>) {
        tracker.create_chat_session(id, None, owner).unwrap();
        tracker
            .save_chat_message(id, "user", "private question", None)
            .unwrap();
    }

    async fn send(
        router: &Router,
        method: &str,
        uri: &str,
        viewer: &SignedIn,
    ) -> (StatusCode, Value) {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("cookie", &viewer.cookie)
                    .header("x-csrf-token", CSRF_TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn listed_ids(router: &Router, viewer: &SignedIn) -> Vec<String> {
        let (status, body) = send(router, "GET", "/api/chat/sessions", viewer).await;
        assert_eq!(status, StatusCode::OK);
        let mut ids: Vec<String> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    }

    #[tokio::test]
    async fn the_owner_lists_and_reads_their_own_chat_session() {
        let tracker = tracker();
        let owner = sign_in(&tracker, "owner@example.com", "viewer");
        owned_session(&tracker, "mine", Some(owner.id));
        let router = chat_router(&tracker);

        assert_eq!(listed_ids(&router, &owner).await, ["mine"]);
        let (status, body) = send(&router, "GET", "/api/chat/sessions/mine", &owner).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["owner_user_id"], owner.id);
        assert_eq!(body["messages"][0]["content"], "private question");
    }

    #[tokio::test]
    async fn another_viewer_cannot_find_a_chat_session_they_do_not_own() {
        let tracker = tracker();
        let owner = sign_in(&tracker, "owner@example.com", "viewer");
        let other = sign_in(&tracker, "other@example.com", "viewer");
        owned_session(&tracker, "mine", Some(owner.id));
        let router = chat_router(&tracker);

        assert!(listed_ids(&router, &other).await.is_empty());
        let (status, body) = send(&router, "GET", "/api/chat/sessions/mine", &other).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!body.to_string().contains("private question"));
        let (status, _) = send(&router, "DELETE", "/api/chat/sessions/mine", &other).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            send(&router, "GET", "/api/chat/sessions/missing", &other)
                .await
                .0,
            StatusCode::NOT_FOUND,
            "a missing session answers the same as another user's"
        );

        assert!(tracker.get_chat_session("mine").unwrap().is_some());
    }

    #[tokio::test]
    async fn an_admin_deletes_another_users_chat_session() {
        let tracker = tracker();
        let owner = sign_in(&tracker, "owner@example.com", "viewer");
        let admin = sign_in(&tracker, "admin@example.com", "admin");
        owned_session(&tracker, "mine", Some(owner.id));
        let router = chat_router(&tracker);

        let (status, _) = send(&router, "DELETE", "/api/chat/sessions/mine", &admin).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(tracker.get_chat_session("mine").unwrap().is_none());
    }

    #[tokio::test]
    async fn an_admin_cannot_read_another_users_chat_history() {
        let tracker = tracker();
        let owner = sign_in(&tracker, "owner@example.com", "viewer");
        let admin = sign_in(&tracker, "admin@example.com", "admin");
        owned_session(&tracker, "mine", Some(owner.id));
        let router = chat_router(&tracker);

        let (status, body) = send(&router, "GET", "/api/chat/sessions/mine", &admin).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!body.to_string().contains("private question"));
        assert!(listed_ids(&router, &admin).await.is_empty());
    }

    #[tokio::test]
    async fn an_ownerless_chat_session_is_visible_only_to_admins() {
        let tracker = tracker();
        let viewer = sign_in(&tracker, "viewer@example.com", "viewer");
        let admin = sign_in(&tracker, "admin@example.com", "admin");
        owned_session(&tracker, "legacy", None);
        owned_session(&tracker, "admins-own", Some(admin.id));
        let router = chat_router(&tracker);

        assert!(listed_ids(&router, &viewer).await.is_empty());
        let (status, _) = send(&router, "GET", "/api/chat/sessions/legacy", &viewer).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(&router, "DELETE", "/api/chat/sessions/legacy", &viewer).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        assert_eq!(listed_ids(&router, &admin).await, ["admins-own", "legacy"]);
        let (status, body) = send(&router, "GET", "/api/chat/sessions/legacy", &admin).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["messages"][0]["content"], "private question");
        let (status, _) = send(&router, "DELETE", "/api/chat/sessions/legacy", &admin).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(tracker.get_chat_session("legacy").unwrap().is_none());
    }

    async fn serve(router: Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        address
    }

    async fn chat_over_websocket(
        address: std::net::SocketAddr,
        viewer: &SignedIn,
        session_id: Option<&str>,
    ) -> Value {
        let mut request = format!("ws://{address}{WEBSOCKET_PATH}")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("cookie", viewer.cookie.parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();

        let mut message = serde_json::json!({ "type": "chat", "message": "hello" });
        if let Some(session_id) = session_id {
            message["session_id"] = Value::from(session_id);
        }
        socket
            .send(Message::text(message.to_string()))
            .await
            .unwrap();

        let reply = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("the server replies")
            .expect("the socket stays open")
            .unwrap();
        serde_json::from_str(reply.to_text().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_websocket_attach_to_another_users_chat_session_is_rejected() {
        let tracker = tracker();
        let owner = sign_in(&tracker, "owner@example.com", "viewer");
        let other = sign_in(&tracker, "other@example.com", "viewer");
        let admin = sign_in(&tracker, "admin@example.com", "admin");
        owned_session(&tracker, "mine", Some(owner.id));
        let address = serve(chat_router(&tracker)).await;

        for intruder in [&other, &admin] {
            let reply = chat_over_websocket(address, intruder, Some("mine")).await;
            assert_eq!(reply["done"], true);
            assert_eq!(reply["error"], CHAT_SESSION_NOT_FOUND);
        }
        assert_eq!(
            tracker.get_chat_history("mine", 10).unwrap().len(),
            1,
            "a rejected attach saves nothing to the session"
        );

        let reply = chat_over_websocket(address, &owner, Some("mine")).await;
        assert_ne!(reply["error"], CHAT_SESSION_NOT_FOUND);
        assert_eq!(tracker.get_chat_history("mine", 10).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_websocket_creates_a_new_chat_session_owned_by_the_caller() {
        let tracker = tracker();
        let owner = sign_in(&tracker, "owner@example.com", "viewer");
        let address = serve(chat_router(&tracker)).await;

        let reply = chat_over_websocket(address, &owner, None).await;
        let session_id = reply["session_id"].as_str().unwrap();
        let session = tracker.get_chat_session(session_id).unwrap().unwrap();
        assert_eq!(session.owner_user_id, Some(owner.id));

        let reply = chat_over_websocket(address, &owner, Some("named-by-client")).await;
        assert_ne!(reply["error"], CHAT_SESSION_NOT_FOUND);
        let session = tracker
            .get_chat_session("named-by-client")
            .unwrap()
            .unwrap();
        assert_eq!(session.owner_user_id, Some(owner.id));
    }
}
