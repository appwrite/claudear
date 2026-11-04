//! Single-use invite links that let an invited (passwordless) user set a password.
//!
//! The secret is shown once, when issued, and only its SHA-256 hash is stored. Links
//! carry it in the URL fragment (`/invite#<secret>`), which browsers never send to the
//! server, so it stays out of access logs, Sentry transactions and Referer headers.

use super::auth::{
    check_api_rate_limit, check_ip_rate_limit, extract_client_ip, hash_password, AdminUser,
    MessageResponse, PASSWORD_LENGTH,
};
use super::error::ApiError;
use super::routes::ApiState;
use super::tokens::hash_secret;
use crate::registry;
use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::HeaderMap,
    Json,
};
use claudear_config::config::Config;
use claudear_core::identity::UserStatus;
use claudear_storage::InviteWrite;
use serde::{Deserialize, Serialize};

/// How long an invite link stays valid.
pub(crate) const INVITE_LIFETIME_DAYS: i64 = 7;

const SECRET_PREFIX: &str = "cldi_";
const SECRET_BYTES: usize = 32;
const LINK_PATH: &str = "/invite#";
const EXPIRY_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// A freshly generated invite: the secret to hand out and the hashed row to store.
pub(crate) struct NewInvite {
    pub secret: String,
    pub write: InviteWrite,
}

impl NewInvite {
    pub(crate) fn generate(created_by: i64) -> Self {
        let secret = generate_invite_secret();
        let expires_at = (chrono::Utc::now() + chrono::Duration::days(INVITE_LIFETIME_DAYS))
            .format(EXPIRY_FORMAT)
            .to_string();
        Self {
            write: InviteWrite {
                token_hash: hash_secret(&secret),
                expires_at,
                created_by,
            },
            secret,
        }
    }

    pub(crate) fn response(&self, config: &Config) -> UserInvite {
        invite_response(config, &self.secret, &self.write.expires_at)
    }
}

/// An invite link as the API returns it. `invite_url` is absolute only when
/// `[dashboard] public_url` is configured.
#[derive(Clone, Serialize)]
pub(crate) struct UserInvite {
    pub invite_link: String,
    pub invite_url: Option<String>,
    pub expires_at: String,
}

/// `cldi_` followed by 64 hex characters (32 random bytes).
pub(crate) fn generate_invite_secret() -> String {
    format!(
        "{SECRET_PREFIX}{}",
        hex::encode(rand::random::<[u8; SECRET_BYTES]>())
    )
}

pub(crate) fn invite_response(config: &Config, secret: &str, expires_at: &str) -> UserInvite {
    let invite_link = format!("{LINK_PATH}{secret}");
    let invite_url = config
        .dashboard
        .public_url
        .as_deref()
        .map(|url| url.trim().trim_end_matches('/'))
        .filter(|url| !url.is_empty())
        .map(|url| format!("{url}{invite_link}"));
    UserInvite {
        invite_link,
        invite_url,
        expires_at: expires_at.to_string(),
    }
}

/// A cost-12 bcrypt hash of random bytes nobody keeps, so an invited user cannot log in,
/// yet a login attempt against them costs exactly as much as against a real account.
pub(crate) async fn unusable_password_hash() -> Result<String, ApiError> {
    let unknowable = hex::encode(rand::random::<[u8; SECRET_BYTES]>());
    hash_password(unknowable).await
}

