//! `PUT /api/directory`: bulk create-or-update of console users and their identities.
//!
//! Entries are matched to users by login email (case-insensitive). An unknown email
//! creates an invited user with an invite link; a known one gets a sparse update. Every
//! entry is validated and conflict-checked before anything is written, and the whole
//! batch is stored in one transaction, so a request either applies fully or not at all.

use super::auth::{check_api_rate_limit, is_valid_login_email, AdminCaller, Role};
use super::identities::{
    conflicts_by_plan, plan_update, IdentityPlan, UpdateIdentitiesRequest, UserIdentities,
};
use super::invites::{unusable_password_hash, NewInvite, UserInvite};
use super::routes::ApiState;
use crate::registry;
use axum::{
    extract::{rejection::JsonRejection, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use claudear_config::users::UserRegistry;
use claudear_core::identity::{ConsoleIdentity, IdentityField, IdentityViolation, UserStatus};
use claudear_storage::{DirectoryUser, DirectoryWrite, UserRow};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryRequest {
    pub users: Vec<DirectoryEntry>,
}

/// One person. `name` and `role` are required for an unknown email and are optional
/// sparse updates for a known one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryEntry {
    pub email: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub identities: Option<UpdateIdentitiesRequest>,
}

#[derive(Serialize)]
pub(crate) struct DirectoryResponse {
    pub users: Vec<DirectoryResult>,
}

#[derive(Serialize)]
pub(crate) struct DirectoryResult {
    pub user_id: i64,
    pub email: String,
    pub created: bool,
    pub status: UserStatus,
    pub identities: UserIdentities,
    pub invite: Option<UserInvite>,
}

/// The part of an entry a violation is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryField {
    Email,
    Name,
    Role,
    Identity(IdentityField),
}

impl DirectoryField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Name => "name",
            Self::Role => "role",
            Self::Identity(field) => field.as_str(),
        }
    }
}

impl Serialize for DirectoryField {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A problem with one entry, named by the entry's email. Never carries a stored value.
#[derive(Debug, Clone, Serialize)]
pub struct DirectoryViolation {
    pub email: String,
    pub field: DirectoryField,
    pub message: String,
}

impl DirectoryViolation {
    fn new(email: &str, field: DirectoryField, message: impl Into<String>) -> Self {
        Self {
            email: email.to_string(),
            field,
            message: message.into(),
        }
    }

    fn identity(email: &str, violation: IdentityViolation) -> Self {
        Self::new(
            email,
            DirectoryField::Identity(violation.field),
            violation.message,
        )
    }
}

#[derive(Debug)]
pub(crate) enum DirectoryError {
    RateLimited,
    InvalidBody,
    Internal,
    Invalid(Vec<DirectoryViolation>),
    Conflict(Vec<DirectoryViolation>),
}

impl DirectoryError {
    fn storage(error: claudear_core::error::Error) -> Self {
        tracing::error!(error = %error, "Directory storage operation failed");
        Self::Internal
    }
}

impl From<StatusCode> for DirectoryError {
    fn from(_: StatusCode) -> Self {
        Self::Internal
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    violations: Vec<DirectoryViolation>,
}

impl IntoResponse for DirectoryError {
    fn into_response(self) -> Response {
        let (status, error, violations) = match self {
            Self::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many requests",
                Vec::new(),
            ),
            Self::InvalidBody => (StatusCode::BAD_REQUEST, "Invalid request body", Vec::new()),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error",
                Vec::new(),
            ),
            Self::Invalid(violations) => (
                StatusCode::BAD_REQUEST,
                "Invalid directory entries",
                violations,
            ),
            Self::Conflict(violations) => (
                StatusCode::CONFLICT,
                "Identities already belong to another person",
                violations,
            ),
        };
        (status, Json(ErrorBody { error, violations })).into_response()
    }
}

/// A validated entry, ready to write.
struct StagedEntry {
    email: String,
    user: StagedUser,
    plan: Option<IdentityPlan>,
}

enum StagedUser {
    Existing {
        user_id: i64,
        name: Option<String>,
        role: Option<Role>,
    },
    New {
        name: String,
        role: Role,
    },
}

