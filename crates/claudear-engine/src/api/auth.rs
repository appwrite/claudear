//! Authentication and user management handlers.

use axum::{
    extract::{rejection::JsonRejection, FromRequestParts, Path, State},
    http::{header, request::Parts, HeaderMap, StatusCode},
    Json,
};
use claudear_core::identity::{normalize_login_email, Identities, Role, UserStatus};
use claudear_storage::{DirectoryUser, DirectoryWrite, FixAttemptTracker, UserRow};
use serde::{Deserialize, Serialize};
use std::ops::RangeInclusive;
use std::str::FromStr;
use tower_cookies::{Cookie, Cookies};

use super::error::ApiError;
use super::identities::{find_conflicts, plan_update, UpdateIdentitiesRequest};
use super::invites::{unusable_password_hash, NewInvite, UserInvite};
use super::routes::ApiState;
use super::tokens::hash_secret;
use crate::registry;

const SESSION_COOKIE: &str = "claudear_session";
const SESSION_MAX_AGE_DAYS: i64 = 7;

/// Allowed password length in bytes; bcrypt ignores everything past 72.
pub(crate) const PASSWORD_LENGTH: RangeInclusive<usize> = 8..=72;

pub(crate) const BEARER_PREFIX: &str = "Bearer ";

/// A bcrypt hash (cost 12) of a value nobody knows. Logins for unknown emails verify
/// against it so they take as long as logins for real accounts.
const TIMING_DUMMY_HASH: &str = "$2b$12$K4v3LB7TzMIXvbQZDz1F9eZ8cU2smBGz.iAU5h1DhGGCk5mPIFY3K";

/// Extract the client IP address from request headers.
/// Checks `x-forwarded-for` first, then `x-real-ip`, then falls back to "unknown".
pub(crate) fn extract_client_ip(headers: &HeaderMap) -> String {
    if let Some(forwarded) = headers.get("x-forwarded-for") {
        if let Ok(value) = forwarded.to_str() {
            // x-forwarded-for can contain multiple IPs; the first is the client
            if let Some(first_ip) = value.split(',').next() {
                let ip = first_ip.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }

    if let Some(real_ip) = headers.get("x-real-ip") {
        if let Ok(value) = real_ip.to_str() {
            let ip = value.trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
        }
    }

    "unknown".to_string()
}

/// Authenticated user extracted from session cookie.
#[derive(Debug, Clone, Serialize)]
pub struct AuthUser {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub role: Role,
    pub avatar_url: Option<String>,
}

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    fn identify(&self) {
        sentry::configure_scope(|scope| {
            scope.set_user(Some(sentry::User {
                id: Some(self.id.to_string()),
                email: Some(self.email.clone()),
                username: Some(self.name.clone()),
                ..Default::default()
            }));
            scope.set_tag("user.role", self.role);
        });
    }
}

impl From<UserRow> for AuthUser {
    fn from(user: UserRow) -> Self {
        Self {
            id: user.id,
            email: user.email,
            name: user.name,
            role: user.role,
            avatar_url: user.avatar_url,
        }
    }
}

impl FromRequestParts<ApiState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let cookies = Cookies::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::internal())?;

        let token = cookies
            .get(SESSION_COOKIE)
            .map(|c| c.value().to_string())
            .ok_or_else(ApiError::unauthenticated)?;

        let user = state
            .tracker
            .get_session_user(&token)
            .map_err(ApiError::storage)?
            .ok_or_else(ApiError::unauthenticated)?;

        // Refresh the idle session timeout on every authenticated request
        let _ = state.tracker.touch_session(&token);

        let auth_user = AuthUser::from(user);
        auth_user.identify();
        Ok(auth_user)
    }
}

/// Admin user extractor: an [`AuthUser`] whose role is admin.
#[derive(Debug, Clone)]
pub struct AdminUser(pub AuthUser);

impl FromRequestParts<ApiState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        if !user.is_admin() {
            return Err(ApiError::admin_required());
        }
        Ok(AdminUser(user))
    }
}

/// An admin authenticated by `Authorization: Bearer <api token>` or, when that header
/// is absent, by session. With the header present the cookie is never consulted, which
/// is what makes the CSRF exemption for Bearer requests safe. Failed Bearer attempts
/// spend the client address's bearer limit; once it is spent every Bearer attempt from
/// that address is refused before the token is looked up.
#[derive(Debug, Clone)]
pub(crate) struct AdminCaller(pub AuthUser);

impl FromRequestParts<ApiState> for AdminCaller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let Some(authorization) = parts.headers.get(header::AUTHORIZATION) else {
            let AdminUser(user) = AdminUser::from_request_parts(parts, state).await?;
            return Ok(Self(user));
        };
        let address = extract_client_ip(&parts.headers);
        if state.limiters.bearer.exhausted(&address) {
            return Err(ApiError::rate_limited());
        }
        let user = authorization
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix(BEARER_PREFIX))
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(|token| {
                state
                    .tracker
                    .get_user_by_api_token_hash(&hash_secret(token))
            })
            .transpose()
            .map_err(ApiError::storage)?
            .flatten()
            .map(AuthUser::from);
        let Some(user) = user else {
            state.limiters.bearer.allow(&address);
            return Err(ApiError::invalid_token());
        };
        user.identify();
        if !user.is_admin() {
            return Err(ApiError::admin_required());
        }
        Ok(Self(user))
    }
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub user: AuthUserResponse,
}

#[derive(Serialize)]
pub struct AuthUserResponse {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub role: Role,
    pub avatar_url: Option<String>,
}

impl From<&AuthUser> for AuthUserResponse {
    fn from(u: &AuthUser) -> Self {
        AuthUserResponse {
            id: u.id,
            email: u.email.clone(),
            name: u.name.clone(),
            role: u.role,
            avatar_url: u.avatar_url.clone(),
        }
    }
}

/// Without a password the user is created invited and an invite link is returned.
#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub email: String,
    #[serde(default)]
    pub password: Option<String>,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub(crate) identities: Option<UpdateIdentitiesRequest>,
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    pub email: Option<String>,
    pub password: Option<String>,
    pub name: Option<String>,
    pub role: Option<String>,
}

