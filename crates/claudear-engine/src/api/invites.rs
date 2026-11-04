//! Single-use invite links that let an invited (passwordless) user set a password.
//!
//! The secret is shown once, when issued, and only its SHA-256 hash is stored. Links
//! carry it in the URL fragment (`/invite#<secret>`), which browsers never send to the
//! server, so it stays out of access logs, Sentry transactions and Referer headers.

use super::auth::{extract_client_ip, hash_password, AdminUser, MessageResponse, PASSWORD_LENGTH};
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
    if !state.limiters.api.allow(&admin.id) {
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
    if !state.limiters.api.allow(&admin.id) {
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
    limit_by_ip(&state, &headers)?;
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
    limit_by_ip(&state, &headers)?;
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

/// Invite endpoints draw from their own per-address limit so they never eat the login
/// budget.
fn limit_by_ip(state: &ApiState, headers: &HeaderMap) -> Result<(), ApiError> {
    if state.limiters.invite.allow(&extract_client_ip(headers)) {
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
}
