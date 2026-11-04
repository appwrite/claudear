//! Per-person identity handles owned by the console.
//!
//! Admins read and write anyone's identities and set person keys (`slug`). Every
//! other logged-in user reads and writes only their own and sees everyone else with
//! the sensitive notification targets redacted. A handle that already belongs to a
//! different person is rejected, and every write reloads the shared registry.

use super::auth::AuthUser;
use super::error::ApiError;
use super::routes::ApiState;
use crate::registry;
use axum::{
    extract::{rejection::JsonRejection, Path, State},
    Json,
};
use claudear_config::config::UserConfig;
use claudear_config::users::UserRegistry;
use claudear_core::identity::{
    validate_slug, ConsoleIdentity, Identities, IdentityField, IdentityOrigin, IdentityViolation,
    UserStatus,
};
use claudear_storage::{DirectoryUser, DirectoryWrite, IdentityWrite, UserRow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A sparse identity update: an absent field is unchanged, `""` or `[]` clears it.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateIdentitiesRequest {
    pub slug: Option<String>,
    pub linear_names: Option<Vec<String>>,
    pub github_usernames: Option<Vec<String>>,
    pub sentry_usernames: Option<Vec<String>>,
    pub jira_usernames: Option<Vec<String>>,
    pub gitlab_usernames: Option<Vec<String>>,
    pub discord_id: Option<String>,
    pub slack_id: Option<String>,
    pub email: Option<String>,
    pub push_user_key: Option<String>,
    pub sms_number: Option<String>,
    pub whatsapp_number: Option<String>,
    pub telegram_chat_id: Option<String>,
}

impl UpdateIdentitiesRequest {
    fn sets(&self, field: IdentityField) -> bool {
        match field {
            IdentityField::Slug => self.slug.is_some(),
            IdentityField::LinearNames => self.linear_names.is_some(),
            IdentityField::GithubUsernames => self.github_usernames.is_some(),
            IdentityField::SentryUsernames => self.sentry_usernames.is_some(),
            IdentityField::JiraUsernames => self.jira_usernames.is_some(),
            IdentityField::GitlabUsernames => self.gitlab_usernames.is_some(),
            IdentityField::DiscordId => self.discord_id.is_some(),
            IdentityField::SlackId => self.slack_id.is_some(),
            IdentityField::Email => self.email.is_some(),
            IdentityField::PushUserKey => self.push_user_key.is_some(),
            IdentityField::SmsNumber => self.sms_number.is_some(),
            IdentityField::WhatsappNumber => self.whatsapp_number.is_some(),
            IdentityField::TelegramChatId => self.telegram_chat_id.is_some(),
        }
    }

    fn merge_into(self, identities: &mut Identities) {
        replace(&mut identities.linear_names, self.linear_names);
        replace(&mut identities.github_usernames, self.github_usernames);
        replace(&mut identities.sentry_usernames, self.sentry_usernames);
        replace(&mut identities.jira_usernames, self.jira_usernames);
        replace(&mut identities.gitlab_usernames, self.gitlab_usernames);
        replace(&mut identities.discord_id, self.discord_id.map(Some));
        replace(&mut identities.slack_id, self.slack_id.map(Some));
        replace(&mut identities.email, self.email.map(Some));
        replace(&mut identities.push_user_key, self.push_user_key.map(Some));
        replace(&mut identities.sms_number, self.sms_number.map(Some));
        replace(
            &mut identities.whatsapp_number,
            self.whatsapp_number.map(Some),
        );
        replace(
            &mut identities.telegram_chat_id,
            self.telegram_chat_id.map(Some),
        );
    }
}

fn replace<T>(target: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *target = value;
    }
}

/// A person's effective identities, as the API returns them.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UserIdentities {
    pub user_id: i64,
    pub name: String,
    /// The console login email; `None` when redacted.
    pub login_email: Option<String>,
    /// The key the registry knows this person by, possibly `console-<id>`; `None` when
    /// the origin is unset.
    pub person_key: Option<String>,
    /// The slug stored on the console row, if any; never a `console-<id>` key.
    pub slug: Option<String>,
    pub origin: IdentityOrigin,
    pub status: UserStatus,
    pub redacted: bool,
    pub identities: Identities,
    pub updated_at: Option<String>,
}

impl UserIdentities {
    /// The identities in effect for `user`: their console row when they have one, else
    /// the config entry matching their login email, else nothing.
    pub(crate) fn effective(
        registry: &UserRegistry,
        user: &UserRow,
        entry: Option<&ConsoleIdentity>,
    ) -> Self {
        let (origin, person_key, slug, identities, updated_at) = match entry {
            Some(entry) => (
                IdentityOrigin::Console,
                Some(registry.person_key(entry)),
                entry.slug.clone(),
                entry.identities.clone(),
                Some(entry.updated_at.clone()),
            ),
            None => match configured_person(registry, &user.email) {
                Some((key, config)) => (
                    IdentityOrigin::Config,
                    Some(key),
                    None,
                    Identities::from(&config),
                    None,
                ),
                None => (
                    IdentityOrigin::Unset,
                    None,
                    None,
                    Identities::default(),
                    None,
                ),
            },
        };
        Self {
            user_id: user.id,
            name: user.name.clone(),
            login_email: Some(user.email.clone()),
            person_key,
            slug,
            origin,
            status: user.status,
            redacted: false,
            identities,
            updated_at,
        }
    }