/// PUT /api/directory
pub(crate) async fn put_directory_handler(
    AdminCaller(admin): AdminCaller,
    State(state): State<ApiState>,
    body: Result<Json<DirectoryRequest>, JsonRejection>,
) -> Result<Json<DirectoryResponse>, DirectoryError> {
    if !check_api_rate_limit(admin.id) {
        return Err(DirectoryError::RateLimited);
    }
    let Json(request) = body.map_err(|_| DirectoryError::InvalidBody)?;

    registry::reload(&state.registry, state.tracker.as_ref());
    let users: HashMap<String, UserRow> = state
        .tracker
        .list_users()
        .map_err(DirectoryError::storage)?
        .into_iter()
        .map(|user| (user.email.to_lowercase(), user))
        .collect();
    let identities = identities_by_user(&state)?;

    let staged = stage(&state.registry, &users, &identities, request.users)?;
    check_conflicts(&state.registry, &staged)?;

    let unusable_hash = if staged
        .iter()
        .any(|entry| matches!(entry.user, StagedUser::New { .. }))
    {
        Some(unusable_password_hash().await?)
    } else {
        None
    };
    let invites: Vec<Option<NewInvite>> = staged
        .iter()
        .map(|entry| match entry.user {
            StagedUser::New { .. } => Some(NewInvite::generate(admin.id)),
            StagedUser::Existing { .. } => None,
        })
        .collect();
    let writes: Vec<DirectoryWrite> = staged
        .iter()
        .zip(&invites)
        .map(|(entry, invite)| DirectoryWrite {
            user: match &entry.user {
                StagedUser::Existing {
                    user_id,
                    name,
                    role,
                } => DirectoryUser::Existing {
                    user_id: *user_id,
                    name: name.clone(),
                    role: role.map(|role| role.as_str().to_string()),
                },
                StagedUser::New { name, role } => DirectoryUser::New {
                    email: entry.email.clone(),
                    name: name.clone(),
                    role: role.as_str().to_string(),
                    password_hash: unusable_hash.clone().unwrap_or_default(),
                    status: UserStatus::Invited,
                },
            },
            identities: entry.plan.as_ref().map(|plan| plan.write.clone()),
            invite: invite.as_ref().map(|invite| invite.write.clone()),
        })
        .collect();

    let user_ids = state
        .tracker
        .save_directory(&writes)
        .map_err(DirectoryError::storage)?;
    registry::reload(&state.registry, state.tracker.as_ref());

    let identities = identities_by_user(&state)?;
    let mut results = Vec::with_capacity(user_ids.len());
    for ((entry, invite), user_id) in staged.iter().zip(&invites).zip(user_ids) {
        let user = state
            .tracker
            .get_user_by_id(user_id)
            .map_err(DirectoryError::storage)?
            .ok_or(DirectoryError::Internal)?;
        results.push(DirectoryResult {
            user_id,
            email: user.email.clone(),
            created: matches!(entry.user, StagedUser::New { .. }),
            status: user.status,
            identities: UserIdentities::effective(&state.registry, &user, identities.get(&user_id)),
            invite: invite.as_ref().map(|invite| invite.response(&state.config)),
        });
    }
    tracing::info!(
        entries = results.len(),
        created = results.iter().filter(|result| result.created).count(),
        "Directory loaded"
    );
    Ok(Json(DirectoryResponse { users: results }))
}

fn identities_by_user(state: &ApiState) -> Result<HashMap<i64, ConsoleIdentity>, DirectoryError> {
    Ok(state
        .tracker
        .list_user_identities()
        .map_err(DirectoryError::storage)?
        .into_iter()
        .map(|entry| (entry.user_id, entry))
        .collect())
}

