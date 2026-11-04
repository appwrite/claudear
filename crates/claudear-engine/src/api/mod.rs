//! Dashboard API endpoints.
//!
//! Provides REST API for the analytics dashboard.

pub mod auth;
mod directory;
pub(crate) mod embedded;
pub mod error;
mod identities;
mod invites;
mod rate_limit;
mod routes;
mod security;
mod tokens;

pub use routes::{create_api_router, create_api_router_full, create_api_router_with_dashboard};

use axum::http;
use claudear_config::config::Config;
use claudear_config::users::UserRegistry;
use claudear_core::error::Result;
use claudear_storage::FixAttemptTracker;
use sentry::integrations::tower::{NewSentryLayer, SentryHttpLayer};
use std::path::PathBuf;
use std::sync::Arc;
use tower_cookies::CookieManagerLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Apply the standard dashboard middleware stack to a router serving the
/// dashboard API: security headers, CSRF protection, CORS, and cookie
/// management. Shared by the standalone [`ApiServer`] and the combined
/// webhook+dashboard server so both expose identical auth/cookie behavior.
///
/// `tls_enabled` controls the CSRF cookie's `Secure` flag and should reflect
/// whether the server is actually serving HTTPS.
///
/// Apply this only to the dashboard routes — the CSRF layer would otherwise
/// reject inbound webhook `POST`s, which carry no CSRF token.
pub fn apply_dashboard_middleware(router: axum::Router, tls_enabled: bool) -> axum::Router {
    // Allow requests from common local dashboard origins.
    // In production behind a reverse proxy, the proxy should handle CORS.
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            origin.to_str().is_ok_and(is_local_origin)
        }))
        .allow_methods([
            http::Method::GET,
            http::Method::POST,
            http::Method::PUT,
            http::Method::DELETE,
            http::Method::OPTIONS,
        ])
        .allow_headers([
            http::header::CONTENT_TYPE,
            http::header::AUTHORIZATION,
            http::header::COOKIE,
            http::HeaderName::from_static("x-csrf-token"),
        ])
        .allow_credentials(true);

    router
        .layer(axum::middleware::from_fn(security::security_headers))
        .layer(axum::middleware::from_fn(move |req, next| {
            security::csrf_protection(req, next, tls_enabled)
        }))
        .layer(cors)
        .layer(CookieManagerLayer::new())
}

const LOCAL_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Whether `origin` is `http(s)://` a loopback host on any port. The host must match
/// exactly, so `http://localhost.attacker.com` is not local.
fn is_local_origin(origin: &str) -> bool {
    let Ok(uri) = origin.parse::<http::Uri>() else {
        return false;
    };
    let scheme_is_web = matches!(uri.scheme_str(), Some("http" | "https"));
    let bare_origin = uri.path_and_query().is_none_or(|path| path.as_str() == "/");
    uri.authority().is_some_and(|authority| {
        scheme_is_web
            && bare_origin
            && !authority.as_str().contains('@')
            && LOCAL_HOSTS.contains(&authority.host())
    })
}

/// API server configuration.
pub struct ApiServer {
    config: Config,
    tracker: Arc<dyn FixAttemptTracker>,
    port: u16,
    dashboard_dir: Option<PathBuf>,
    config_path: PathBuf,
    registry: UserRegistry,
}

impl ApiServer {
    /// Create a new API server.
    pub fn new(config: Config, tracker: Arc<dyn FixAttemptTracker>, config_path: PathBuf) -> Self {
        let port = config.webhook_port; // Reuse webhook port for now
        Self {
            registry: UserRegistry::new(config.users.clone()),
            config,
            tracker,
            port,
            dashboard_dir: None,
            config_path,
        }
    }

    /// Create a new API server with custom port.
    pub fn with_port(
        config: Config,
        tracker: Arc<dyn FixAttemptTracker>,
        port: u16,
        config_path: PathBuf,
    ) -> Self {
        Self {
            registry: UserRegistry::new(config.users.clone()),
            config,
            tracker,
            port,
            dashboard_dir: None,
            config_path,
        }
    }

    /// Create a new API server with dashboard directory.
    pub fn with_dashboard(
        config: Config,
        tracker: Arc<dyn FixAttemptTracker>,
        port: u16,
        dashboard_dir: PathBuf,
        config_path: PathBuf,
    ) -> Self {
        Self {
            registry: UserRegistry::new(config.users.clone()),
            config,
            tracker,
            port,
            dashboard_dir: Some(dashboard_dir),
            config_path,
        }
    }

    /// Serve the given registry instead of one built from config, so the dashboard
    /// shares the daemon's person directory and identity edits reach every consumer.
    pub fn with_registry(mut self, registry: UserRegistry) -> Self {
        self.registry = registry;
        self
    }

    fn router(&self) -> axum::Router {
        create_api_router_with_dashboard(
            self.config.clone(),
            self.tracker.clone(),
            self.config_path.clone(),
            self.tracker.subscribe_indexing_progress(),
            self.dashboard_dir.clone(),
            self.registry.clone(),
        )
    }