/// Any unusable token: unknown, expired, accepted, revoked or malformed.
fn invalid_invite() -> ApiError {
    ApiError::not_found("This invite link is invalid or has expired")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InspectInviteRequest {
    pub token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptInviteRequest {
    pub token: String,
    pub password: String,
}

#[derive(Serialize)]
pub(crate) struct InviteDetails {
    pub name: String,
    pub email: String,
    pub expires_at: String,
}

/// POST /api/users/{id}/invite — issue a new link; any earlier link stops working.
pub(crate) async fn issue_invite_handler(
    AdminUser(admin): AdminUser,
    State(state): State<ApiState>,
    Path(user_id): Path<i64>,
) -> Result<Json<UserInvite>, ApiError> {
    if !check_api_rate_limit(admin.id) {
        return Err(ApiError::rate_limited());
    }
    let user = state
        .tracker
        .get_user_by_id(user_id)
        .map_err(ApiError::storage)?
        .ok_or_else(ApiError::user_not_found)?;
    if user.status == UserStatus::Active {
        return Err(ApiError::conflict("User is already active"));
    }
    let invite = NewInvite::generate(admin.id);
    state
        .tracker
        .issue_invite(
            user_id,
            &invite.write.token_hash,
            &invite.write.expires_at,
            admin.id,
        )
        .map_err(ApiError::storage)?;
    Ok(Json(invite.response(&state.config)))
}

/// DELETE /api/users/{id}/invite
pub(crate) async fn revoke_invite_handler(
    AdminUser(admin): AdminUser,
    State(state): State<ApiState>,
    Path(user_id): Path<i64>,
) -> Result<Json<MessageResponse>, ApiError> {
    if !check_api_rate_limit(admin.id) {
        return Err(ApiError::rate_limited());
    }
    let revoked = state
        .tracker
        .revoke_invite(user_id)
        .map_err(ApiError::storage)?;
    if !revoked {
        return Err(ApiError::not_found("User has no open invite"));
    }
    Ok(Json(MessageResponse {
        message: "Invite revoked".to_string(),
    }))
}

/// POST /api/auth/invite/inspect — who an open invite is for, so the page can greet them.
pub(crate) async fn inspect_invite_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Json<InspectInviteRequest>, JsonRejection>,
) -> Result<Json<InviteDetails>, ApiError> {
    limit_by_ip(&headers)?;
    let Json(request) = body?;
    let token_hash = invite_hash(&request.token)?;
    let (invite, user) = state
        .tracker
        .find_open_invite(&token_hash)
        .map_err(ApiError::storage)?
        .ok_or_else(invalid_invite)?;
    Ok(Json(InviteDetails {
        name: user.name,
        email: user.email,
        expires_at: invite.expires_at,
    }))
}

/// POST /api/auth/invite/accept — set the password and activate. Does not log in.
pub(crate) async fn accept_invite_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Json<AcceptInviteRequest>, JsonRejection>,
) -> Result<Json<MessageResponse>, ApiError> {
    limit_by_ip(&headers)?;
    let Json(request) = body?;
    if !PASSWORD_LENGTH.contains(&request.password.len()) {
        return Err(ApiError::invalid_password());
    }
    let token_hash = invite_hash(&request.token)?;
    if state
        .tracker
        .find_open_invite(&token_hash)
        .map_err(ApiError::storage)?
        .is_none()
    {
        return Err(invalid_invite());
    }
    let password_hash = hash_password(request.password).await?;
    let _writes = state.identity_writes.lock().await;
    state
        .tracker
        .accept_invite(&token_hash, &password_hash)
        .map_err(ApiError::storage)?
        .ok_or_else(invalid_invite)?;
    registry::reload(&state.registry, state.tracker.as_ref());
    Ok(Json(MessageResponse {
        message: "Invite accepted".to_string(),
    }))
}

/// Invite endpoints draw from their own per-IP bucket so they never eat the login budget.
fn limit_by_ip(headers: &HeaderMap) -> Result<(), ApiError> {
    let ip = extract_client_ip(headers);
    if check_ip_rate_limit(&format!("invite:{ip}")) {
        Ok(())
    } else {
        Err(ApiError::rate_limited())
    }
}