    /// The view for a non-admin looking at someone else: sensitive fields and the login
    /// email removed.
    pub(crate) fn redacted(self) -> Self {
        Self {
            login_email: None,
            redacted: true,
            identities: self.identities.redacted(),
            ..self
        }
    }
}

/// A validated identity write for one person, ready for conflict checks and storage.
#[derive(Debug, Clone)]
pub(crate) struct IdentityPlan {
    /// `None` for a user that does not exist yet.
    pub user_id: Option<i64>,
    /// The person key after the write; `None` when it will be a `console-<id>` key that
    /// does not exist yet.
    pub person_key: Option<String>,
    /// The key the user holds as a console person before the write.
    pub current_key: Option<String>,
    pub write: IdentityWrite,
}

/// Merge `update` onto the person's current identities and validate what it changed.
///
/// The base is the user's console row (`existing`), else the config entry matching their
/// login email, in which case the slug defaults to that config key so the link becomes
/// explicit. Values the update leaves alone are not re-validated, because config values
/// were never validated. Only admins may send `slug`; callers enforce that.
pub(crate) fn plan_update(
    registry: &UserRegistry,
    existing: Option<&ConsoleIdentity>,
    login_email: &str,
    user_id: Option<i64>,
    update: UpdateIdentitiesRequest,
) -> Result<IdentityPlan, Vec<IdentityViolation>> {
    let user_id = user_id.or(existing.map(|entry| entry.user_id));
    let touched: Vec<IdentityField> = IdentityField::ALL
        .into_iter()
        .filter(|field| update.sets(*field))
        .collect();
    let (base_slug, mut identities) = base(registry, existing, login_email);

    let mut violations = Vec::new();
    let slug = match update.slug.as_deref().map(str::trim) {
        None => base_slug,
        Some("") => None,
        Some(slug) => {
            if let Err(violation) = validate_slug(slug) {
                violations.push(violation);
            }
            Some(slug.to_string())
        }
    };

    update.merge_into(&mut identities);
    let identities = identities.normalized();
    if let Err(found) = identities.validate() {
        violations.extend(
            found
                .into_iter()
                .filter(|violation| touched.contains(&violation.field)),
        );
    }
    if !violations.is_empty() {
        return Err(violations);
    }

    let current_key = existing.map(|entry| registry.person_key(entry));
    let person_key = match user_id {
        Some(user_id) => Some(registry.person_key(&ConsoleIdentity {
            user_id,
            login_email: login_email.to_string(),
            name: String::new(),
            slug: slug.clone(),
            identities: identities.clone(),
            updated_at: String::new(),
        })),
        None => slug
            .clone()
            .or_else(|| configured_person(registry, login_email).map(|(key, _)| key)),
    };
    Ok(IdentityPlan {
        user_id,
        person_key,
        current_key,
        write: IdentityWrite { slug, identities },
    })
}

/// Handles and person keys that another person already holds, in the registry or
/// earlier in `plans`. Notification targets may be shared and are never conflicts.
pub(crate) fn find_conflicts(
    registry: &UserRegistry,
    plans: &[IdentityPlan],
) -> Vec<IdentityViolation> {
    conflicts_by_plan(registry, plans)
        .into_iter()
        .flatten()
        .collect()
}

/// [`find_conflicts`] grouped by plan: element `i` holds the conflicts of `plans[i]`.
pub(crate) fn conflicts_by_plan(
    registry: &UserRegistry,
    plans: &[IdentityPlan],
) -> Vec<Vec<IdentityViolation>> {
    let mut conflicts = Vec::with_capacity(plans.len());
    let mut claimed: HashMap<(IdentityField, String), Claimant> = HashMap::new();
    let mut claim = |field: IdentityField, value: &str, claimant: Claimant| -> bool {
        let value = if field.is_case_insensitive() {
            value.to_lowercase()
        } else {
            value.to_string()
        };
        *claimed.entry((field, value)).or_insert(claimant) == claimant
    };

    for (index, plan) in plans.iter().enumerate() {
        let mut violations = Vec::new();
        let claimant = plan.user_id.map_or(Claimant::Entry(index), Claimant::User);
        if let Some(slug) = &plan.write.slug {
            let owner = registry.owner(IdentityField::Slug, slug);
            if owner.is_some() && owner != plan.current_key {
                violations.push(conflict(IdentityField::Slug, slug));
            }
        }
        if let Some(key) = &plan.person_key {
            if !claim(IdentityField::Slug, key, claimant) {
                violations.push(conflict(IdentityField::Slug, key));
            }
        }
        for field in CONFLICT_FIELDS {
            for value in handle_values(&plan.write.identities, field) {
                let owned_elsewhere = registry.owner(field, value).is_some_and(|owner| {
                    Some(&owner) != plan.person_key.as_ref()
                        && Some(&owner) != plan.current_key.as_ref()
                });
                if owned_elsewhere || !claim(field, value, claimant) {
                    violations.push(conflict(field, value));
                }
            }
        }
        conflicts.push(violations);
    }
    conflicts
}