    /// Start the API server.
    pub async fn start(self) -> Result<()> {
        let tls_enabled = self.config.tls.enabled;
        let app = apply_dashboard_middleware(self.router(), tls_enabled)
            // Sentry layers: NewSentryLayer must be outermost (added last in axum's layer chain)
            .layer(SentryHttpLayer::new().enable_transaction())
            .layer(NewSentryLayer::new_from_top());

        let tls_enabled = self.config.tls.enabled;
        let scheme = if tls_enabled { "https" } else { "http" };

        tracing::info!(
            "Dashboard API server starting ({}://{}:{})",
            scheme,
            self.config.bind_address,
            if tls_enabled {
                self.config.tls.https_port
            } else {
                self.port
            }
        );
        if self.dashboard_dir.is_some() {
            tracing::info!(
                "Serving dashboard from filesystem at {}://localhost:{}",
                scheme,
                if tls_enabled {
                    self.config.tls.https_port
                } else {
                    self.port
                }
            );
        } else if embedded::has_dashboard() {
            tracing::info!(
                "Serving embedded dashboard at {}://localhost:{}",
                scheme,
                if tls_enabled {
                    self.config.tls.https_port
                } else {
                    self.port
                }
            );
        } else {
            tracing::info!(
                "API only mode - no dashboard embedded. Use --dashboard-dir for development."
            );
        }

        // Spawn background task to periodically clean up expired and idle sessions.
        // Runs every hour, cleaning up sessions past their expires_at or idle for 30+ minutes.
        let tracker_for_cleanup = self.tracker.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                match tracker_for_cleanup.cleanup_expired_sessions() {
                    Ok(count) if count > 0 => {
                        tracing::info!(deleted = count, "Cleaned up expired sessions");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to clean up expired sessions");
                    }
                    _ => {}
                }
            }
        });

        if tls_enabled {
            claudear_integrations::tls::serve_with_tls(
                &self.config.tls,
                &self.config.bind_address,
                app,
            )
            .await?;
        } else {
            claudear_integrations::tls::serve_plain_http(&self.config.bind_address, self.port, app)
                .await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use claudear_config::config::UserConfig;
    use claudear_core::identity::Role;
    use claudear_storage::SqliteTracker;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn server(users: HashMap<String, UserConfig>) -> ApiServer {
        let tracker: Arc<dyn FixAttemptTracker> =
            Arc::new(SqliteTracker::in_memory().expect("in-memory tracker"));
        let config = Config {
            users,
            storage_dir: std::env::temp_dir().join("claudear-api-server-tests"),
            ..Config::default()
        };
        ApiServer::with_port(config, tracker, 0, PathBuf::from("claudear.toml"))
    }

    #[tokio::test]
    async fn identity_edits_through_the_server_reach_the_injected_registry() {
        let server = server(HashMap::new());
        let tracker = server.tracker.clone();
        let admin = tracker
            .create_user("admin@example.com", "unused-hash", "Admin", Role::Admin)
            .expect("create admin");
        let token = tracker
            .create_session(admin, "2099-12-31 23:59:59")
            .expect("create session");
        let registry = UserRegistry::new(HashMap::new());
        let notifier_view = registry.clone();
        let router = server
            .with_registry(registry.clone())
            .router()
            .layer(CookieManagerLayer::new());
        assert!(notifier_view.resolve("github", "admin-gh").is_none());

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/users/{admin}/identities"))
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={token}"))
                    .body(Body::from(
                        serde_json::json!({"github_usernames": ["admin-gh"]}).to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let resolved = notifier_view
            .resolve("github", "admin-gh")
            .expect("an HTTP identity edit must reach every holder of the registry");
        assert_eq!(resolved.slug, format!("console-{admin}"));
    }

    #[test]
    fn server_without_an_injected_registry_serves_config_people() {
        let server = server(HashMap::from([(
            "jake".to_string(),
            UserConfig {
                github_usernames: vec!["jakebarnby".to_string()],
                ..UserConfig::default()
            },
        )]));

        assert_eq!(
            server
                .registry
                .resolve("github", "jakebarnby")
                .map(|resolved| resolved.slug),
            Some("jake".to_string())
        );
    }

    #[test]
    fn only_exact_loopback_hosts_are_local_origins() {
        for origin in [
            "http://localhost",
            "http://localhost:5173",
            "https://localhost:8443",
            "http://127.0.0.1:3100",
            "https://127.0.0.1",
            "http://[::1]:3000",
        ] {
            assert!(is_local_origin(origin), "{origin} is local");
        }
        for origin in [
            "http://localhost.attacker.com",
            "https://127.0.0.1.attacker.com",
            "http://localhostattacker.com",
            "http://127.0.0.10",
            "http://attacker.com",
            "ftp://localhost",
            "http://user@localhost",
            "http://localhost/path",
            "localhost:5173",
            "null",
            "",
        ] {
            assert!(!is_local_origin(origin), "{origin} is not local");
        }
    }

    #[tokio::test]
    async fn cors_allows_credentials_only_for_loopback_origins() {
        let router = apply_dashboard_middleware(
            axum::Router::new().route("/api/ping", axum::routing::get(|| async { "pong" })),
            false,
        );
        for (origin, allowed) in [
            ("http://localhost:5173", true),
            ("http://localhost.attacker.com", false),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("OPTIONS")
                        .uri("/api/ping")
                        .header("origin", origin)
                        .header("access-control-request-method", "GET")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            let allowed_origin = response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok());
            assert_eq!(allowed_origin, allowed.then_some(origin), "{origin}");
        }
    }
}