/// Validate every entry, collecting the violations of all of them.
fn stage(
    registry: &UserRegistry,
    users: &HashMap<String, UserRow>,
    identities: &HashMap<i64, ConsoleIdentity>,
    entries: Vec<DirectoryEntry>,
) -> Result<Vec<StagedEntry>, DirectoryError> {
    let mut staged = Vec::with_capacity(entries.len());
    let mut violations = Vec::new();
    let mut seen = HashSet::new();

    for entry in entries {
        let email = entry.email.trim().to_string();
        let mut problems = Vec::new();
        if !is_valid_login_email(&email) {
            problems.push(DirectoryViolation::new(
                &email,
                DirectoryField::Email,
                "email must be a valid email address",
            ));
        } else if !seen.insert(email.to_lowercase()) {
            problems.push(DirectoryViolation::new(
                &email,
                DirectoryField::Email,
                "email appears more than once in this request",
            ));
        }

        let existing = users.get(&email.to_lowercase());
        let name = entry.name.as_deref().map(str::trim);
        if name == Some("") || (existing.is_none() && name.is_none()) {
            problems.push(DirectoryViolation::new(
                &email,
                DirectoryField::Name,
                "name is required for a new user and must not be empty",
            ));
        }
        let role = match entry.role.as_deref().map(Role::from_str) {
            Some(Ok(role)) => Some(role),
            Some(Err(_)) => {
                problems.push(DirectoryViolation::new(
                    &email,
                    DirectoryField::Role,
                    "role must be admin or viewer",
                ));
                None
            }
            None => {
                if existing.is_none() {
                    problems.push(DirectoryViolation::new(
                        &email,
                        DirectoryField::Role,
                        "role is required for a new user",
                    ));
                }
                None
            }
        };

        let login_email = existing.map_or(email.as_str(), |user| user.email.as_str());
        let plan = match entry.identities {
            Some(update) => match plan_update(
                registry,
                existing.and_then(|user| identities.get(&user.id)),
                login_email,
                existing.map(|user| user.id),
                update,
                true,
            ) {
                Ok(plan) => Some(plan),
                Err(found) => {
                    problems.extend(
                        found
                            .into_iter()
                            .map(|violation| DirectoryViolation::identity(&email, violation)),
                    );
                    None
                }
            },
            None => None,
        };

        let user = match (existing, name, role) {
            (Some(user), name, role) => Some(StagedUser::Existing {
                user_id: user.id,
                name: name.map(str::to_string),
                role,
            }),
            (None, Some(name), Some(role)) => Some(StagedUser::New {
                name: name.to_string(),
                role,
            }),
            (None, _, _) => None,
        };
        match user {
            Some(user) if problems.is_empty() => staged.push(StagedEntry { email, user, plan }),
            _ => violations.append(&mut problems),
        }
    }

    if violations.is_empty() {
        Ok(staged)
    } else {
        Err(DirectoryError::Invalid(violations))
    }
}

