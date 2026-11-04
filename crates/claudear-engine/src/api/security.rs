//! Security middleware for the dashboard API.
//!
//! Provides security headers (X-Frame-Options, X-Content-Type-Options, etc.)
//! and CSRF protection via double-submit cookie pattern.

use axum::{
    extract::Request,
    http::{HeaderValue, Method, StatusCode},
    middleware::Next,
    response::Response,
};
use tower_cookies::{Cookie, Cookies};

const CSRF_COOKIE: &str = "claudear_csrf";
const CSRF_HEADER: &str = "x-csrf-token";
const CSRF_TOKEN_LEN: usize = 32;
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self' ws: wss:; font-src 'self'; frame-ancestors 'none'";

/// Middleware that adds security headers to every response.
pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "referrer-policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );

    response
}

/// Middleware that enforces CSRF protection on state-changing requests.
///
/// Uses the double-submit cookie pattern:
/// - A random CSRF token is set in a readable (non-HttpOnly) cookie
/// - Mutating requests (POST/PUT/DELETE) must include the token in the X-CSRF-Token header
/// - The header value must match the cookie value
///
/// Login is exempted since there's no session/CSRF cookie yet.
pub async fn csrf_protection(
    request: Request,
    next: Next,
    tls_enabled: bool,
) -> Result<Response, StatusCode> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    let is_mutating = matches!(method, Method::POST | Method::PUT | Method::DELETE);

    // Login and logout are exempted: login has no CSRF cookie yet,
    // and logout is safe (just clears the session).
    let is_exempt = path == "/api/auth/login" || path == "/api/auth/logout";

    // Access the Cookies extension from the request (set by CookieManagerLayer).
    let cookies = request
        .extensions()
        .get::<Cookies>()
        .cloned()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    if is_mutating && !is_exempt {
        let cookie_token = cookies
            .get(CSRF_COOKIE)
            .map(|c| c.value().to_string())
            .unwrap_or_default();

        // Extract the CSRF header value
        let header_token = request
            .headers()
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();

        if cookie_token.is_empty() || header_token.is_empty() || cookie_token != header_token {
            tracing::warn!(
                path = %path,
                method = %method,
                "CSRF validation failed"
            );
            return Err(StatusCode::FORBIDDEN);
        }
    }

    // Ensure a CSRF cookie is set (so the frontend can read it for future requests).
    // Only set if not already present to avoid resetting on every request.
    if cookies.get(CSRF_COOKIE).is_none() {
        let token = generate_csrf_token();
        let mut cookie = Cookie::new(CSRF_COOKIE, token);
        cookie.set_path("/");
        cookie.set_http_only(false); // Frontend JS must be able to read this
        cookie.set_secure(tls_enabled);
        cookie.set_same_site(tower_cookies::cookie::SameSite::Lax);
        cookies.add(cookie);
    }

    let response = next.run(request).await;

    Ok(response)
}

fn generate_csrf_token() -> String {
    hex::encode(rand::random::<[u8; CSRF_TOKEN_LEN]>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{get, post};
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use tower_cookies::CookieManagerLayer;

    fn test_app() -> Router {
        Router::new()
            .route("/api/test", get(|| async { "ok" }))
            .route("/api/test", post(|| async { "created" }))
            .route("/api/auth/login", post(|| async { "logged in" }))
            .layer(axum::middleware::from_fn(security_headers))
            .layer(axum::middleware::from_fn(move |req, next| {
                csrf_protection(req, next, false)
            }))
            .layer(CookieManagerLayer::new())
    }

    fn directive(name: &str) -> Vec<&'static str> {
        CONTENT_SECURITY_POLICY
            .split(';')
            .map(str::split_whitespace)
            .find_map(|mut parts| (parts.next() == Some(name)).then(|| parts.collect()))
            .unwrap_or_else(|| panic!("no {name} directive"))
    }

    #[test]
    fn scripts_and_fonts_load_only_from_the_dashboard_origin() {
        assert_eq!(directive("script-src"), ["'self'"]);
        assert_eq!(directive("font-src"), ["'self'"]);
        assert_eq!(directive("style-src"), ["'self'", "'unsafe-inline'"]);
    }

    #[tokio::test]
    async fn test_security_headers_present() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");
        assert_eq!(
            response.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
        assert_eq!(
            response.headers().get("referrer-policy").unwrap(),
            "strict-origin-when-cross-origin"
        );
        assert_eq!(
            response.headers().get("content-security-policy").unwrap(),
            CONTENT_SECURITY_POLICY
        );
        assert!(response.headers().get("permissions-policy").is_some());
    }

    #[tokio::test]
    async fn test_csrf_sets_cookie_on_get() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let set_cookie = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .find(|v| v.to_str().unwrap_or("").contains("claudear_csrf"));
        assert!(set_cookie.is_some(), "CSRF cookie should be set");
    }

    #[tokio::test]
    async fn test_csrf_blocks_post_without_token() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_csrf_allows_post_with_matching_token() {
        let app = test_app();
        let token = "test_csrf_token_value";
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/test")
                    .header("cookie", format!("claudear_csrf={}", token))
                    .header("x-csrf-token", token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_csrf_blocks_post_with_mismatched_token() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/test")
                    .header("cookie", "claudear_csrf=token_a")
                    .header("x-csrf-token", "token_b")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_csrf_exempts_login() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_csrf_allows_get_without_token() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");
    }

    #[test]
    fn test_generate_csrf_token_length() {
        let token = generate_csrf_token();
        assert_eq!(token.len(), CSRF_TOKEN_LEN * 2); // hex encoding doubles length
    }

    #[test]
    fn test_generate_csrf_token_uniqueness() {
        let a = generate_csrf_token();
        let b = generate_csrf_token();
        assert_ne!(a, b);
    }
}
