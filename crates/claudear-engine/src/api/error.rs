//! The error every user, identity, invite and directory endpoint returns: a status with
//! a JSON body `{"error", "violations"?}`. Bodies never carry a stored value.

use super::directory::DirectoryViolation;
use axum::{
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use claudear_core::identity::IdentityViolation;
use serde::Serialize;
use std::borrow::Cow;

/// How SQLite reports a second user with the same login email.
const DUPLICATE_EMAIL_CONSTRAINT: &str = "UNIQUE constraint failed: users.email";

/// Per-field problems attached to a 400 or 409.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum Violations {
    Identity(Vec<IdentityViolation>),
    Directory(Vec<DirectoryViolation>),
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: Cow<'static, str>,
    violations: Option<Violations>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    violations: Option<&'a Violations>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status,
            message: message.into(),
            violations: None,
        }
    }

    fn with_violations(status: StatusCode, message: &'static str, violations: Violations) -> Self {
        Self {
            violations: Some(violations),
            ..Self::new(status, message)
        }
    }

    pub(crate) fn bad_request(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub(crate) fn forbidden(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub(crate) fn not_found(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub(crate) fn conflict(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub(crate) fn unauthenticated() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Not authenticated")
    }

    pub(crate) fn invalid_token() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Invalid or expired token")
    }

    pub(crate) fn admin_required() -> Self {
        Self::forbidden("Admin access required")
    }

    pub(crate) fn rate_limited() -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "Too many requests")
    }

    pub(crate) fn invalid_body() -> Self {
        Self::bad_request("Invalid request body")
    }

    pub(crate) fn invalid_role() -> Self {
        Self::bad_request("Invalid role")
    }

    pub(crate) fn invalid_password() -> Self {
        Self::bad_request("Password must be 8-72 bytes")
    }

    pub(crate) fn user_not_found() -> Self {
        Self::not_found("User not found")
    }

    pub(crate) fn duplicate_email() -> Self {
        Self::conflict("A user with this email already exists")
    }

    pub(crate) fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
    }

    pub(crate) fn storage(error: claudear_core::error::Error) -> Self {
        tracing::error!(error = %error, "Storage operation failed");
        Self::internal()
    }

    /// A failed write to the users table: a login email another user already holds is a
    /// 409, which a concurrent writer can cause after the up-front duplicate check passed.
    pub(crate) fn user_write(error: claudear_core::error::Error) -> Self {
        match &error {
            claudear_core::error::Error::Database(message)
                if message.contains(DUPLICATE_EMAIL_CONSTRAINT) =>
            {
                Self::duplicate_email()
            }
            _ => Self::storage(error),
        }
    }

    pub(crate) fn invalid_identities(violations: Vec<IdentityViolation>) -> Self {
        Self::with_violations(
            StatusCode::BAD_REQUEST,
            "Invalid identities",
            Violations::Identity(violations),
        )
    }

    pub(crate) fn identity_conflict(violations: Vec<IdentityViolation>) -> Self {
        Self::with_violations(
            StatusCode::CONFLICT,
            "Identities already belong to another person",
            Violations::Identity(violations),
        )
    }

    pub(crate) fn invalid_directory(violations: Vec<DirectoryViolation>) -> Self {
        Self::with_violations(
            StatusCode::BAD_REQUEST,
            "Invalid directory entries",
            Violations::Directory(violations),
        )
    }

    pub(crate) fn directory_conflict(violations: Vec<DirectoryViolation>) -> Self {
        Self::with_violations(
            StatusCode::CONFLICT,
            "Identities already belong to another person",
            Violations::Directory(violations),
        )
    }

    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    #[cfg(test)]
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl From<JsonRejection> for ApiError {
    fn from(_: JsonRejection) -> Self {
        Self::invalid_body()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: &self.message,
            violations: self.violations.as_ref(),
        };
        (self.status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_core::identity::IdentityField;
    use claudear_storage::{SqliteTracker, UserStore};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};

    async fn render(error: ApiError) -> (StatusCode, Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (status, serde_json::from_slice(&bytes).expect("json body"))
    }

    #[tokio::test]
    async fn errors_render_as_json_with_optional_violations() {
        let (status, body) = render(ApiError::admin_required()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"error": "Admin access required"}));

        let violation = IdentityViolation::new(IdentityField::DiscordId, "bad");
        let (status, body) = render(ApiError::invalid_identities(vec![violation.clone()])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({"error": "Invalid identities",
                   "violations": [{"field": "discord_id", "message": "bad"}]})
        );

        let (status, body) = render(ApiError::identity_conflict(vec![violation])).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["violations"][0]["field"], "discord_id");
    }

    #[tokio::test]
    async fn a_duplicate_login_email_from_storage_is_a_conflict() {
        let tracker = SqliteTracker::in_memory().expect("in-memory tracker");
        tracker
            .create_user("taken@example.com", "unused-hash", "Taken", "viewer")
            .expect("create user");
        let error = tracker
            .create_user("taken@example.com", "unused-hash", "Again", "viewer")
            .expect_err("unique email");

        let (status, body) = render(ApiError::user_write(error)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            json!({"error": "A user with this email already exists"})
        );

        let (status, body) = render(ApiError::user_write(claudear_core::error::Error::Database(
            "disk I/O error".to_string(),
        )))
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"error": "Internal server error"}));
    }
}