fn invite_hash(token: &str) -> Result<String, ApiError> {
    let token = token.trim();
    if token.starts_with(SECRET_PREFIX) {
        Ok(hash_secret(token))
    } else {
        Err(invalid_invite())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_api_router_with_dashboard;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use claudear_config::users::UserRegistry;
    use claudear_storage::{FixAttemptTracker, SqliteTracker};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Users created first so the acting admin's id stays clear of ids other suites
    /// write as, because the API rate limiter is process-wide.
    const FILLER_USERS: usize = 1100;
    const CSRF_TOKEN: &str = "invite-tests-csrf";

    fn config(public_url: Option<&str>) -> Config {
        let mut config = Config {
            storage_dir: std::env::temp_dir().join("claudear-invites-tests"),
            ..Config::default()
        };
        config.dashboard.public_url = public_url.map(str::to_string);
        config
    }

    #[test]
    fn secret_is_prefixed_hex_of_32_bytes() {
        let secret = generate_invite_secret();
        let hex_part = secret.strip_prefix("cldi_").expect("cldi_ prefix");
        assert_eq!(hex_part.len(), 64);
        assert!(hex_part
            .chars()
            .all(|character| character.is_ascii_hexdigit()));
        assert_ne!(secret, generate_invite_secret());
    }

    #[test]
    fn new_invite_stores_only_the_hash_and_expires_in_a_week() {
        let invite = NewInvite::generate(3);
        assert_eq!(invite.write.token_hash, hash_secret(&invite.secret));
        assert_ne!(invite.write.token_hash, invite.secret);
        assert_eq!(invite.write.created_by, 3);
        let expires =
            chrono::NaiveDateTime::parse_from_str(&invite.write.expires_at, EXPIRY_FORMAT)
                .expect("expiry format");
        let lifetime = expires - chrono::Utc::now().naive_utc();
        assert!(
            lifetime > chrono::Duration::days(INVITE_LIFETIME_DAYS) - chrono::Duration::minutes(1)
        );
        assert!(lifetime <= chrono::Duration::days(INVITE_LIFETIME_DAYS));
    }

    #[test]
    fn invite_url_is_absolute_only_with_a_public_url() {
        let relative = invite_response(&config(None), "cldi_abc", "2026-11-11 10:00:00");
        assert_eq!(relative.invite_link, "/invite#cldi_abc");
        assert_eq!(relative.invite_url, None);
        assert_eq!(relative.expires_at, "2026-11-11 10:00:00");

        for public_url in [
            "https://claudear.example.com",
            "https://claudear.example.com/",
        ] {
            let absolute = invite_response(&config(Some(public_url)), "cldi_abc", "x");
            assert_eq!(
                absolute.invite_url.as_deref(),
                Some("https://claudear.example.com/invite#cldi_abc"),
                "{public_url}"
            );
        }

        let blank = invite_response(&config(Some("  ")), "cldi_abc", "x");
        assert_eq!(blank.invite_url, None);
    }

    #[tokio::test]
    async fn unusable_hash_is_default_cost_bcrypt() {
        let hash = unusable_password_hash().await.expect("hash");
        assert!(hash.starts_with("$2b$12$"), "unexpected hash format");
        assert!(!bcrypt::verify("", &hash).expect("verify"));
    }

    #[test]
    fn invalid_tokens_all_share_one_generic_response() {
        for token in ["cldr_abc", ""] {
            let error = invite_hash(token).expect_err("invalid token");
            assert_eq!(error.status(), invalid_invite().status());
            assert_eq!(error.message(), invalid_invite().message());
        }
        assert_eq!(
            invite_hash(" cldi_abc ").ok(),
            Some(hash_secret("cldi_abc"))
        );
    }

    struct TestApp {
        router: Router,
        tracker: Arc<dyn FixAttemptTracker>,
        admin_token: String,
    }

    fn test_app() -> TestApp {
        let tracker: Arc<dyn FixAttemptTracker> =
            Arc::new(SqliteTracker::in_memory().expect("in-memory tracker"));
        for index in 0..FILLER_USERS {
            tracker
                .create_user(
                    &format!("filler{index}@example.com"),
                    "unused-hash",
                    "Filler",
                    "viewer",
                )
                .expect("create filler user");
        }
        let admin = tracker
            .create_user("invites-admin@example.com", "unused-hash", "Admin", "admin")
            .expect("create admin");
        let admin_token = tracker
            .create_session(admin, "2099-12-31 23:59:59")
            .expect("create session");
        let config = config(None);
        let registry = UserRegistry::new(config.users.clone());
        let router = crate::api::apply_dashboard_middleware(
            create_api_router_with_dashboard(
                config,
                tracker.clone(),
                PathBuf::from("claudear.toml"),
                tracker.subscribe_indexing_progress(),
                None,
                registry,
            ),
            false,
        );
        TestApp {
            router,
            tracker,
            admin_token,
        }
    }

    async fn send(
        app: &TestApp,
        method: &str,
        uri: &str,
        session: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let cookie = match session {
            Some(token) => format!("claudear_csrf={CSRF_TOKEN}; claudear_session={token}"),
            None => format!("claudear_csrf={CSRF_TOKEN}"),
        };
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie)
            .header("x-csrf-token", CSRF_TOKEN)
            .header("x-forwarded-for", "203.0.113.77")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");
        let response = app.router.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    fn token(invite: &Value) -> String {
        invite["invite_link"]
            .as_str()
            .and_then(|link| link.strip_prefix("/invite#"))
            .expect("invite link")
            .to_string()
    }

    #[tokio::test]
    async fn invited_user_inspects_accepts_then_logs_in() {
        let app = test_app();

        let (status, created) = send(
            &app,
            "POST",
            "/api/users",
            Some(&app.admin_token),
            json!({"email": "invites-amy@example.com", "name": "Amy", "role": "viewer"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["status"], "invited");
        assert_eq!(created["invite"]["invite_url"], Value::Null);
        assert_eq!(
            created["invite_expires_at"],
            created["invite"]["expires_at"]
        );
        let secret = token(&created["invite"]);
        assert!(secret.starts_with("cldi_"));

        let login = json!({"email": "invites-amy@example.com", "password": "correct-horse"});
        let (status, _) = send(&app, "POST", "/api/auth/login", None, login.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, details) = send(
            &app,
            "POST",
            "/api/auth/invite/inspect",
            None,
            json!({"token": secret}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(details["name"], "Amy");
        assert_eq!(details["email"], "invites-amy@example.com");

        let (status, body) = send(
            &app,
            "POST",
            "/api/auth/invite/accept",
            None,
            json!({"token": secret, "password": "short"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Password must be 8-72 bytes");

        let (status, body) = send(
            &app,
            "POST",
            "/api/auth/invite/accept",
            None,
            json!({"token": secret, "password": "correct-horse"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["message"], "Invite accepted");

        let (status, body) = send(
            &app,
            "POST",
            "/api/auth/invite/accept",
            None,
            json!({"token": secret, "password": "correct-horse"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "This invite link is invalid or has expired");

        let (status, body) = send(&app, "POST", "/api/auth/login", None, login).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user"]["email"], "invites-amy@example.com");
    }

    #[tokio::test]
    async fn reissue_invalidates_the_old_link_and_active_users_get_409() {
        let app = test_app();
        let (_, created) = send(
            &app,
            "POST",
            "/api/users",
            Some(&app.admin_token),
            json!({"email": "invites-bo@example.com", "name": "Bo", "role": "viewer"}),
        )
        .await;
        let user_id = created["id"].as_i64().expect("user id");
        let first = token(&created["invite"]);
        let uri = format!("/api/users/{user_id}/invite");

        let (status, reissued) = send(&app, "POST", &uri, Some(&app.admin_token), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let second = token(&reissued);
        assert_ne!(first, second);

        let inspect = |token: String| json!({"token": token});
        let (status, _) = send(
            &app,
            "POST",
            "/api/auth/invite/inspect",
            None,
            inspect(first),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(
            &app,
            "POST",
            "/api/auth/invite/inspect",
            None,
            inspect(second.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = send(&app, "DELETE", &uri, Some(&app.admin_token), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["message"], "Invite revoked");
        let (status, _) = send(
            &app,
            "POST",
            "/api/auth/invite/inspect",
            None,
            inspect(second),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(&app, "DELETE", &uri, Some(&app.admin_token), json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let active = app
            .tracker
            .get_user_by_email("invites-admin@example.com")
            .expect("lookup")
            .expect("admin");
        let (status, body) = send(
            &app,
            "POST",
            &format!("/api/users/{}/invite", active.id),
            Some(&app.admin_token),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "User is already active");

        let (status, _) = send(
            &app,
            "POST",
            "/api/users/999999/invite",
            Some(&app.admin_token),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn admin_password_on_an_invited_user_activates_them() {
        let app = test_app();
        let (_, created) = send(
            &app,
            "POST",
            "/api/users",
            Some(&app.admin_token),
            json!({"email": "invites-cy@example.com", "name": "Cy", "role": "viewer"}),
        )
        .await;
        let user_id = created["id"].as_i64().expect("user id");
        let secret = token(&created["invite"]);

        let (status, updated) = send(
            &app,
            "PUT",
            &format!("/api/users/{user_id}"),
            Some(&app.admin_token),
            json!({"password": "admin-chosen-1"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(updated["status"], "active");
        assert_eq!(updated["invite_expires_at"], Value::Null);

        let (status, _) = send(
            &app,
            "POST",
            "/api/auth/invite/inspect",
            None,
            json!({"token": secret}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(
            &app,
            "POST",
            "/api/auth/login",
            None,
            json!({"email": "invites-cy@example.com", "password": "admin-chosen-1"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}
