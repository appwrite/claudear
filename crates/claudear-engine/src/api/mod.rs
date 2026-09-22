//! Dashboard API endpoints.
//!
//! Provides REST API for the analytics dashboard.

pub mod auth;
pub(crate) mod embedded;
mod routes;
mod security;

pub use routes::{create_api_router, create_api_router_full, create_api_router_with_dashboard};

use axum::http;
use claudear_config::config::Config;
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
            if let Ok(origin_str) = origin.to_str() {
                // Allow localhost and 127.0.0.1 on any port (common dev/local setup)
                origin_str.starts_with("http://localhost")
                    || origin_str.starts_with("https://localhost")
                    || origin_str.starts_with("http://127.0.0.1")
                    || origin_str.starts_with("https://127.0.0.1")
            } else {
                false
            }
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

/// API server configuration.
pub struct ApiServer {
    config: Config,
    tracker: Arc<dyn FixAttemptTracker>,
    port: u16,
    dashboard_dir: Option<PathBuf>,
    config_path: PathBuf,
}

impl ApiServer {
    /// Create a new API server.
    pub fn new(config: Config, tracker: Arc<dyn FixAttemptTracker>, config_path: PathBuf) -> Self {
        let port = config.webhook_port; // Reuse webhook port for now
        Self {
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
            config,
            tracker,
            port,
            dashboard_dir: Some(dashboard_dir),
            config_path,
        }
    }

    /// Start the API server.
    pub async fn start(self) -> Result<()> {
        // Subscribe to indexing progress from the tracker's watch channel
        let indexing_rx = self.tracker.subscribe_indexing_progress();

        let tls_enabled = self.config.tls.enabled;
        let app = apply_dashboard_middleware(
            create_api_router_with_dashboard(
                self.config.clone(),
                self.tracker.clone(),
                self.config_path,
                indexing_rx,
                self.dashboard_dir.clone(),
            ),
            tls_enabled,
        )
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