/// Who claims a value within one batch: an existing user, or a not-yet-created entry.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Claimant {
    User(i64),
    Entry(usize),
}

const CONFLICT_FIELDS: [IdentityField; 7] = [
    IdentityField::LinearNames,
    IdentityField::GithubUsernames,
    IdentityField::SentryUsernames,
    IdentityField::JiraUsernames,
    IdentityField::GitlabUsernames,
    IdentityField::DiscordId,
    IdentityField::SlackId,
];

fn handle_values(identities: &Identities, field: IdentityField) -> Vec<&str> {
    fn list(values: &[String]) -> Vec<&str> {
        values.iter().map(String::as_str).collect()
    }
    match field {
        IdentityField::LinearNames => list(&identities.linear_names),
        IdentityField::GithubUsernames => list(&identities.github_usernames),
        IdentityField::SentryUsernames => list(&identities.sentry_usernames),
        IdentityField::JiraUsernames => list(&identities.jira_usernames),
        IdentityField::GitlabUsernames => list(&identities.gitlab_usernames),
        IdentityField::DiscordId => identities.discord_id.as_deref().into_iter().collect(),
        IdentityField::SlackId => identities.slack_id.as_deref().into_iter().collect(),
        IdentityField::Slug
        | IdentityField::Email
        | IdentityField::PushUserKey
        | IdentityField::SmsNumber
        | IdentityField::WhatsappNumber
        | IdentityField::TelegramChatId => Vec::new(),
    }
}

fn conflict(field: IdentityField, value: &str) -> IdentityViolation {
    IdentityViolation::new(
        field,
        format!("'{value}' already belongs to another person"),
    )
}

fn base(
    registry: &UserRegistry,
    existing: Option<&ConsoleIdentity>,
    login_email: &str,
) -> (Option<String>, Identities) {
    if let Some(entry) = existing {
        return (entry.slug.clone(), entry.identities.clone());
    }
    match configured_person(registry, login_email) {
        Some((key, config)) => (Some(key), Identities::from(&config)),
        None => (None, Identities::default()),
    }
}

/// The config person a user without a console row stands for: the entry matching their
/// login email, unless a console person already holds that key.
fn configured_person(registry: &UserRegistry, login_email: &str) -> Option<(String, UserConfig)> {
    registry
        .configured_for_email(login_email)
        .filter(|(key, _)| registry.origin(key) == Some(IdentityOrigin::Config))
}

/// GET /api/identities
pub(crate) async fn list_identities_handler(
    actor: AuthUser,
    State(state): State<ApiState>,
) -> Result<Json<Vec<UserIdentities>>, ApiError> {
    let users = state.tracker.list_users().map_err(ApiError::storage)?;
    let mut entries: HashMap<i64, ConsoleIdentity> = state
        .tracker
        .list_user_identities()
        .map_err(ApiError::storage)?
        .into_iter()
        .map(|entry| (entry.user_id, entry))
        .collect();
    let identities = users
        .iter()
        .map(|user| {
            visible(
                &state.registry,
                &actor,
                user,
                entries.remove(&user.id).as_ref(),
            )
        })
        .collect();
    Ok(Json(identities))
}

/// GET /api/users/{id}/identities
pub(crate) async fn get_user_identities_handler(
    actor: AuthUser,
    State(state): State<ApiState>,
    Path(user_id): Path<i64>,
) -> Result<Json<UserIdentities>, ApiError> {
    let user = find_user(&state, user_id)?;
    let entry = console_entry(&state, user_id)?;
    Ok(Json(visible(
        &state.registry,
        &actor,
        &user,
        entry.as_ref(),
    )))
}

/// PUT /api/users/{id}/identities
pub(crate) async fn update_user_identities_handler(
    actor: AuthUser,
    State(state): State<ApiState>,
    Path(user_id): Path<i64>,
    body: Result<Json<UpdateIdentitiesRequest>, JsonRejection>,
) -> Result<Json<UserIdentities>, ApiError> {
    authorize_write(&state, &actor, user_id)?;
    let Json(update) = body?;
    if update.slug.is_some() && !actor.is_admin() {
        return Err(ApiError::forbidden(
            "Only an admin can change the person key",
        ));
    }
    let user = find_user(&state, user_id)?;

    let _writes = state.identity_writes.lock().await;
    registry::reload(&state.registry, state.tracker.as_ref());
    let existing = console_entry(&state, user_id)?;
    let plan = plan_update(
        &state.registry,
        existing.as_ref(),
        &user.email,
        Some(user_id),
        update,
    )
    .map_err(ApiError::invalid_identities)?;
    let conflicts = find_conflicts(&state.registry, std::slice::from_ref(&plan));
    if !conflicts.is_empty() {
        return Err(ApiError::identity_conflict(conflicts));
    }

    save_identities(&state, user_id, plan.write)?;
    registry::reload(&state.registry, state.tracker.as_ref());

    let entry = console_entry(&state, user_id)?;
    Ok(Json(UserIdentities::effective(
        &state.registry,
        &user,
        entry.as_ref(),
    )))
}

