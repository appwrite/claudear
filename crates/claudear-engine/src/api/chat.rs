//! Chat wiring for the dashboard API: state construction and the session
//! guard layered over the integrations chat router.

use super::auth::AuthUser;
use super::routes::ApiState;
use axum::extract::Request;
use axum::http::uri::Authority;
use axum::http::{header, StatusCode, Uri};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::Response;
use axum::Router;
use claudear_analysis::feedback::EmbeddingClient;
use claudear_analysis::repo::code_index::CodeSearchService;
use claudear_config::config::Config;
use claudear_integrations::chat::{ChatService, ChatState};
use claudear_storage::FixAttemptTracker;
use std::sync::Arc;

const WEBSOCKET_PATH: &str = "/api/chat/ws";
const FORWARDED_HOST: &str = "x-forwarded-host";

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

/// Require a dashboard session on every route of `router`.
pub(super) fn authenticated(router: Router, state: ApiState) -> Router {
    router.route_layer(from_fn_with_state(state, require_session))
}

/// Reject requests without a session, and cross-origin websocket upgrades.
async fn require_session(
    _user: AuthUser,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if request.uri().path() == WEBSOCKET_PATH && !is_same_origin(&request) {
        return Err(StatusCode::FORBIDDEN);
    }
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
    let forwarded_host = header_value(FORWARDED_HOST)
        .and_then(|value| value.split(',').next())
        .map(str::trim);
    let target = request.uri().authority().map(Authority::as_str);

    [header_value(header::HOST.as_str()), forwarded_host, target]
        .into_iter()
        .flatten()
        .any(|host| host.eq_ignore_ascii_case(origin.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{apply_dashboard_middleware, create_api_router_full};
    use axum::body::Body;
    use axum::routing::get;
    use claudear_analysis::feedback::EmbeddingConfig;
    use claudear_storage::SqliteTracker;
    use std::path::PathBuf;
    use std::time::Instant;
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

    fn stub_router(tracker: &Arc<dyn FixAttemptTracker>) -> Router {
        let config = Config::default();
        let state = ApiState {
            storage_dir: config.storage_dir.clone(),
            config,
            tracker: tracker.clone(),
            start_time: Instant::now(),
            config_path: PathBuf::from("claudear.toml"),
            indexing_rx: tracker.subscribe_indexing_progress(),
            chat_service: None,
            startup_config: None,
        };
        let chat = Router::new()
            .route("/api/chat/ws", get(|| async { StatusCode::OK }))
            .route("/api/chat/models", get(|| async { StatusCode::OK }))
            .route("/api/chat/sessions", get(|| async { StatusCode::OK }))
            .route(
                "/api/chat/sessions/{id}",
                get(|| async { StatusCode::OK }).delete(|| async { StatusCode::OK }),
            );
        apply_dashboard_middleware(Router::new().merge(authenticated(chat, state)), false)
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

    fn try_embedding_client() -> Option<Arc<EmbeddingClient>> {
        EmbeddingClient::new(EmbeddingConfig {
            pool_size: 1,
            ..EmbeddingConfig::from_env()
        })
        .ok()
        .map(Arc::new)
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
    async fn chat_enabled_router_requires_a_session() {
        let Some(embedding_client) = try_embedding_client() else {
            eprintln!("Skipping: embedding model unavailable");
            return;
        };
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
}