#[derive(Serialize)]
pub struct UserResponse {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub role: Role,
    pub avatar_url: Option<String>,
    pub status: UserStatus,
    /// Expiry of the invited user's open invite, past or not.
    pub invite_expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl UserResponse {
    fn new(user: UserRow, invite_expires_at: Option<String>) -> Self {
        Self {
            id: user.id,
            email: user.email,
            name: user.name,
            role: user.role,
            avatar_url: user.avatar_url,
            status: user.status,
            invite_expires_at,
            created_at: user.created_at,
            updated_at: user.updated_at,
        }
    }
}

/// A newly created user; `invite` carries the one-time link for an invited user.
#[derive(Serialize)]
pub(crate) struct CreatedUserResponse {
    #[serde(flatten)]
    pub user: UserResponse,
    pub invite: Option<UserInvite>,
}

#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

/// POST /api/auth/login
pub async fn login_handler(
    State(state): State<ApiState>,
    cookies: Cookies,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, StatusCode> {
    let client_ip = extract_client_ip(&headers);
    let email = normalize_login_email(&body.email);

    // Rate limit login attempts by IP to prevent distributed brute force across many emails
    if !state.limiters.address.allow(&client_ip) {
        tracing::warn!(ip = %client_ip, "Login IP rate limit exceeded");
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    // Rate limit login attempts by email to prevent brute force and bcrypt CPU exhaustion
    if !state.limiters.login.allow(&email) {
        tracing::warn!(ip = %client_ip, "Login rate limit exceeded");
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    let user = state
        .tracker
        .get_user_by_email(&email)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Unknown, invited and wrong-password logins all run one cost-12 bcrypt verification
    // and fail with the same 401, so neither timing nor body reveals which case it was.
    let hash_to_verify = user.as_ref().map_or_else(
        || TIMING_DUMMY_HASH.to_string(),
        |u| u.password_hash.clone(),
    );

    let password = body.password.clone();
    let valid = tokio::task::spawn_blocking(move || bcrypt::verify(&password, &hash_to_verify))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .unwrap_or(false);

    let user = match user {
        Some(user) if valid && user.status == UserStatus::Active => user,
        _ => return Err(StatusCode::UNAUTHORIZED),
    };

    // Create session
    let expires_at = chrono::Utc::now() + chrono::Duration::days(SESSION_MAX_AGE_DAYS);
    let expires_str = expires_at.format("%Y-%m-%d %H:%M:%S").to_string();

    let token = state
        .tracker
        .create_session(user.id, &expires_str)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Set cookie.
    // Mark the cookie Secure only when the server is actually serving HTTPS
    // (TLS enabled). Keying this off bind_address would force Secure=true for
    // any non-localhost bind (e.g. "0.0.0.0"), so a plain-HTTP deployment would
    // have browsers silently drop the cookie — login succeeds but every
    // subsequent request is unauthenticated. This mirrors the CSRF cookie,
    // which already keys off tls.enabled.
    let mut cookie = Cookie::new(SESSION_COOKIE, token);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_secure(state.config.tls.enabled);
    cookie.set_same_site(tower_cookies::cookie::SameSite::Lax);
    cookie.set_max_age(tower_cookies::cookie::time::Duration::days(
        SESSION_MAX_AGE_DAYS,
    ));
    cookies.add(cookie);

    sentry::configure_scope(|scope| {
        scope.set_user(Some(sentry::User {
            id: Some(user.id.to_string()),
            email: Some(user.email.clone()),
            username: Some(user.name.clone()),
            ..Default::default()
        }));
        scope.set_tag("user.role", user.role);
    });

    Ok(Json(LoginResponse {
        user: AuthUserResponse {
            id: user.id,
            email: user.email,
            name: user.name,
            role: user.role,
            avatar_url: user.avatar_url,
        },
    }))
}

/// POST /api/auth/logout
pub async fn logout_handler(
    State(state): State<ApiState>,
    cookies: Cookies,
) -> Result<Json<MessageResponse>, StatusCode> {
    // Get session token from cookie
    if let Some(cookie) = cookies.get(SESSION_COOKIE) {
        let token = cookie.value().to_string();
        let _ = state.tracker.delete_session(&token);
    }

    // Clear cookie
    let mut cookie = Cookie::new(SESSION_COOKIE, "");
    cookie.set_path("/");
    cookie.set_max_age(tower_cookies::cookie::time::Duration::ZERO);
    cookies.remove(cookie);

    Ok(Json(MessageResponse {
        message: "Logged out".to_string(),
    }))
}

/// GET /api/auth/me
pub async fn me_handler(user: AuthUser) -> Json<AuthUserResponse> {
    Json(AuthUserResponse::from(&user))
}

#[derive(Deserialize)]
pub struct UpdateProfileRequest {
    pub name: Option<String>,
    pub password: Option<String>,
    pub current_password: Option<String>,
}

/// PUT /api/auth/profile
pub async fn update_profile_handler(
    user: AuthUser,
    State(state): State<ApiState>,
    Json(body): Json<UpdateProfileRequest>,
) -> Result<Json<UserResponse>, StatusCode> {
    if !state.limiters.api.allow(&user.id) {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    // Validate name if provided
    if let Some(ref name) = body.name {
        if name.trim().is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    // If changing password, require current_password verification
    let password_hash = match &body.password {
        Some(new_pw) => {
            if !PASSWORD_LENGTH.contains(&new_pw.len()) {
                return Err(StatusCode::BAD_REQUEST);
            }
            // Rate limit password change to prevent bcrypt CPU exhaustion
            if !state.limiters.login.allow(&user.email) {
                return Err(StatusCode::TOO_MANY_REQUESTS);
            }
            let current_pw = body
                .current_password
                .as_deref()
                .ok_or(StatusCode::BAD_REQUEST)?;

            // Fetch current user to verify password
            let current_user = state
                .tracker
                .get_user_by_id(user.id)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .ok_or(StatusCode::NOT_FOUND)?;

            let current_pw_owned = current_pw.to_string();
            let stored_hash = current_user.password_hash.clone();
            let valid = tokio::task::spawn_blocking(move || {
                bcrypt::verify(&current_pw_owned, &stored_hash)
            })
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            if !valid {
                return Err(StatusCode::FORBIDDEN);
            }

            Some(
                hash_password(new_pw.clone())
                    .await
                    .map_err(|error| error.status())?,
            )
        }
        None => None,
    };

    let trimmed_name = body.name.as_deref().map(str::trim);

    state
        .tracker
        .update_user(
            user.id,
            None,
            password_hash.as_deref(),
            trimmed_name,
            None,
            None,
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let updated = state
        .tracker
        .get_user_by_id(user.id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(
        user_response(&state, updated).map_err(|error| error.status())?,
    ))
}

/// POST /api/auth/avatar
pub async fn upload_avatar_handler(
    user: AuthUser,
    State(state): State<ApiState>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !state.limiters.api.allow(&user.id) {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    const MAX_SIZE: usize = 5 * 1024 * 1024; // 5MB
    const ALLOWED_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

    let avatars_dir = state.storage_dir.join("avatars");

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
    {
        // Only process the "avatar" field
        if field.name() != Some("avatar") {
            continue;
        }

        let content_type = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();

        if !ALLOWED_TYPES.contains(&content_type.as_str()) {
            return Err(StatusCode::BAD_REQUEST);
        }

        let ext = match content_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => return Err(StatusCode::BAD_REQUEST),
        };

        let data = field.bytes().await.map_err(|_| StatusCode::BAD_REQUEST)?;

        if data.len() > MAX_SIZE {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }

        // Validate magic bytes match the claimed content type
        let valid_magic = match ext {
            "png" => data.starts_with(&[0x89, b'P', b'N', b'G']),
            "jpg" => data.starts_with(&[0xFF, 0xD8, 0xFF]),
            "gif" => data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"),
            "webp" => data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP",
            _ => false,
        };
        if !valid_magic {
            return Err(StatusCode::BAD_REQUEST);
        }

        // Delete old avatar file using the path stored in DB (avoids prefix-matching bugs)
        if let Some(ref old_url) = user.avatar_url {
            if let Some(old_filename) = old_url.rsplit('/').next() {
                let old_path = avatars_dir.join(old_filename);
                let _ = tokio::fs::remove_file(&old_path).await;
            }
        }

        // Use random token in filename to prevent enumeration
        let random_token = hex::encode(rand::random::<[u8; 8]>());
        let filename = format!("{}_{}.{}", user.id, random_token, ext);
        let file_path = avatars_dir.join(&filename);
        tokio::fs::write(&file_path, &data)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let avatar_url = format!("/avatars/{}", filename);

        // Update user's avatar_url in DB
        state
            .tracker
            .update_user(user.id, None, None, None, None, Some(&avatar_url))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        return Ok(Json(serde_json::json!({ "avatar_url": avatar_url })));
    }

    Err(StatusCode::BAD_REQUEST)
}

/// Hash a password with the default bcrypt cost, off the async runtime.
pub(crate) async fn hash_password(password: String) -> Result<String, ApiError> {
    tokio::task::spawn_blocking(move || bcrypt::hash(password, bcrypt::DEFAULT_COST))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|_| ApiError::internal())
}

/// Whether `email` is usable as a login email (same rules as an identity email).
pub(crate) fn is_valid_login_email(email: &str) -> bool {
    Identities {
        email: Some(email.to_string()),
        ..Identities::default()
    }
    .validate()
    .is_ok()
}

fn validate_login_email(email: &str) -> Result<(), ApiError> {
    if is_valid_login_email(email) {
        Ok(())
    } else {
        Err(ApiError::bad_request("Email must be a valid email address"))
    }
}

fn validate_password(password: Option<&String>) -> Result<(), ApiError> {
    if password.is_none_or(|password| PASSWORD_LENGTH.contains(&password.len())) {
        Ok(())
    } else {
        Err(ApiError::invalid_password())
    }
}

fn validate_name(name: &str) -> Result<(), ApiError> {
    if name.is_empty() {
        Err(ApiError::bad_request("Name must not be empty"))
    } else {
        Ok(())
    }
}

fn parse_role(role: &str) -> Result<Role, ApiError> {
    Role::from_str(role).map_err(|_| ApiError::invalid_role())
}

fn user_response(state: &ApiState, user: UserRow) -> Result<UserResponse, ApiError> {
    let invite_expires_at = match user.status {
        UserStatus::Active => None,
        UserStatus::Invited => state
            .tracker
            .get_open_invite_for_user(user.id)
            .map_err(ApiError::storage)?
            .map(|invite| invite.expires_at),
    };
    Ok(UserResponse::new(user, invite_expires_at))
}

fn find_user(state: &ApiState, id: i64) -> Result<UserRow, ApiError> {
    state
        .tracker
        .get_user_by_id(id)
        .map_err(ApiError::storage)?
        .ok_or_else(ApiError::user_not_found)
}

/// GET /api/users
pub async fn list_users_handler(
    _admin: AdminUser,
    State(state): State<ApiState>,
) -> Result<Json<Vec<UserResponse>>, ApiError> {
    let users = state.tracker.list_users().map_err(ApiError::storage)?;
    let mut expiries = state
        .tracker
        .open_invite_expiries()
        .map_err(ApiError::storage)?;

    let response = users
        .into_iter()
        .map(|user| {
            let invite_expires_at = match user.status {
                UserStatus::Active => None,
                UserStatus::Invited => expiries.remove(&user.id),
            };
            UserResponse::new(user, invite_expires_at)
        })
        .collect();

    Ok(Json(response))
}

/// GET /api/users/{id}
pub async fn get_user_handler(
    _admin: AdminUser,
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<Json<UserResponse>, ApiError> {
    let user = find_user(&state, id)?;
    Ok(Json(user_response(&state, user)?))
}

/// POST /api/users
pub(crate) async fn create_user_handler(
    AdminUser(admin): AdminUser,
    State(state): State<ApiState>,
    body: Result<Json<CreateUserRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreatedUserResponse>), ApiError> {
    if !state.limiters.api.allow(&admin.id) {
        return Err(ApiError::rate_limited());
    }
    let Json(body) = body?;

    let role = parse_role(&body.role)?;
    let email = normalize_login_email(&body.email);
    let name = body.name.trim();
    validate_login_email(&email)?;
    validate_name(name)?;
    validate_password(body.password.as_ref())?;

    let (password_hash, status, invite) = match body.password {
        Some(password) => (hash_password(password).await?, UserStatus::Active, None),
        None => (
            unusable_password_hash().await?,
            UserStatus::Invited,
            Some(NewInvite::generate(admin.id)),
        ),
    };

    let _writes = state.identity_writes.lock().await;
    if state
        .tracker
        .get_user_by_email(&email)
        .map_err(ApiError::storage)?
        .is_some()
    {
        return Err(ApiError::duplicate_email());
    }

    let identities = match body.identities {
        Some(update) => {
            registry::reload(&state.registry, state.tracker.as_ref());
            let plan = plan_update(&state.registry, None, &email, None, update)
                .map_err(ApiError::invalid_identities)?;
            let conflicts = find_conflicts(&state.registry, std::slice::from_ref(&plan));
            if !conflicts.is_empty() {
                return Err(ApiError::identity_conflict(conflicts));
            }
            Some(plan.write)
        }
        None => None,
    };

    let user_ids = state
        .tracker
        .save_directory(&[DirectoryWrite {
            user: DirectoryUser::New {
                email,
                name: name.to_string(),
                role,
                password_hash,
                status,
            },
            identities,
            invite: invite.as_ref().map(|invite| invite.write.clone()),
        }])
        .map_err(ApiError::user_write)?;
    registry::reload(&state.registry, state.tracker.as_ref());

    let user_id = user_ids.first().copied().ok_or_else(ApiError::internal)?;
    let user = find_user(&state, user_id)?;
    let invite = invite.map(|invite| invite.response(&state.config));
    let invite_expires_at = invite.as_ref().map(|invite| invite.expires_at.clone());

    Ok((
        StatusCode::CREATED,
        Json(CreatedUserResponse {
            user: UserResponse::new(user, invite_expires_at),
            invite,
        }),
    ))
}

/// The columns an admin update changes, with the activation of an invited user.
struct UserUpdate<'a> {
    email: Option<&'a str>,
    password_hash: Option<&'a str>,
    name: Option<&'a str>,
    role: Option<Role>,
    activation_hash: Option<&'a str>,
}

/// Write the column changes before the activation: the column write is the one a login
/// email taken in the meantime can fail, and it must fail before the user is activated.
fn apply_user_update(
    tracker: &dyn FixAttemptTracker,
    id: i64,
    update: &UserUpdate<'_>,
) -> Result<(), ApiError> {
    tracker
        .update_user(
            id,
            update.email,
            update.password_hash,
            update.name,
            update.role,
            None,
        )
        .map_err(ApiError::user_write)?;
    if let Some(hash) = update.activation_hash {
        tracker.activate_user(id, hash).map_err(ApiError::storage)?;
    }
    Ok(())
}

/// PUT /api/users/{id}
///
/// A password set on an invited user activates them and revokes their invite.
pub async fn update_user_handler(
    AdminUser(admin): AdminUser,
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    body: Result<Json<UpdateUserRequest>, JsonRejection>,
) -> Result<Json<UserResponse>, ApiError> {
    if !state.limiters.api.allow(&admin.id) {
        return Err(ApiError::rate_limited());
    }
    let Json(body) = body?;

    let role = body.role.as_deref().map(parse_role).transpose()?;
    let email = body.email.as_deref().map(normalize_login_email);
    let name = body.name.as_deref().map(str::trim);
    if let Some(email) = &email {
        validate_login_email(email)?;
    }
    if let Some(name) = name {
        validate_name(name)?;
    }
    validate_password(body.password.as_ref())?;

    let password_hash = match body.password {
        Some(password) => Some(hash_password(password).await?),
        None => None,
    };

    let _writes = state.identity_writes.lock().await;
    let user = find_user(&state, id)?;
    let email = email.filter(|email| *email != user.email);
    if let Some(email) = &email {
        let holder = state
            .tracker
            .get_user_by_email(email)
            .map_err(ApiError::storage)?;
        if holder.is_some_and(|holder| holder.id != id) {
            return Err(ApiError::duplicate_email());
        }
    }

    let activates = user.status == UserStatus::Invited && password_hash.is_some();
    let (password_hash, activation_hash) = if activates {
        (None, password_hash.as_deref())
    } else {
        (password_hash.as_deref(), None)
    };
    apply_user_update(
        state.tracker.as_ref(),
        id,
        &UserUpdate {
            email: email.as_deref(),
            password_hash,
            name,
            role,
            activation_hash,
        },
    )?;
    if email.is_some() || activates {
        registry::reload(&state.registry, state.tracker.as_ref());
    }

    let user = find_user(&state, id)?;
    Ok(Json(user_response(&state, user)?))
}

/// DELETE /api/users/{id}
pub async fn delete_user_handler(
    AdminUser(admin): AdminUser,
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<Json<MessageResponse>, ApiError> {
    if !state.limiters.api.allow(&admin.id) {
        return Err(ApiError::rate_limited());
    }

    if admin.id == id {
        return Err(ApiError::bad_request("You cannot delete yourself"));
    }

    let _writes = state.identity_writes.lock().await;
    let _ = state.tracker.delete_user_sessions(id);

    let deleted = state.tracker.delete_user(id).map_err(ApiError::storage)?;

    if !deleted {
        return Err(ApiError::user_not_found());
    }
    registry::reload(&state.registry, state.tracker.as_ref());

    Ok(Json(MessageResponse {
        message: "User deleted".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::routes::create_api_router;
    use axum::body::Body;
    use axum::http::Request;
    use claudear_config::config::{
        AgentConfig, AskConfig, CascadeConfig, CodeIndexConfig, Config, DeployQaConfig,
        IssuesConfig, LearningConfig, NotifiersConfig, PrDistributionConfig, PrioritisationConfig,
        RegressionConfig, RetryConfig, ScmConfig,
    };
    use claudear_storage::SqliteTracker;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use tower_cookies::CookieManagerLayer;

    fn test_config() -> Config {
        Config {
            workspace: "/tmp/repos".into(),
            known_orgs: vec!["test-org".to_string()],
            auto_discover_paths: vec![],
            poll_interval_ms: 300_000,
            webhook_port: 3100,
            bind_address: "127.0.0.1".to_string(),
            db_path: ":memory:".into(),
            max_issues_per_cycle: 5,
            max_concurrent: 1,
            processing_delay_ms: 5000,
            max_activity_entries: 100,
            ipc_timeout_secs: 30,
            debug_logging: false,
            agent: AgentConfig::default(),
            scm: ScmConfig::default(),
            issues: IssuesConfig::default(),
            notifiers: NotifiersConfig::default(),
            ask: AskConfig::default(),
            retry: RetryConfig::default(),
            regression: RegressionConfig::default(),
            deploy_qa: DeployQaConfig::default(),
            pr_distribution: PrDistributionConfig::default(),
            cascade: CascadeConfig::default(),
            users: std::collections::HashMap::new(),
            learning: LearningConfig::default(),
            prioritisation: PrioritisationConfig::default(),
            code_index: CodeIndexConfig::default(),
            retrieval_eval: Default::default(),
            evaluation: claudear_config::config::EvaluationConfig::default(),
            storage_dir: "/tmp/claudear-storage".into(),
            dashboard: claudear_config::config::DashboardConfig::default(),
            llm: claudear_config::config::LlmModelConfig::default(),
            chat: claudear_config::config::ChatConfig::default(),
            tls: claudear_config::config::TlsConfig::default(),
            embedding: claudear_config::config::EmbeddingModelConfig::default(),
            qa: claudear_config::config::QaConfig::default(),
            knowledgebase: claudear_config::config::KnowledgebasesConfig::default(),
            reports: claudear_config::config::ReportsConfig::default(),
            mcp_server: claudear_config::config::McpSearchServerConfig::default(),
        }
    }

    /// Create router + tracker Arc for testing.
    fn create_test_app() -> (
        axum::Router,
        std::sync::Arc<dyn claudear_storage::FixAttemptTracker>,
    ) {
        let tracker: std::sync::Arc<dyn claudear_storage::FixAttemptTracker> =
            std::sync::Arc::new(SqliteTracker::in_memory().unwrap());
        let indexing_rx = tracker.subscribe_indexing_progress();
        let router = create_api_router(
            test_config(),
            tracker.clone(),
            std::path::PathBuf::from("claudear.toml"),
            indexing_rx,
        )
        .layer(CookieManagerLayer::new());
        (router, tracker)
    }

    /// Seed an admin user and return (user_id, session_token).
    fn seed_admin(
        tracker: &std::sync::Arc<dyn claudear_storage::FixAttemptTracker>,
    ) -> (i64, String) {
        let password_hash = bcrypt::hash("password", 4).unwrap();
        let user_id = tracker
            .create_user("admin@test.com", &password_hash, "Admin User", Role::Admin)
            .unwrap();
        let token = tracker
            .create_session(user_id, "2099-12-31 23:59:59")
            .unwrap();
        (user_id, token)
    }

    /// Seed a viewer user and return (user_id, session_token).
    fn seed_viewer(
        tracker: &std::sync::Arc<dyn claudear_storage::FixAttemptTracker>,
    ) -> (i64, String) {
        let password_hash = bcrypt::hash("password", 4).unwrap();
        let user_id = tracker
            .create_user(
                "viewer@test.com",
                &password_hash,
                "Viewer User",
                Role::Viewer,
            )
            .unwrap();
        let token = tracker
            .create_session(user_id, "2099-12-31 23:59:59")
            .unwrap();
        (user_id, token)
    }

    #[tokio::test]
    async fn test_login_success() {
        let (router, tracker) = create_test_app();

        // Seed a user (not via seed_admin, to test login flow directly)
        let password_hash = bcrypt::hash("secret123", 4).unwrap();
        tracker
            .create_user("user@test.com", &password_hash, "Test User", Role::Admin)
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"email":"user@test.com","password":"secret123"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Check that a set-cookie header is present
        let set_cookie = response.headers().get("set-cookie");
        assert!(set_cookie.is_some(), "Expected set-cookie header");
        let cookie_val = set_cookie.unwrap().to_str().unwrap();
        assert!(
            cookie_val.contains("claudear_session"),
            "Cookie should contain claudear_session"
        );

        // Check response body contains user data
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("user@test.com"));
        assert!(body_str.contains("Test User"));
    }

    #[tokio::test]
    async fn test_login_wrong_password() {
        let (router, tracker) = create_test_app();

        let password_hash = bcrypt::hash("correct_password", 4).unwrap();
        tracker
            .create_user("user@test.com", &password_hash, "Test User", Role::Admin)
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"email":"user@test.com","password":"wrong_password"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_login_unknown_email() {
        let (router, _tracker) = create_test_app();

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"email":"nobody@test.com","password":"whatever"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_me_authenticated() {
        let (router, tracker) = create_test_app();
        let (_user_id, token) = seed_admin(&tracker);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("admin@test.com"));
        assert!(body_str.contains("Admin User"));
        assert!(body_str.contains("admin"));
    }

    #[tokio::test]
    async fn test_me_unauthenticated() {
        let (router, _tracker) = create_test_app();

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_create_user_admin_only() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin(&tracker);

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"email":"new@test.com","password":"newpass1!","name":"New User","role":"viewer"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("new@test.com"));
        assert!(body_str.contains("New User"));
        assert!(body_str.contains("viewer"));
    }

    #[tokio::test]
    async fn test_viewer_cannot_create_user() {
        let (router, tracker) = create_test_app();
        let (_viewer_id, token) = seed_viewer(&tracker);

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"email":"new@test.com","password":"newpass1!","name":"New User","role":"viewer"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_cannot_delete_self() {
        let (router, tracker) = create_test_app();
        let (admin_id, token) = seed_admin(&tracker);

        let response = router
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/users/{}", admin_id))
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// Seed an admin user with a custom email and return (user_id, session_token).
    fn seed_admin_with_email(
        tracker: &std::sync::Arc<dyn claudear_storage::FixAttemptTracker>,
        email: &str,
    ) -> (i64, String) {
        let password_hash = bcrypt::hash("password", 4).unwrap();
        let user_id = tracker
            .create_user(email, &password_hash, "Admin User", Role::Admin)
            .unwrap();
        let token = tracker
            .create_session(user_id, "2099-12-31 23:59:59")
            .unwrap();
        (user_id, token)
    }

    #[tokio::test]
    async fn test_create_user_invalid_role() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-create-role@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"email":"x@test.com","password":"longpassword","name":"X","role":"superuser"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_create_user_empty_email() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-create-email@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"email":"","password":"longpassword","name":"X","role":"viewer"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_create_user_email_without_at() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-create-noat@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"email":"bademail","password":"longpassword","name":"X","role":"viewer"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_create_user_password_too_short() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-create-pw@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"email":"x@test.com","password":"short","name":"X","role":"viewer"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_create_user_empty_name() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-create-name@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"email":"x@test.com","password":"longpassword","name":"","role":"viewer"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_create_user_duplicate_email() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-create-dup@test.com");

        // Create a user first
        let hash = bcrypt::hash("password", 4).unwrap();
        tracker
            .create_user("dup@test.com", &hash, "Dup User", Role::Viewer)
            .unwrap();

        // Try to create another user with the same email
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"email":"dup@test.com","password":"longpassword","name":"Another","role":"viewer"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_update_user_invalid_role() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-update-role@test.com");

        // Create a target user to update
        let hash = bcrypt::hash("password", 4).unwrap();
        let target_id = tracker
            .create_user("target-update@test.com", &hash, "Target", Role::Viewer)
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/users/{}", target_id))
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(r#"{"role":"superuser"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_user_not_found() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-update-nf@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/users/99999")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(r#"{"name":"Updated"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_delete_user_success() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-del-ok@test.com");

        // Create a target user to delete
        let hash = bcrypt::hash("password", 4).unwrap();
        let target_id = tracker
            .create_user("target-del@test.com", &hash, "Target", Role::Viewer)
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/users/{}", target_id))
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("User deleted"));
    }

    #[tokio::test]
    async fn test_delete_user_not_found() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-del-nf@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/users/99999")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_users_as_admin() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-list@test.com");

        // Create an additional user
        let hash = bcrypt::hash("password", 4).unwrap();
        tracker
            .create_user("extra-list@test.com", &hash, "Extra User", Role::Viewer)
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        // Should contain both users
        assert!(body_str.contains("admin-list@test.com"));
        assert!(body_str.contains("extra-list@test.com"));

        // Verify the response parses as a JSON array
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&body_str).unwrap();
        assert_eq!(parsed.len(), 2);
    }

    #[tokio::test]
    async fn test_get_user_by_id() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-getuser@test.com");

        // Create a target user
        let hash = bcrypt::hash("password", 4).unwrap();
        let target_id = tracker
            .create_user("target-get@test.com", &hash, "Target Get", Role::Viewer)
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!("/api/users/{}", target_id))
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("target-get@test.com"));
        assert!(body_str.contains("Target Get"));
        assert!(body_str.contains("viewer"));
    }

    #[tokio::test]
    async fn test_get_user_not_found() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-getuser-nf@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/users/99999")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_logout_clears_session_cookie() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-logout@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/logout")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Verify response body
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
        assert!(body_str.contains("Logged out"));
    }

    #[test]
    fn test_auth_user_response_from_auth_user() {
        let user = AuthUser {
            id: 42,
            email: "convert@test.com".to_string(),
            name: "Convert User".to_string(),
            role: Role::Admin,
            avatar_url: None,
        };

        let response = AuthUserResponse::from(&user);

        assert_eq!(response.id, 42);
        assert_eq!(response.email, "convert@test.com");
        assert_eq!(response.name, "Convert User");
        assert_eq!(response.role, Role::Admin);
    }

    #[test]
    fn test_extract_client_ip_from_x_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 5.6.7.8".parse().unwrap());
        assert_eq!(extract_client_ip(&headers), "1.2.3.4");
    }

    #[test]
    fn test_extract_client_ip_from_x_forwarded_for_single() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "10.0.0.1".parse().unwrap());
        assert_eq!(extract_client_ip(&headers), "10.0.0.1");
    }

    #[test]
    fn test_extract_client_ip_from_x_forwarded_for_trims_whitespace() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "  192.168.1.1  , 10.0.0.1".parse().unwrap(),
        );
        assert_eq!(extract_client_ip(&headers), "192.168.1.1");
    }

    #[test]
    fn test_extract_client_ip_from_x_real_ip() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "9.8.7.6".parse().unwrap());
        assert_eq!(extract_client_ip(&headers), "9.8.7.6");
    }

    #[test]
    fn test_extract_client_ip_from_x_real_ip_trims() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "  10.10.10.10  ".parse().unwrap());
        assert_eq!(extract_client_ip(&headers), "10.10.10.10");
    }

    #[test]
    fn test_extract_client_ip_prefers_x_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.1.1.1".parse().unwrap());
        headers.insert("x-real-ip", "2.2.2.2".parse().unwrap());
        assert_eq!(extract_client_ip(&headers), "1.1.1.1");
    }

    #[test]
    fn test_extract_client_ip_falls_back_to_unknown() {
        let headers = HeaderMap::new();
        assert_eq!(extract_client_ip(&headers), "unknown");
    }

    #[test]
    fn test_extract_client_ip_empty_x_forwarded_for_falls_through() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "".parse().unwrap());
        headers.insert("x-real-ip", "3.3.3.3".parse().unwrap());
        assert_eq!(extract_client_ip(&headers), "3.3.3.3");
    }

    #[test]
    fn test_extract_client_ip_empty_both_returns_unknown() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "".parse().unwrap());
        headers.insert("x-real-ip", "".parse().unwrap());
        assert_eq!(extract_client_ip(&headers), "unknown");
    }

    #[test]
    fn test_extract_client_ip_whitespace_only_x_real_ip_returns_unknown() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "   ".parse().unwrap());
        assert_eq!(extract_client_ip(&headers), "unknown");
    }

    #[test]
    fn test_auth_user_response_from_auth_user_with_avatar() {
        let user = AuthUser {
            id: 99,
            email: "avatar@test.com".to_string(),
            name: "Avatar User".to_string(),
            role: Role::Viewer,
            avatar_url: Some("/avatars/99_abc.png".to_string()),
        };

        let response = AuthUserResponse::from(&user);

        assert_eq!(response.id, 99);
        assert_eq!(response.email, "avatar@test.com");
        assert_eq!(response.name, "Avatar User");
        assert_eq!(response.role, Role::Viewer);
        assert_eq!(response.avatar_url.as_deref(), Some("/avatars/99_abc.png"));
    }

    #[tokio::test]
    async fn test_logout_without_session_cookie() {
        let (router, _tracker) = create_test_app();

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/logout")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Logout should still succeed even without a cookie
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_update_user_short_password() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-update-shortpw@test.com");

        let hash = bcrypt::hash("password", 4).unwrap();
        let target_id = tracker
            .create_user("target-update-pw@test.com", &hash, "Target", Role::Viewer)
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/users/{}", target_id))
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(r#"{"password":"short"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_user_password_too_long() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-update-longpw@test.com");

        let hash = bcrypt::hash("password", 4).unwrap();
        let target_id = tracker
            .create_user(
                "target-update-longpw@test.com",
                &hash,
                "Target",
                Role::Viewer,
            )
            .unwrap();

        let long_pw = "a".repeat(73);
        let body = serde_json::json!({"password": long_pw});

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/users/{}", target_id))
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_user_with_valid_password() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-update-validpw@test.com");

        let hash = bcrypt::hash("password", 4).unwrap();
        let target_id = tracker
            .create_user(
                "target-update-validpw@test.com",
                &hash,
                "Target",
                Role::Viewer,
            )
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/users/{}", target_id))
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(r#"{"password":"newlongpassword123"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_update_user_name() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-update-name@test.com");

        let hash = bcrypt::hash("password", 4).unwrap();
        let target_id = tracker
            .create_user(
                "target-name-upd@test.com",
                &hash,
                "Original Name",
                Role::Viewer,
            )
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/users/{}", target_id))
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(r#"{"name":"Updated Name"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("Updated Name"));
    }

    #[tokio::test]
    async fn test_update_user_role_to_admin() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-role-to-admin@test.com");

        let hash = bcrypt::hash("password", 4).unwrap();
        let target_id = tracker
            .create_user("target-role-upd@test.com", &hash, "Target", Role::Viewer)
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/users/{}", target_id))
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(r#"{"role":"admin"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("\"role\":\"admin\""));
    }

    #[tokio::test]
    async fn test_create_user_password_too_long() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-create-longpw@test.com");

        let long_pw = "b".repeat(73);
        let body = serde_json::json!({
            "email": "longpw@test.com",
            "password": long_pw,
            "name": "Long PW User",
            "role": "viewer"
        });

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_create_user_whitespace_name() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-create-wsname@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"email":"x@test.com","password":"longpassword","name":"   ","role":"viewer"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_login_sends_x_forwarded_for_ip() {
        let (router, tracker) = create_test_app();
        let password_hash = bcrypt::hash("secret123", 4).unwrap();
        tracker
            .create_user("iptest@test.com", &password_hash, "IP User", Role::Admin)
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .header("x-forwarded-for", "1.2.3.4")
                    .body(Body::from(
                        r#"{"email":"iptest@test.com","password":"secret123"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_update_profile_name_only() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-profile-name@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/profile")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(r#"{"name":"New Profile Name"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("New Profile Name"));
    }

    #[tokio::test]
    async fn test_update_profile_empty_name_rejected() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) =
            seed_admin_with_email(&tracker, "admin-profile-emptyname@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/profile")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(r#"{"name":"  "}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_profile_password_too_short() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-profile-shortpw@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/profile")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"password":"short","current_password":"password"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_profile_password_too_long() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-profile-longpw@test.com");

        let long_pw = "c".repeat(73);
        let body = serde_json::json!({
            "password": long_pw,
            "current_password": "password"
        });

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/profile")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_profile_password_no_current_password() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-profile-nocurpw@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/profile")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(r#"{"password":"newpassword123"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_profile_password_wrong_current_password() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) =
            seed_admin_with_email(&tracker, "admin-profile-wrongcurpw@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/profile")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"password":"newpassword123","current_password":"wrongpassword"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_update_profile_password_success() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) =
            seed_admin_with_email(&tracker, "admin-profile-pwsuccess@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/profile")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"password":"newpassword123","current_password":"password"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_update_profile_unauthenticated() {
        let (router, _tracker) = create_test_app();

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/profile")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"Nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_viewer_cannot_list_users() {
        let (router, tracker) = create_test_app();
        let (_viewer_id, token) = seed_viewer(&tracker);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_viewer_cannot_get_user() {
        let (router, tracker) = create_test_app();
        let (_viewer_id, token) = seed_viewer(&tracker);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/users/1")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_viewer_cannot_update_user() {
        let (router, tracker) = create_test_app();
        let (_viewer_id, token) = seed_viewer(&tracker);

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/users/1")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(r#"{"name":"Hacked"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_viewer_cannot_delete_user() {
        let (router, tracker) = create_test_app();
        let (_viewer_id, token) = seed_viewer(&tracker);

        let response = router
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/users/1")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_unauthenticated_list_users() {
        let (router, _tracker) = create_test_app();

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_unauthenticated_create_user() {
        let (router, _tracker) = create_test_app();

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"email":"x@test.com","password":"longpassword","name":"X","role":"viewer"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_invalid_session_token() {
        let (router, _tracker) = create_test_app();

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header("cookie", "claudear_session=invalid-token-12345")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_create_user_admin_role() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-create-adminrole@test.com");

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(
                        r#"{"email":"newadmin@test.com","password":"longpassword","name":"New Admin","role":"admin"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("\"role\":\"admin\""));
    }

    #[tokio::test]
    async fn test_login_response_body_structure() {
        let (router, tracker) = create_test_app();
        let password_hash = bcrypt::hash("testpass1", 4).unwrap();
        tracker
            .create_user(
                "struct@test.com",
                &password_hash,
                "Struct User",
                Role::Viewer,
            )
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"email":"struct@test.com","password":"testpass1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(parsed["user"].is_object());
        assert_eq!(parsed["user"]["email"], "struct@test.com");
        assert_eq!(parsed["user"]["name"], "Struct User");
        assert_eq!(parsed["user"]["role"], "viewer");
    }

    #[tokio::test]
    async fn test_update_user_email() {
        let (router, tracker) = create_test_app();
        let (_admin_id, token) = seed_admin_with_email(&tracker, "admin-update-email@test.com");

        let hash = bcrypt::hash("password", 4).unwrap();
        let target_id = tracker
            .create_user("original-email@test.com", &hash, "Email User", Role::Viewer)
            .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/users/{}", target_id))
                    .header("content-type", "application/json")
                    .header("cookie", format!("claudear_session={}", token))
                    .body(Body::from(r#"{"email":"new-email@test.com"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("new-email@test.com"));
    }

    #[tokio::test]
    async fn test_upload_avatar_no_auth() {
        let (router, _tracker) = create_test_app();

        // No auth cookie -> should be rejected
        let boundary = "---TestBoundary123";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"test.png\"\r\nContent-Type: image/png\r\n\r\nfakedata\r\n--{boundary}--\r\n"
        );

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/avatar")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={}", boundary),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should be 401 (no auth)
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_upload_avatar_with_valid_png() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage_dir = temp_dir.path().to_path_buf();
        let avatars_dir = storage_dir.join("avatars");
        std::fs::create_dir_all(&avatars_dir).unwrap();

        let tracker: std::sync::Arc<dyn claudear_storage::FixAttemptTracker> =
            std::sync::Arc::new(SqliteTracker::in_memory().unwrap());

        let mut config = test_config();
        config.storage_dir = storage_dir;

        let password_hash = bcrypt::hash("password", 4).unwrap();
        tracker
            .create_user(
                "avatar@test.com",
                &password_hash,
                "Avatar User",
                Role::Admin,
            )
            .unwrap();
        let token = tracker.create_session(1, "2099-12-31 23:59:59").unwrap();

        let indexing_rx = tracker.subscribe_indexing_progress();
        let router = create_api_router(
            config,
            tracker.clone(),
            std::path::PathBuf::from("claudear.toml"),
            indexing_rx,
        )
        .layer(CookieManagerLayer::new());

        // Build a valid PNG (minimal PNG header)
        let mut png_data: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png_data.extend_from_slice(&[0u8; 100]); // Some padding

        let boundary = "----TestBoundary456";
        let mut body_bytes: Vec<u8> = Vec::new();
        body_bytes.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"test.png\"\r\nContent-Type: image/png\r\n\r\n"
            ).as_bytes()
        );
        body_bytes.extend_from_slice(&png_data);
        body_bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/avatar")
                    .header("cookie", format!("claudear_session={}", token))
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={}", boundary),
                    )
                    .body(Body::from(body_bytes))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(resp["avatar_url"]
            .as_str()
            .unwrap()
            .starts_with("/avatars/"));
        assert!(resp["avatar_url"].as_str().unwrap().ends_with(".png"));
    }

    #[tokio::test]
    async fn test_upload_avatar_invalid_content_type() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage_dir = temp_dir.path().to_path_buf();
        let avatars_dir = storage_dir.join("avatars");
        std::fs::create_dir_all(&avatars_dir).unwrap();

        let tracker: std::sync::Arc<dyn claudear_storage::FixAttemptTracker> =
            std::sync::Arc::new(SqliteTracker::in_memory().unwrap());

        let mut config = test_config();
        config.storage_dir = storage_dir;

        let password_hash = bcrypt::hash("password", 4).unwrap();
        tracker
            .create_user(
                "avatar2@test.com",
                &password_hash,
                "Avatar User",
                Role::Admin,
            )
            .unwrap();
        let token = tracker.create_session(1, "2099-12-31 23:59:59").unwrap();

        let indexing_rx = tracker.subscribe_indexing_progress();
        let router = create_api_router(
            config,
            tracker.clone(),
            std::path::PathBuf::from("claudear.toml"),
            indexing_rx,
        )
        .layer(CookieManagerLayer::new());

        // Upload a text/plain file (not allowed)
        let boundary = "----TestBoundary789";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"test.txt\"\r\nContent-Type: text/plain\r\n\r\nHello World\r\n--{boundary}--\r\n"
        );

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/avatar")
                    .header("cookie", format!("claudear_session={}", token))
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={}", boundary),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_upload_avatar_wrong_magic_bytes() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage_dir = temp_dir.path().to_path_buf();
        let avatars_dir = storage_dir.join("avatars");
        std::fs::create_dir_all(&avatars_dir).unwrap();

        let tracker: std::sync::Arc<dyn claudear_storage::FixAttemptTracker> =
            std::sync::Arc::new(SqliteTracker::in_memory().unwrap());

        let mut config = test_config();
        config.storage_dir = storage_dir;

        let password_hash = bcrypt::hash("password", 4).unwrap();
        tracker
            .create_user(
                "avatar3@test.com",
                &password_hash,
                "Avatar User",
                Role::Admin,
            )
            .unwrap();
        let token = tracker.create_session(1, "2099-12-31 23:59:59").unwrap();

        let indexing_rx = tracker.subscribe_indexing_progress();
        let router = create_api_router(
            config,
            tracker.clone(),
            std::path::PathBuf::from("claudear.toml"),
            indexing_rx,
        )
        .layer(CookieManagerLayer::new());

        // Claim image/png but provide wrong magic bytes
        let fake_data = b"not a real png file at all";
        let boundary = "----TestBoundaryMagic";
        let mut body_bytes: Vec<u8> = Vec::new();
        body_bytes.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"fake.png\"\r\nContent-Type: image/png\r\n\r\n"
            ).as_bytes()
        );
        body_bytes.extend_from_slice(fake_data);
        body_bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/avatar")
                    .header("cookie", format!("claudear_session={}", token))
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={}", boundary),
                    )
                    .body(Body::from(body_bytes))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_upload_avatar_no_avatar_field() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage_dir = temp_dir.path().to_path_buf();
        let avatars_dir = storage_dir.join("avatars");
        std::fs::create_dir_all(&avatars_dir).unwrap();

        let tracker: std::sync::Arc<dyn claudear_storage::FixAttemptTracker> =
            std::sync::Arc::new(SqliteTracker::in_memory().unwrap());

        let mut config = test_config();
        config.storage_dir = storage_dir;

        let password_hash = bcrypt::hash("password", 4).unwrap();
        tracker
            .create_user(
                "avatar4@test.com",
                &password_hash,
                "Avatar User",
                Role::Admin,
            )
            .unwrap();
        let token = tracker.create_session(1, "2099-12-31 23:59:59").unwrap();

        let indexing_rx = tracker.subscribe_indexing_progress();
        let router = create_api_router(
            config,
            tracker.clone(),
            std::path::PathBuf::from("claudear.toml"),
            indexing_rx,
        )
        .layer(CookieManagerLayer::new());

        // Upload with wrong field name (not "avatar")
        let boundary = "----TestBoundaryNoField";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"photo\"; filename=\"test.png\"\r\nContent-Type: image/png\r\n\r\nfakedata\r\n--{boundary}--\r\n"
        );

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/avatar")
                    .header("cookie", format!("claudear_session={}", token))
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={}", boundary),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        // No "avatar" field found => BAD_REQUEST
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_upload_avatar_valid_jpeg() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage_dir = temp_dir.path().to_path_buf();
        let avatars_dir = storage_dir.join("avatars");
        std::fs::create_dir_all(&avatars_dir).unwrap();

        let tracker: std::sync::Arc<dyn claudear_storage::FixAttemptTracker> =
            std::sync::Arc::new(SqliteTracker::in_memory().unwrap());

        let mut config = test_config();
        config.storage_dir = storage_dir;

        let password_hash = bcrypt::hash("password", 4).unwrap();
        tracker
            .create_user(
                "avatar5@test.com",
                &password_hash,
                "Avatar User",
                Role::Admin,
            )
            .unwrap();
        let token = tracker.create_session(1, "2099-12-31 23:59:59").unwrap();

        let indexing_rx = tracker.subscribe_indexing_progress();
        let router = create_api_router(
            config,
            tracker.clone(),
            std::path::PathBuf::from("claudear.toml"),
            indexing_rx,
        )
        .layer(CookieManagerLayer::new());

        // Build minimal JPEG header
        let mut jpeg_data: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpeg_data.extend_from_slice(&[0u8; 100]);

        let boundary = "----TestBoundaryJpeg";
        let mut body_bytes: Vec<u8> = Vec::new();
        body_bytes.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"photo.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n"
            ).as_bytes()
        );
        body_bytes.extend_from_slice(&jpeg_data);
        body_bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/avatar")
                    .header("cookie", format!("claudear_session={}", token))
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={}", boundary),
                    )
                    .body(Body::from(body_bytes))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(resp["avatar_url"].as_str().unwrap().ends_with(".jpg"));
    }

    fn session_cookie(token: &str) -> String {
        format!("claudear_session={token}")
    }

    #[tokio::test]
    async fn test_minting_a_users_token_is_admin_only() {
        let (router, tracker) = create_test_app();
        let (admin_id, admin_tok) = seed_admin(&tracker);
        let (viewer_id, viewer_tok) = seed_viewer(&tracker);

        // Viewer cannot mint a token for another user (AdminUser extractor → 403).
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/users/{admin_id}/tokens"))
                    .header("content-type", "application/json")
                    .header("cookie", session_cookie(&viewer_tok))
                    .body(Body::from(r#"{"name":"x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Admin can, and receives the one-time secret.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/users/{viewer_id}/tokens"))
                    .header("content-type", "application/json")
                    .header("cookie", session_cookie(&admin_tok))
                    .body(Body::from(r#"{"name":"ci"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["secret"].as_str().unwrap().starts_with("cldr_"));
    }

    #[tokio::test]
    async fn test_token_revocation_is_owner_scoped() {
        let (router, tracker) = create_test_app();
        let (admin_id, admin_tok) = seed_admin(&tracker);
        let (viewer_id, viewer_tok) = seed_viewer(&tracker);

        let admin_token = tracker
            .create_api_token(admin_id, "adm", "hash_admin", "cldr_adm", None)
            .unwrap();

        // A viewer cannot revoke another user's token: the delete is owner-scoped,
        // so it reports 404 and leaves the token in place.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/tokens/{}", admin_token.id))
                    .header("cookie", session_cookie(&viewer_tok))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(tracker.list_api_tokens(admin_id).unwrap().len(), 1);

        // An admin may revoke anyone's token.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/tokens/{}", admin_token.id))
                    .header("cookie", session_cookie(&admin_tok))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(tracker.list_api_tokens(admin_id).unwrap().len(), 0);

        // A user may revoke their own token.
        let own = tracker
            .create_api_token(viewer_id, "v", "hash_v", "cldr_v", None)
            .unwrap();
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/tokens/{}", own.id))
                    .header("cookie", session_cookie(&viewer_tok))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(tracker.list_api_tokens(viewer_id).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_listing_all_tokens_is_admin_only() {
        let (router, tracker) = create_test_app();
        let (_admin_id, admin_tok) = seed_admin(&tracker);
        let (_viewer_id, viewer_tok) = seed_viewer(&tracker);

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/tokens/all")
                    .header("cookie", session_cookie(&viewer_tok))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/tokens/all")
                    .header("cookie", session_cookie(&admin_tok))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    mod directory_writes {
        use super::super::*;
        use crate::api::create_api_router_with_dashboard;
        use axum::body::Body;
        use axum::http::Request;
        use claudear_config::config::{Config, UserConfig};
        use claudear_config::users::UserRegistry;
        use claudear_storage::{FixAttemptTracker, SqliteTracker, UserStore};
        use http_body_util::BodyExt;
        use serde_json::{json, Value};
        use std::collections::HashMap;
        use std::sync::Arc;
        use tower::ServiceExt;
        use tower_cookies::CookieManagerLayer;

        struct TestApp {
            router: axum::Router,
            tracker: Arc<dyn FixAttemptTracker>,
            registry: UserRegistry,
            admin_token: String,
        }

        fn test_app() -> TestApp {
            test_app_on(Arc::new(
                SqliteTracker::in_memory().expect("in-memory tracker"),
            ))
        }

        fn test_app_on(tracker: Arc<dyn FixAttemptTracker>) -> TestApp {
            let admin = tracker
                .create_user(
                    "writes-admin@example.com",
                    "unused-hash",
                    "Admin",
                    Role::Admin,
                )
                .expect("create admin");
            let admin_token = tracker
                .create_session(admin, "2099-12-31 23:59:59")
                .expect("create session");
            let config = Config {
                users: HashMap::from([(
                    "dana".to_string(),
                    UserConfig {
                        github_usernames: vec!["dana-gh".to_string()],
                        email: Some("writes-dana@example.com".to_string()),
                        ..UserConfig::default()
                    },
                )]),
                storage_dir: std::env::temp_dir().join("claudear-auth-writes-tests"),
                ..Config::default()
            };
            let registry = UserRegistry::new(config.users.clone());
            let router = create_api_router_with_dashboard(
                config,
                tracker.clone(),
                std::path::PathBuf::from("claudear.toml"),
                tracker.subscribe_indexing_progress(),
                None,
                registry.clone(),
            )
            .layer(CookieManagerLayer::new());
            TestApp {
                router,
                tracker,
                registry,
                admin_token,
            }
        }

        async fn send(
            app: &TestApp,
            method: &str,
            uri: &str,
            session: Option<&str>,
            body: Value,
        ) -> (StatusCode, Vec<u8>) {
            let mut builder = Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json");
            if let Some(session) = session {
                builder = builder.header("cookie", format!("claudear_session={session}"));
            }
            let response = app
                .router
                .clone()
                .oneshot(builder.body(Body::from(body.to_string())).expect("request"))
                .await
                .expect("response");
            let status = response.status();
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("body")
                .to_bytes()
                .to_vec();
            (status, bytes)
        }

        fn json(bytes: &[u8]) -> Value {
            serde_json::from_slice(bytes).expect("json body")
        }

        #[tokio::test]
        async fn create_email_change_and_delete_keep_the_registry_live() {
            let app = test_app();

            let (status, body) = send(
                &app,
                "POST",
                "/api/users",
                Some(&app.admin_token),
                json!({"email": "writes-erin@example.com", "name": "Erin", "role": "viewer",
                       "identities": {"github_usernames": ["erin-gh"]}}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
            let created = json(&body);
            assert_eq!(created["status"], "invited");
            let user_id = created["id"].as_i64().expect("user id");
            let console_key = format!("console-{user_id}");
            assert_eq!(
                app.registry
                    .resolve("github", "erin-gh")
                    .map(|person| person.slug),
                Some(console_key.clone())
            );
            assert!(app.registry.users().contains_key(&console_key));

            let (status, body) = send(
                &app,
                "PUT",
                &format!("/api/users/{user_id}"),
                Some(&app.admin_token),
                json!({"email": "writes-dana@example.com"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(json(&body)["status"], "invited");
            assert_eq!(
                app.registry
                    .resolve("github", "erin-gh")
                    .map(|person| person.slug),
                Some("dana".to_string()),
                "the new login email links the console user to the config entry"
            );

            let (status, _) = send(
                &app,
                "DELETE",
                &format!("/api/users/{user_id}"),
                Some(&app.admin_token),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert!(app.registry.resolve("github", "erin-gh").is_none());
            assert_eq!(
                app.registry
                    .resolve("github", "dana-gh")
                    .map(|person| person.slug),
                Some("dana".to_string())
            );
        }

        #[tokio::test]
        async fn a_handle_storage_refuses_creates_no_user_and_is_a_409() {
            let directory = tempfile::tempdir().expect("temporary directory");
            let app = test_app_on(crate::api::fixture::tracker_refusing_handle_claims(
                directory.path(),
            ));
            let before = app.tracker.list_users().expect("list").len();

            let (status, body) = send(
                &app,
                "POST",
                "/api/users",
                Some(&app.admin_token),
                json!({"email": "writes-claim@example.com", "name": "Claim", "role": "viewer",
                       "identities": {"github_usernames": ["claimed-gh"]}}),
            )
            .await;

            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(
                json(&body),
                json!({"error": "Identities already belong to another person"})
            );
            assert_eq!(app.tracker.list_users().expect("list").len(), before);
        }

        #[tokio::test]
        async fn create_user_rejects_bad_input_without_writing() {
            let app = test_app();
            let before = app.tracker.list_users().expect("list").len();

            for body in [
                json!({"email": "writes-x@example.com", "name": "X", "role": "owner"}),
                json!({"email": "not-an-email", "name": "X", "role": "viewer"}),
                json!({"email": "writes-x@example.com", "name": " ", "role": "viewer"}),
                json!({"email": "writes-x@example.com", "name": "X", "role": "viewer",
                       "password": "short"}),
            ] {
                let (status, _) =
                    send(&app, "POST", "/api/users", Some(&app.admin_token), body).await;
                assert_eq!(status, StatusCode::BAD_REQUEST);
            }

            let (status, body) = send(
                &app,
                "POST",
                "/api/users",
                Some(&app.admin_token),
                json!({"email": "writes-x@example.com", "name": "X", "role": "viewer",
                       "identities": {"github_usernames": ["dana-gh"]}}),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(json(&body)["violations"][0]["field"], "github_usernames");

            let (status, body) = send(
                &app,
                "POST",
                "/api/users",
                Some(&app.admin_token),
                json!({"email": "writes-x@example.com", "name": "X", "role": "viewer",
                       "identities": {"discord_id": "1"}}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(json(&body)["violations"][0]["field"], "discord_id");

            assert_eq!(app.tracker.list_users().expect("list").len(), before);
        }

        #[tokio::test]
        async fn create_user_errors_are_json_and_emails_are_case_insensitive() {
            let app = test_app();
            let before = app.tracker.list_users().expect("list").len();

            for (body, error) in [
                (
                    json!({"email": "writes-y@example.com", "name": "Y", "role": "owner"}),
                    "Invalid role",
                ),
                (
                    json!({"email": "writes-y@example.com", "name": "Y", "role": "viewer",
                           "password": "short"}),
                    "Password must be 8-72 bytes",
                ),
                (
                    json!({"email": "not-an-email", "name": "Y", "role": "viewer"}),
                    "Email must be a valid email address",
                ),
                (
                    json!({"email": "writes-y@example.com", "name": " ", "role": "viewer"}),
                    "Name must not be empty",
                ),
                (
                    json!({"email": "writes-y@example.com"}),
                    "Invalid request body",
                ),
            ] {
                let (status, response) =
                    send(&app, "POST", "/api/users", Some(&app.admin_token), body).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
                assert_eq!(json(&response), json!({"error": error}));
            }

            let (status, body) = send(
                &app,
                "POST",
                "/api/users",
                Some(&app.admin_token),
                json!({"email": " Writes-Amy@Example.com ", "name": "Amy", "role": "viewer",
                       "password": "correct-horse"}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
            assert_eq!(json(&body)["email"], "writes-amy@example.com");

            let (status, body) = send(
                &app,
                "POST",
                "/api/users",
                Some(&app.admin_token),
                json!({"email": "writes-amy@example.com", "name": "Twin", "role": "viewer"}),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(
                json(&body),
                json!({"error": "A user with this email already exists"})
            );
            assert_eq!(app.tracker.list_users().expect("list").len(), before + 1);

            let (status, _) = send(
                &app,
                "POST",
                "/api/auth/login",
                None,
                json!({"email": "WRITES-AMY@example.com", "password": "correct-horse"}),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "login matches the email case-insensitively"
            );
        }

        #[tokio::test]
        async fn update_user_rejects_a_taken_email_in_any_case() {
            let app = test_app();
            let holder = app
                .tracker
                .create_user(
                    "writes-holder@example.com",
                    "unused-hash",
                    "Holder",
                    Role::Viewer,
                )
                .expect("create holder");
            let mover = app
                .tracker
                .create_user(
                    "writes-mover@example.com",
                    "unused-hash",
                    "Mover",
                    Role::Viewer,
                )
                .expect("create mover");

            let (status, body) = send(
                &app,
                "PUT",
                &format!("/api/users/{mover}"),
                Some(&app.admin_token),
                json!({"email": "Writes-Holder@Example.com"}),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(
                json(&body),
                json!({"error": "A user with this email already exists"})
            );

            let (status, body) = send(
                &app,
                "PUT",
                &format!("/api/users/{holder}"),
                Some(&app.admin_token),
                json!({"role": "owner"}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(json(&body), json!({"error": "Invalid role"}));

            let (status, body) = send(
                &app,
                "PUT",
                "/api/users/999999",
                Some(&app.admin_token),
                json!({"name": "Nobody"}),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(json(&body), json!({"error": "User not found"}));
        }

        #[test]
        fn a_taken_email_fails_the_update_before_the_invited_user_is_activated() {
            let tracker = SqliteTracker::in_memory().expect("in-memory tracker");
            tracker
                .create_user(
                    "writes-taken@example.com",
                    "unused-hash",
                    "Taken",
                    Role::Viewer,
                )
                .expect("create holder");
            let invited = tracker
                .save_directory(&[DirectoryWrite {
                    user: DirectoryUser::New {
                        email: "writes-invited@example.com".to_string(),
                        name: "Invited".to_string(),
                        role: Role::Viewer,
                        password_hash: "unused-hash".to_string(),
                        status: UserStatus::Invited,
                    },
                    identities: None,
                    invite: Some(claudear_storage::InviteWrite {
                        token_hash: "invite-hash".to_string(),
                        expires_at: "2099-12-31 23:59:59".to_string(),
                        created_by: 1,
                    }),
                }])
                .expect("create invited user")[0];

            let error = apply_user_update(
                &tracker,
                invited,
                &UserUpdate {
                    email: Some("writes-taken@example.com"),
                    password_hash: None,
                    name: None,
                    role: None,
                    activation_hash: Some("new-hash"),
                },
            )
            .expect_err("the email is taken");

            assert_eq!(error.status(), StatusCode::CONFLICT);
            let user = tracker
                .get_user_by_id(invited)
                .expect("lookup")
                .expect("user exists");
            assert_eq!(user.status, UserStatus::Invited);
            assert_eq!(user.password_hash, "unused-hash");
            assert!(tracker
                .get_open_invite_for_user(invited)
                .expect("lookup")
                .is_some());
        }
    }
}