/// DELETE /api/users/{id}/identities — drop the console override so config applies again.
///
/// A non-admin cannot change their person key, so when dropping the row would change it
/// (an admin set a slug other than the config key their login email matches), their
/// reset clears the handles and targets but keeps the slug.
pub(crate) async fn reset_user_identities_handler(
    actor: AuthUser,
    State(state): State<ApiState>,
    Path(user_id): Path<i64>,
) -> Result<Json<UserIdentities>, ApiError> {
    authorize_write(&state, &actor, user_id)?;
    let user = find_user(&state, user_id)?;

    let _writes = state.identity_writes.lock().await;
    registry::reload(&state.registry, state.tracker.as_ref());
    let configured_key = state
        .registry
        .configured_for_email(&user.email)
        .map(|(key, _)| key);
    let kept_slug = console_entry(&state, user_id)?
        .and_then(|entry| entry.slug)
        .filter(|slug| !actor.is_admin() && Some(slug) != configured_key.as_ref());
    match kept_slug {
        Some(slug) => save_identities(
            &state,
            user_id,
            IdentityWrite {
                slug: Some(slug),
                identities: Identities::default(),
            },
        )?,
        None => {
            state
                .tracker
                .delete_user_identities(user_id)
                .map_err(ApiError::storage)?;
        }
    }
    registry::reload(&state.registry, state.tracker.as_ref());

    let entry = console_entry(&state, user_id)?;
    Ok(Json(UserIdentities::effective(
        &state.registry,
        &user,
        entry.as_ref(),
    )))
}

fn save_identities(state: &ApiState, user_id: i64, write: IdentityWrite) -> Result<(), ApiError> {
    state
        .tracker
        .save_directory(&[DirectoryWrite {
            user: DirectoryUser::Existing {
                user_id,
                name: None,
                role: None,
            },
            identities: Some(write),
            invite: None,
        }])
        .map_err(ApiError::storage)?;
    Ok(())
}

fn authorize_write(state: &ApiState, actor: &AuthUser, user_id: i64) -> Result<(), ApiError> {
    if !state.limiters.api.allow(&actor.id) {
        return Err(ApiError::rate_limited());
    }
    if actor.is_admin() || actor.id == user_id {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "You can only change your own identities",
        ))
    }
}

fn visible(
    registry: &UserRegistry,
    actor: &AuthUser,
    user: &UserRow,
    entry: Option<&ConsoleIdentity>,
) -> UserIdentities {
    let identities = UserIdentities::effective(registry, user, entry);
    if actor.is_admin() || actor.id == user.id {
        identities
    } else {
        identities.redacted()
    }
}

fn find_user(state: &ApiState, user_id: i64) -> Result<UserRow, ApiError> {
    state
        .tracker
        .get_user_by_id(user_id)
        .map_err(ApiError::storage)?
        .ok_or_else(ApiError::user_not_found)
}