fn check_conflicts(registry: &UserRegistry, staged: &[StagedEntry]) -> Result<(), DirectoryError> {
    let (emails, plans): (Vec<&str>, Vec<IdentityPlan>) = staged
        .iter()
        .filter_map(|entry| entry.plan.clone().map(|plan| (entry.email.as_str(), plan)))
        .unzip();
    let violations: Vec<DirectoryViolation> = conflicts_by_plan(registry, &plans)
        .into_iter()
        .zip(emails)
        .flat_map(|(conflicts, email)| {
            conflicts
                .into_iter()
                .map(move |violation| DirectoryViolation::identity(email, violation))
        })
        .collect();
    if violations.is_empty() {
        Ok(())
    } else {
        Err(DirectoryError::Conflict(violations))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tokens::hash_secret;
    use crate::api::{apply_dashboard_middleware, create_api_router_with_dashboard};
    use axum::body::Body;
    use axum::http::Request;
    use axum::Router;
    use claudear_config::config::{Config, UserConfig};
    use claudear_storage::{FixAttemptTracker, SqliteTracker};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;
    use tower_cookies::CookieManagerLayer;

    /// Users created first so the acting users' ids stay clear of ids other suites
    /// write as, because the API rate limiter is process-wide.
    const FILLER_USERS: usize = 1300;
    const AMY_DISCORD_ID: &str = "452316113016193024";

    struct Caller {
        id: i64,
        session: String,
        bearer: String,
    }

    struct TestApp {
        router: Router,
        tracker: Arc<dyn FixAttemptTracker>,
        registry: UserRegistry,
        admin: Caller,
        viewer: Caller,
    }

    fn caller(tracker: &Arc<dyn FixAttemptTracker>, email: &str, role: Role) -> Caller {
        let id = tracker
            .create_user(email, "unused-hash", email, role.as_str())
            .expect("create user");
        let session = tracker
            .create_session(id, "2099-12-31 23:59:59")
            .expect("create session");
        let bearer = format!("cldr_{}", hex::encode(rand::random::<[u8; 24]>()));
        tracker
            .create_api_token(id, "directory", &hash_secret(&bearer), &bearer[..13], None)
            .expect("create token");
        Caller {
            id,
            session,
            bearer,
        }
    }

    fn test_app(with_middleware: bool) -> TestApp {
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
        let admin = caller(&tracker, "directory-admin@example.com", Role::Admin);
        let viewer = caller(&tracker, "directory-viewer@example.com", Role::Viewer);
        let config = Config {
            users: HashMap::from([(
                "carol".to_string(),
                UserConfig {
                    github_usernames: vec!["carol-gh".to_string()],
                    email: Some("carol@example.com".to_string()),
                    ..UserConfig::default()
                },
            )]),
            storage_dir: std::env::temp_dir().join("claudear-directory-tests"),
            ..Config::default()
        };
        let registry = UserRegistry::new(config.users.clone());
        let router = create_api_router_with_dashboard(
            config,
            tracker.clone(),
            PathBuf::from("claudear.toml"),
            tracker.subscribe_indexing_progress(),
            None,
            registry.clone(),
        );
        let router = if with_middleware {
            apply_dashboard_middleware(router, false)
        } else {
            router.layer(CookieManagerLayer::new())
        };
        TestApp {
            router,
            tracker,
            registry,
            admin,
            viewer,
        }
    }

    async fn put(
        app: &TestApp,
        authorization: Option<&str>,
        session: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method("PUT")
            .uri("/api/directory")
            .header("content-type", "application/json");
        if let Some(authorization) = authorization {
            builder = builder.header("authorization", authorization);
        }
        if let Some(session) = session {
            builder = builder.header("cookie", format!("claudear_session={session}"));
        }
        let request = builder.body(Body::from(body.to_string())).expect("request");
        let response = app.router.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn bearer(caller: &Caller) -> String {
        format!("Bearer {}", caller.bearer)
    }

    fn user_count(app: &TestApp) -> usize {
        app.tracker.list_users().expect("list users").len()
    }

    #[test]
    fn directory_fields_serialize_flat() {
        let violation = DirectoryViolation::new(
            "amy@example.com",
            DirectoryField::Identity(IdentityField::DiscordId),
            "bad",
        );
        assert_eq!(
            serde_json::to_value(&violation).expect("serialize"),
            json!({"email": "amy@example.com", "field": "discord_id", "message": "bad"})
        );
        assert_eq!(DirectoryField::Email.as_str(), "email");
        assert_eq!(DirectoryField::Name.as_str(), "name");
        assert_eq!(DirectoryField::Role.as_str(), "role");
    }

    #[tokio::test]
    async fn admin_caller_authenticates_by_bearer_or_session() {
        let app = test_app(false);
        let empty = json!({"users": []});

        let (status, body) = put(&app, Some(&bearer(&app.admin)), None, empty.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"users": []}));

        let (status, _) = put(&app, Some(&bearer(&app.viewer)), None, empty.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, _) = put(&app, Some("Bearer cldr_unknown"), None, empty.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = put(
            &app,
            Some("Bearer cldr_unknown"),
            Some(&app.admin.session),
            empty.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "cookie must be ignored");

        let (status, _) = put(
            &app,
            Some("Basic YWRtaW46YWRtaW4="),
            Some(&app.admin.session),
            empty.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = put(&app, None, Some(&app.admin.session), empty.clone()).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = put(&app, None, Some(&app.viewer.session), empty.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, _) = put(&app, None, None, empty).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bearer_needs_no_csrf_token_but_a_session_does() {
        let app = test_app(true);
        let empty = json!({"users": []});

        let (status, _) = put(&app, Some(&bearer(&app.admin)), None, empty.clone()).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = put(&app, None, Some(&app.admin.session), empty).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn creates_invited_users_and_sparsely_updates_existing_ones() {
        let app = test_app(false);
        let before = user_count(&app);

        let (status, body) = put(
            &app,
            Some(&bearer(&app.admin)),
            None,
            json!({"users": [
                {"email": "directory-amy@example.com", "name": "Amy", "role": "viewer",
                 "identities": {"github_usernames": ["amy-gh"], "discord_id": AMY_DISCORD_ID}},
                {"email": "Directory-Viewer@Example.com",
                 "identities": {"linear_names": ["Viewer Person"]}}
            ]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(user_count(&app), before + 1);

        let created = &body["users"][0];
        assert_eq!(created["created"], true);
        assert_eq!(created["status"], "invited");
        assert_eq!(created["email"], "directory-amy@example.com");
        assert!(created["invite"]["invite_link"]
            .as_str()
            .expect("invite link")
            .starts_with("/invite#cldi_"));
        assert_eq!(created["identities"]["origin"], "console");
        assert_eq!(
            created["identities"]["identities"]["github_usernames"],
            json!(["amy-gh"])
        );
        let amy_id = created["user_id"].as_i64().expect("user id");
        let amy = app
            .tracker
            .get_user_by_id(amy_id)
            .expect("lookup")
            .expect("amy exists");
        assert_eq!(amy.status, UserStatus::Invited);
        assert!(amy.password_hash.starts_with("$2b$12$"));
        assert!(app
            .tracker
            .get_open_invite_for_user(amy_id)
            .expect("lookup")
            .is_some());
        assert_eq!(
            app.registry.discord_id_for_github("AMY-GH").as_deref(),
            Some(AMY_DISCORD_ID)
        );
        assert!(app
            .registry
            .users()
            .contains_key(&format!("console-{amy_id}")));

        let updated = &body["users"][1];
        assert_eq!(updated["created"], false);
        assert_eq!(updated["status"], "active");
        assert_eq!(updated["user_id"], app.viewer.id);
        assert_eq!(updated["invite"], Value::Null);
        assert_eq!(
            updated["identities"]["identities"]["linear_names"],
            json!(["Viewer Person"])
        );
        let viewer = app
            .tracker
            .get_user_by_id(app.viewer.id)
            .expect("lookup")
            .expect("viewer exists");
        assert_eq!(viewer.name, "directory-viewer@example.com");
        assert_eq!(viewer.role, "viewer");

        let (status, body) = put(
            &app,
            Some(&bearer(&app.admin)),
            None,
            json!({"users": [
                {"email": "directory-viewer@example.com", "name": "Vera", "role": "admin",
                 "identities": {"slack_id": "U0123456789"}}
            ]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let identities = &body["users"][0]["identities"]["identities"];
        assert_eq!(identities["linear_names"], json!(["Viewer Person"]));
        assert_eq!(identities["slack_id"], "U0123456789");
        let viewer = app
            .tracker
            .get_user_by_id(app.viewer.id)
            .expect("lookup")
            .expect("viewer exists");
        assert_eq!(viewer.name, "Vera");
        assert_eq!(viewer.role, "admin");
    }

    #[tokio::test]
    async fn one_invalid_entry_rejects_the_whole_batch() {
        let app = test_app(false);
        let before = user_count(&app);

        let (status, body) = put(
            &app,
            Some(&bearer(&app.admin)),
            None,
            json!({"users": [
                {"email": "directory-ok@example.com", "name": "Ok", "role": "viewer",
                 "identities": {"github_usernames": ["ok-gh"]}},
                {"email": "directory-bad@example.com", "name": "Bad", "role": "owner",
                 "identities": {"discord_id": "123"}},
                {"email": "directory-nameless@example.com"},
                {"email": "not-an-email", "name": "X", "role": "viewer"},
                {"email": "DIRECTORY-OK@example.com", "name": "Twice", "role": "viewer"}
            ]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Invalid directory entries");
        let violations: Vec<(String, String)> = body["violations"]
            .as_array()
            .expect("violations")
            .iter()
            .map(|violation| {
                (
                    violation["email"].as_str().unwrap_or_default().to_string(),
                    violation["field"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let expected = [
            ("directory-bad@example.com", "role"),
            ("directory-bad@example.com", "discord_id"),
            ("directory-nameless@example.com", "name"),
            ("directory-nameless@example.com", "role"),
            ("not-an-email", "email"),
            ("DIRECTORY-OK@example.com", "email"),
        ];
        assert_eq!(
            violations,
            expected
                .iter()
                .map(|(email, field)| (email.to_string(), field.to_string()))
                .collect::<Vec<_>>()
        );
        assert!(!body.to_string().contains("123"));
        assert_eq!(user_count(&app), before);
        assert!(app.registry.resolve("github", "ok-gh").is_none());
    }

    #[tokio::test]
    async fn handle_conflicts_reject_the_batch_with_409() {
        let app = test_app(false);
        let before = user_count(&app);

        let (status, body) = put(
            &app,
            Some(&bearer(&app.admin)),
            None,
            json!({"users": [
                {"email": "directory-one@example.com", "name": "One", "role": "viewer",
                 "identities": {"github_usernames": ["shared-gh"]}},
                {"email": "directory-two@example.com", "name": "Two", "role": "viewer",
                 "identities": {"github_usernames": ["Shared-GH", "carol-gh"]}}
            ]}),
        )
        .await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["violations"][0]["email"], "directory-two@example.com");
        assert_eq!(body["violations"][0]["field"], "github_usernames");
        assert_eq!(body["violations"].as_array().map(Vec::len), Some(2));
        assert_eq!(user_count(&app), before);
    }

    #[tokio::test]
    async fn unknown_entry_fields_are_rejected() {
        let app = test_app(false);
        let (status, body) = put(
            &app,
            Some(&bearer(&app.admin)),
            None,
            json!({"users": [{"email": "directory-x@example.com", "name": "X",
                              "role": "viewer", "password": "hunter2hunter2"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Invalid request body");
    }
}