fn console_entry(state: &ApiState, user_id: i64) -> Result<Option<ConsoleIdentity>, ApiError> {
    Ok(state
        .tracker
        .list_user_identities()
        .map_err(ApiError::storage)?
        .into_iter()
        .find(|entry| entry.user_id == user_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_api_router_with_dashboard;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::Response;
    use axum::Router;
    use claudear_config::config::Config;
    use claudear_storage::{FixAttemptTracker, SqliteTracker};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;
    use tower_cookies::CookieManagerLayer;

    const JAKE_DISCORD_ID: &str = "452316113016193024";
    const ALICE_DISCORD_ID: &str = "452316113016193025";
    const SLACK_ID: &str = "U0123456789";

    fn config_users() -> HashMap<String, UserConfig> {
        HashMap::from([
            (
                "jake".to_string(),
                UserConfig {
                    github_usernames: vec!["jakebarnby".to_string()],
                    discord_id: Some(JAKE_DISCORD_ID.to_string()),
                    email: Some("Jake@Example.com".to_string()),
                    sms_number: Some("+14155550123".to_string()),
                    ..UserConfig::default()
                },
            ),
            (
                "alice".to_string(),
                UserConfig {
                    github_usernames: vec!["alicesmith".to_string()],
                    discord_id: Some(ALICE_DISCORD_ID.to_string()),
                    email: Some("alice@example.com".to_string()),
                    sms_number: Some("+14155550199".to_string()),
                    ..UserConfig::default()
                },
            ),
        ])
    }

    fn console(
        user_id: i64,
        login_email: &str,
        slug: Option<&str>,
        identities: Identities,
    ) -> ConsoleIdentity {
        ConsoleIdentity {
            user_id,
            login_email: login_email.to_string(),
            name: format!("User {user_id}"),
            slug: slug.map(str::to_string),
            identities,
            updated_at: "2026-11-04 10:00:00".to_string(),
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn fields(violations: &[IdentityViolation]) -> Vec<IdentityField> {
        violations.iter().map(|violation| violation.field).collect()
    }

    #[test]
    fn absent_fields_are_unchanged_and_empty_values_clear() {
        let registry = UserRegistry::new(HashMap::new());
        let existing = console(
            5,
            "amy@example.com",
            Some("amy"),
            Identities {
                github_usernames: strings(&["amy"]),
                linear_names: strings(&["Amy"]),
                discord_id: Some(JAKE_DISCORD_ID.to_string()),
                slack_id: Some(SLACK_ID.to_string()),
                ..Identities::default()
            },
        );
        let update = UpdateIdentitiesRequest {
            linear_names: Some(Vec::new()),
            slack_id: Some(String::new()),
            telegram_chat_id: Some("-100123".to_string()),
            ..UpdateIdentitiesRequest::default()
        };

        let plan = plan_update(
            &registry,
            Some(&existing),
            "amy@example.com",
            Some(5),
            update,
        )
        .expect("valid update");

        assert_eq!(plan.write.slug.as_deref(), Some("amy"));
        assert_eq!(plan.write.identities.github_usernames, strings(&["amy"]));
        assert_eq!(
            plan.write.identities.discord_id.as_deref(),
            Some(JAKE_DISCORD_ID)
        );
        assert!(plan.write.identities.linear_names.is_empty());
        assert_eq!(plan.write.identities.slack_id, None);
        assert_eq!(
            plan.write.identities.telegram_chat_id.as_deref(),
            Some("-100123")
        );
        assert_eq!(plan.person_key.as_deref(), Some("amy"));
        assert_eq!(plan.current_key.as_deref(), Some("amy"));
    }

    #[test]
    fn admin_clears_the_slug_with_an_empty_string() {
        let registry = UserRegistry::new(HashMap::new());
        let existing = console(5, "amy@example.com", Some("amy"), Identities::default());
        let update = UpdateIdentitiesRequest {
            slug: Some(" ".to_string()),
            ..UpdateIdentitiesRequest::default()
        };

        let plan = plan_update(
            &registry,
            Some(&existing),
            "amy@example.com",
            Some(5),
            update,
        )
        .expect("valid update");

        assert_eq!(plan.write.slug, None);
        assert_eq!(plan.person_key.as_deref(), Some("console-5"));
    }

    #[test]
    fn first_save_starts_from_the_config_entry_and_links_its_key() {
        let registry = UserRegistry::new(config_users());
        let update = UpdateIdentitiesRequest {
            slack_id: Some(SLACK_ID.to_string()),
            ..UpdateIdentitiesRequest::default()
        };

        let plan = plan_update(&registry, None, "jake@EXAMPLE.com", Some(7), update)
            .expect("valid update");

        assert_eq!(plan.write.slug.as_deref(), Some("jake"));
        assert_eq!(
            plan.write.identities.github_usernames,
            strings(&["jakebarnby"])
        );
        assert_eq!(
            plan.write.identities.discord_id.as_deref(),
            Some(JAKE_DISCORD_ID)
        );
        assert_eq!(
            plan.write.identities.sms_number.as_deref(),
            Some("+14155550123")
        );
        assert_eq!(plan.write.identities.slack_id.as_deref(), Some(SLACK_ID));
        assert_eq!(plan.person_key.as_deref(), Some("jake"));
        assert_eq!(plan.current_key, None);
        assert!(find_conflicts(&registry, &[plan]).is_empty());
    }

    #[test]
    fn first_save_ignores_a_config_entry_another_console_person_holds() {
        let registry = UserRegistry::new(config_users());
        registry.replace_console(vec![console(
            1,
            "other@example.com",
            Some("jake"),
            Identities::default(),
        )]);

        let plan = plan_update(
            &registry,
            None,
            "jake@example.com",
            Some(7),
            UpdateIdentitiesRequest::default(),
        )
        .expect("valid update");

        assert_eq!(plan.write.slug, None);
        assert!(plan.write.identities.is_empty());
        assert_eq!(plan.person_key.as_deref(), Some("console-7"));
    }

    #[test]
    fn identity_email_never_links_to_a_config_entry() {
        let registry = UserRegistry::new(config_users());
        let update = UpdateIdentitiesRequest {
            email: Some("jake@example.com".to_string()),
            ..UpdateIdentitiesRequest::default()
        };

        let plan = plan_update(&registry, None, "viewer@example.com", Some(9), update)
            .expect("valid update");

        assert_eq!(plan.write.slug, None);
        assert_eq!(plan.person_key.as_deref(), Some("console-9"));
        assert!(find_conflicts(&registry, &[plan]).is_empty());
    }

    #[test]
    fn only_changed_fields_are_validated() {
        let users = HashMap::from([(
            "legacy".to_string(),
            UserConfig {
                discord_id: Some("123".to_string()),
                email: Some("legacy@example.com".to_string()),
                ..UserConfig::default()
            },
        )]);
        let registry = UserRegistry::new(users);
        let untouched = UpdateIdentitiesRequest {
            github_usernames: Some(strings(&["legacy-gh"])),
            ..UpdateIdentitiesRequest::default()
        };
        let plan = plan_update(&registry, None, "legacy@example.com", Some(3), untouched)
            .expect("inherited config values are not re-validated");
        assert_eq!(plan.write.identities.discord_id.as_deref(), Some("123"));

        let touched = UpdateIdentitiesRequest {
            discord_id: Some("123".to_string()),
            github_usernames: Some(strings(&["-bad"])),
            sms_number: Some("12345".to_string()),
            ..UpdateIdentitiesRequest::default()
        };
        let violations = plan_update(&registry, None, "legacy@example.com", Some(3), touched)
            .expect_err("invalid values");
        assert_eq!(
            fields(&violations),
            vec![
                IdentityField::GithubUsernames,
                IdentityField::DiscordId,
                IdentityField::SmsNumber
            ]
        );
        assert!(violations
            .iter()
            .all(|violation| !violation.message.contains("12345")));
    }

    #[test]
    fn slug_rules() {
        let registry = UserRegistry::new(HashMap::new());
        for slug in ["console-1", "Jake", "-jake"] {
            let update = UpdateIdentitiesRequest {
                slug: Some(slug.to_string()),
                ..UpdateIdentitiesRequest::default()
            };
            let violations = plan_update(&registry, None, "a@example.com", Some(2), update)
                .expect_err("invalid slug");
            assert_eq!(fields(&violations), vec![IdentityField::Slug], "{slug}");
        }
    }

    #[test]
    fn handles_owned_by_another_person_conflict() {
        let registry = UserRegistry::new(config_users());
        registry.replace_console(vec![console(
            1,
            "bee@example.com",
            None,
            Identities {
                github_usernames: strings(&["bee"]),
                slack_id: Some(SLACK_ID.to_string()),
                ..Identities::default()
            },
        )]);
        let update = UpdateIdentitiesRequest {
            github_usernames: Some(strings(&["AliceSmith", "BEE", "mine"])),
            slack_id: Some(SLACK_ID.to_string()),
            discord_id: Some(JAKE_DISCORD_ID.to_string()),
            sms_number: Some("+14155550199".to_string()),
            ..UpdateIdentitiesRequest::default()
        };
        let plan =
            plan_update(&registry, None, "new@example.com", Some(8), update).expect("valid update");

        let violations = find_conflicts(&registry, &[plan]);

        assert_eq!(
            fields(&violations),
            vec![
                IdentityField::GithubUsernames,
                IdentityField::GithubUsernames,
                IdentityField::DiscordId,
                IdentityField::SlackId,
            ]
        );
        assert!(violations.iter().all(|violation| violation
            .message
            .ends_with("already belongs to another person")));
    }

    #[test]
    fn a_person_keeps_their_own_handles_when_relinked() {
        let registry = UserRegistry::new(config_users());
        let existing = console(
            1,
            "bee@example.com",
            None,
            Identities {
                github_usernames: strings(&["bee"]),
                ..Identities::default()
            },
        );
        registry.replace_console(vec![existing.clone()]);
        let update = UpdateIdentitiesRequest {
            slug: Some("alice".to_string()),
            github_usernames: Some(strings(&["bee", "alicesmith"])),
            ..UpdateIdentitiesRequest::default()
        };

        let plan = plan_update(
            &registry,
            Some(&existing),
            "bee@example.com",
            Some(1),
            update,
        )
        .expect("valid update");

        assert_eq!(plan.person_key.as_deref(), Some("alice"));
        assert_eq!(plan.current_key.as_deref(), Some("console-1"));
        assert!(find_conflicts(&registry, &[plan]).is_empty());
    }

    #[test]
    fn a_slug_held_by_another_console_person_conflicts() {
        let registry = UserRegistry::new(config_users());
        registry.replace_console(vec![console(
            1,
            "jake@example.com",
            None,
            Identities::default(),
        )]);
        assert_eq!(registry.origin("jake"), Some(IdentityOrigin::Console));
        let update = UpdateIdentitiesRequest {
            slug: Some("jake".to_string()),
            ..UpdateIdentitiesRequest::default()
        };

        let plan = plan_update(&registry, None, "other@example.com", Some(2), update)
            .expect("valid update");

        assert_eq!(
            fields(&find_conflicts(&registry, &[plan])),
            vec![IdentityField::Slug]
        );

        let own = console(1, "jake@example.com", None, Identities::default());
        let update = UpdateIdentitiesRequest {
            slug: Some("jake".to_string()),
            ..UpdateIdentitiesRequest::default()
        };
        let plan = plan_update(&registry, Some(&own), "jake@example.com", Some(1), update)
            .expect("valid update");
        assert!(find_conflicts(&registry, &[plan]).is_empty());
    }

    #[test]
    fn entries_in_one_batch_conflict_with_each_other_but_share_notification_targets() {
        let registry = UserRegistry::new(HashMap::new());
        let plan = |user_id: i64, slug: &str| {
            let update = UpdateIdentitiesRequest {
                slug: Some(slug.to_string()),
                gitlab_usernames: Some(strings(&["Shared"])),
                email: Some("team@example.com".to_string()),
                ..UpdateIdentitiesRequest::default()
            };
            plan_update(
                &registry,
                None,
                &format!("{user_id}@example.com"),
                Some(user_id),
                update,
            )
            .expect("valid update")
        };

        let violations =
            find_conflicts(&registry, &[plan(1, "one"), plan(2, "two"), plan(3, "one")]);

        assert_eq!(
            fields(&violations),
            vec![
                IdentityField::GitlabUsernames,
                IdentityField::Slug,
                IdentityField::GitlabUsernames,
            ]
        );
    }

    async fn response_json(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("json body")
        };
        (status, body)
    }

    struct Session {
        id: i64,
        token: String,
    }

    struct TestApp {
        router: Router,
        tracker: Arc<dyn FixAttemptTracker>,
        registry: UserRegistry,
        admin: Session,
        viewer: Session,
    }

    fn session(tracker: &Arc<dyn FixAttemptTracker>, email: &str, role: &str) -> Session {
        let id = tracker
            .create_user(email, "unused-hash", email, role)
            .expect("create user");
        let token = tracker
            .create_session(id, "2099-12-31 23:59:59")
            .expect("create session");
        Session { id, token }
    }

    fn test_app() -> TestApp {
        let tracker: Arc<dyn FixAttemptTracker> =
            Arc::new(SqliteTracker::in_memory().expect("in-memory tracker"));
        let admin = session(&tracker, "jake@example.com", "admin");
        let viewer = session(&tracker, "viewer@example.com", "viewer");
        let config = Config {
            users: config_users(),
            storage_dir: std::env::temp_dir().join("claudear-identities-tests"),
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
        )
        .layer(CookieManagerLayer::new());
        TestApp {
            router,
            tracker,
            registry,
            admin,
            viewer,
        }
    }

    fn request(method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header("cookie", format!("claudear_session={token}"));
        }
        match body {
            Some(body) => builder
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("request")
    }

    async fn send(app: &TestApp, request: Request<Body>) -> (StatusCode, Value) {
        let response = app.router.clone().oneshot(request).await.expect("response");
        response_json(response).await
    }

    #[tokio::test]
    async fn list_identities_redacts_other_people_for_a_viewer() {
        let app = test_app();

        let (status, _) = send(&app, request("GET", "/api/identities", None, None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, body) = send(
            &app,
            request("GET", "/api/identities", Some(&app.viewer.token), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let people = body.as_array().expect("array");
        assert_eq!(people.len(), 2);
        let admin = people
            .iter()
            .find(|person| person["user_id"] == app.admin.id)
            .expect("admin listed");
        assert_eq!(admin["origin"], "config");
        assert_eq!(admin["person_key"], "jake");
        assert_eq!(admin["slug"], Value::Null);
        assert_eq!(admin["redacted"], true);
        assert_eq!(admin["login_email"], Value::Null);
        assert!(admin.get("email").is_none());
        assert_eq!(admin["identities"]["sms_number"], Value::Null);
        assert_eq!(
            admin["identities"]["github_usernames"],
            json!(["jakebarnby"])
        );
        let viewer = people
            .iter()
            .find(|person| person["user_id"] == app.viewer.id)
            .expect("viewer listed");
        assert_eq!(viewer["redacted"], false);
        assert_eq!(viewer["login_email"], "viewer@example.com");
        assert_eq!(viewer["origin"], "unset");
        assert_eq!(viewer["status"], "active");
    }

    #[tokio::test]
    async fn get_user_identities_is_full_for_admins_and_404_when_missing() {
        let app = test_app();

        let (status, body) = send(
            &app,
            request(
                "GET",
                &format!("/api/users/{}/identities", app.admin.id),
                Some(&app.admin.token),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["redacted"], false);
        assert_eq!(body["origin"], "config");
        assert_eq!(body["identities"]["sms_number"], "+14155550123");
        assert_eq!(body["updated_at"], Value::Null);

        let (status, body) = send(
            &app,
            request(
                "GET",
                "/api/users/999999/identities",
                Some(&app.admin.token),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "User not found");
    }

    #[tokio::test]
    async fn put_user_identities_enforces_access_validation_and_conflicts() {
        let app = test_app();
        let own = format!("/api/users/{}/identities", app.viewer.id);

        let (status, body) = send(
            &app,
            request(
                "PUT",
                &format!("/api/users/{}/identities", app.admin.id),
                Some(&app.viewer.token),
                Some(json!({"slack_id": SLACK_ID})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({"error": "You can only change your own identities"})
        );

        let (status, body) = send(
            &app,
            request(
                "PUT",
                &own,
                Some(&app.viewer.token),
                Some(json!({"slug": "viewer"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({"error": "Only an admin can change the person key"})
        );

        let (status, body) = send(
            &app,
            request(
                "PUT",
                &own,
                Some(&app.viewer.token),
                Some(json!({"discord_id": "123", "unknown": true})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Invalid request body");

        let (status, body) = send(
            &app,
            request(
                "PUT",
                &own,
                Some(&app.viewer.token),
                Some(json!({"discord_id": "123"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["violations"][0]["field"], "discord_id");

        let (status, body) = send(
            &app,
            request(
                "PUT",
                &own,
                Some(&app.viewer.token),
                Some(json!({"github_usernames": ["AliceSmith"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["violations"][0]["field"], "github_usernames");
        assert!(app.tracker.list_user_identities().expect("list").is_empty());

        let (status, body) = send(
            &app,
            request(
                "PUT",
                &own,
                Some(&app.viewer.token),
                Some(json!({"github_usernames": ["@Viewer-GH"], "email": "alice@example.com"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["origin"], "console");
        assert_eq!(body["person_key"], format!("console-{}", app.viewer.id));
        assert_eq!(body["slug"], Value::Null);
        assert_eq!(body["status"], "active");
        assert_eq!(body["identities"]["github_usernames"], json!(["Viewer-GH"]));
        assert_eq!(
            app.registry
                .resolve("github", "viewer-gh")
                .map(|person| person.slug),
            Some(format!("console-{}", app.viewer.id))
        );
        assert_eq!(
            app.registry
                .resolve("github", "alicesmith")
                .map(|person| person.slug),
            Some("alice".to_string())
        );
    }

    #[tokio::test]
    async fn admin_save_from_config_then_delete_reverts_to_config() {
        let app = test_app();
        let uri = format!("/api/users/{}/identities", app.admin.id);

        let (status, body) = send(
            &app,
            request(
                "PUT",
                &uri,
                Some(&app.admin.token),
                Some(json!({"slack_id": SLACK_ID})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["origin"], "console");
        assert_eq!(body["person_key"], "jake");
        assert_eq!(body["slug"], "jake", "saving from config links the key");
        assert_eq!(
            body["identities"]["github_usernames"],
            json!(["jakebarnby"])
        );
        assert_eq!(body["identities"]["slack_id"], SLACK_ID);
        assert_eq!(app.registry.origin("jake"), Some(IdentityOrigin::Console));
        assert_eq!(
            app.registry
                .get_by_slug("jake")
                .and_then(|person| person.slack_id),
            Some(SLACK_ID.to_string())
        );

        let (status, _) = send(&app, request("DELETE", &uri, Some(&app.viewer.token), None)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, body) =
            send(&app, request("DELETE", &uri, Some(&app.admin.token), None)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["origin"], "config");
        assert_eq!(body["person_key"], "jake");
        assert_eq!(body["slug"], Value::Null);
        assert_eq!(body["identities"]["slack_id"], Value::Null);
        assert_eq!(app.registry.origin("jake"), Some(IdentityOrigin::Config));
        assert!(app.tracker.list_user_identities().expect("list").is_empty());
    }

    #[tokio::test]
    async fn a_viewer_reset_keeps_the_person_key_an_admin_set() {
        let app = test_app();
        let uri = format!("/api/users/{}/identities", app.viewer.id);

        let (status, body) = send(
            &app,
            request(
                "PUT",
                &uri,
                Some(&app.admin.token),
                Some(json!({"slug": "vera", "github_usernames": ["vera-gh"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["slug"], "vera");

        let (status, body) =
            send(&app, request("DELETE", &uri, Some(&app.viewer.token), None)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["origin"], "console");
        assert_eq!(body["slug"], "vera");
        assert_eq!(body["person_key"], "vera");
        assert_eq!(body["identities"]["github_usernames"], json!([]));
        let stored = app.tracker.list_user_identities().expect("list");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].slug.as_deref(), Some("vera"));
        assert!(stored[0].identities.is_empty());
        assert_eq!(app.registry.origin("vera"), Some(IdentityOrigin::Console));
        assert!(app.registry.resolve("github", "vera-gh").is_none());

        let (status, body) =
            send(&app, request("DELETE", &uri, Some(&app.admin.token), None)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["origin"], "unset");
        assert_eq!(body["slug"], Value::Null);
        assert!(app.tracker.list_user_identities().expect("list").is_empty());
        assert_eq!(app.registry.origin("vera"), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_claims_of_one_handle_let_exactly_one_win() {
        const CLAIMANTS: usize = 8;
        const ROUNDS: usize = 10;
        let app = test_app();
        let claimants: Vec<Session> = (0..CLAIMANTS)
            .map(|index| {
                session(
                    &app.tracker,
                    &format!("claimant{index}@example.com"),
                    "viewer",
                )
            })
            .collect();

        for round in 0..ROUNDS {
            let handle = format!("contested-{round}");
            let barrier = Arc::new(tokio::sync::Barrier::new(CLAIMANTS));
            let tasks: Vec<_> = claimants
                .iter()
                .map(|claimant| {
                    let router = app.router.clone();
                    let barrier = barrier.clone();
                    let claim = request(
                        "PUT",
                        &format!("/api/users/{}/identities", claimant.id),
                        Some(&claimant.token),
                        Some(json!({"github_usernames": [handle]})),
                    );
                    tokio::spawn(async move {
                        barrier.wait().await;
                        router.oneshot(claim).await.expect("response").status()
                    })
                })
                .collect();
            let mut statuses = Vec::with_capacity(CLAIMANTS);
            for task in tasks {
                statuses.push(task.await.expect("claim task"));
            }

            let won = statuses
                .iter()
                .filter(|status| **status == StatusCode::OK)
                .count();
            let refused = statuses
                .iter()
                .filter(|status| **status == StatusCode::CONFLICT)
                .count();
            assert_eq!(
                (won, refused),
                (1, CLAIMANTS - 1),
                "round {round}: {statuses:?}"
            );
            let owners = app
                .tracker
                .list_user_identities()
                .expect("list")
                .into_iter()
                .filter(|entry| entry.identities.github_usernames.contains(&handle))
                .count();
            assert_eq!(owners, 1, "round {round}");
        }
    }

    #[tokio::test]
    async fn extractor_rejections_are_json() {
        let app = test_app();

        let (status, body) = send(&app, request("GET", "/api/identities", None, None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"error": "Not authenticated"}));

        let (status, body) = send(
            &app,
            request("GET", "/api/users", Some(&app.viewer.token), None),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"error": "Admin access required"}));
    }
}
