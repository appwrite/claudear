//! User registry for resolving issue assignees to notification targets.
//!
//! The registry merges people configured in `[users.<key>]` (the seed) with console users
//! whose identities live in the database. A console person replaces the config entry with
//! the same person key in full. Every clone shares one directory, so [`UserRegistry::replace_console`]
//! is seen everywhere at once, and [`UserRegistry::reload`] applies reloads in the order they read.

mod channel;

pub use channel::NotificationChannel;

use crate::config::UserConfig;
use claudear_core::identity::{
    ConsoleIdentity, Identities, IdentityField, IdentityOrigin, CONSOLE_KEY_PREFIX,
};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

const SOURCES: [(&str, IdentityField); 5] = [
    ("linear", IdentityField::LinearNames),
    ("github", IdentityField::GithubUsernames),
    ("sentry", IdentityField::SentryUsernames),
    ("jira", IdentityField::JiraUsernames),
    ("gitlab", IdentityField::GitlabUsernames),
];

/// Resolved user with notification channel IDs.
#[derive(Debug, Clone)]
pub struct ResolvedUser {
    /// The user's slug key from config.
    pub slug: String,
    /// Discord user ID for mentions.
    pub discord_id: Option<String>,
    /// Slack user ID for mentions.
    pub slack_id: Option<String>,
    /// Email address.
    pub email: Option<String>,
    /// Pushover user key.
    pub push_user_key: Option<String>,
    /// SMS phone number.
    pub sms_number: Option<String>,
}

/// Registry that resolves issue metadata to user notification targets.
#[derive(Clone, Default)]
pub struct UserRegistry {
    seed: Arc<Seed>,
    directory: Arc<RwLock<Arc<Directory>>>,
    reloading: Arc<Mutex<()>>,
}

impl fmt::Debug for UserRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserRegistry")
            .field("persons", &self.snapshot().persons.len())
            .finish()
    }
}

impl UserRegistry {
    /// Create a new user registry from config.
    pub fn new(users: HashMap<String, UserConfig>) -> Self {
        let seed = Seed::new(users);
        let directory = Directory::build(&seed, Vec::new());
        Self {
            seed: Arc::new(seed),
            directory: Arc::new(RwLock::new(Arc::new(directory))),
            reloading: Arc::default(),
        }
    }

    /// Load the console identities with `load` and apply them. Reloads through any clone
    /// run one at a time, so a reload that read an older snapshot can never replace the
    /// directory after one that read a newer snapshot. On error the directory is kept.
    pub fn reload<E>(
        &self,
        load: impl FnOnce() -> Result<Vec<ConsoleIdentity>, E>,
    ) -> Result<(), E> {
        let _reloading = self
            .reloading
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let entries = load()?;
        self.replace_console(entries);
        Ok(())
    }

    /// Rebuild the merged directory from the console users' identities. An empty list
    /// restores the config-only directory.
    pub fn replace_console(&self, entries: Vec<ConsoleIdentity>) {
        let directory = Arc::new(Directory::build(&self.seed, entries));
        *self
            .directory
            .write()
            .unwrap_or_else(PoisonError::into_inner) = directory;
    }

    /// Resolve an issue's assignee to a person based on the source type.
    ///
    /// The source (`linear`, `github`, `sentry`, `jira`, `gitlab`) is case-insensitive.
    /// GitHub and GitLab logins match case-insensitively; other handles match exactly.
    pub fn resolve(&self, source: &str, assignee_value: &str) -> Option<ResolvedUser> {
        let field = source_field(source)?;
        let directory = self.snapshot();
        directory
            .handle_owner(field, assignee_value)
            .map(Person::resolved)
    }

    /// Look up a person by key (for resolving global config references and issue metadata).
    /// A console person's `console-<id>` alias is accepted too.
    pub fn get_by_slug(&self, slug: &str) -> Option<UserConfig> {
        self.snapshot()
            .person(slug)
            .map(|person| person.user.clone())
    }

    /// Every person in the merged directory, keyed by person key.
    pub fn users(&self) -> HashMap<String, UserConfig> {
        self.snapshot()
            .persons
            .iter()
            .map(|person| (person.key.clone(), person.user.clone()))
            .collect()
    }

    /// Whether the person behind `slug` comes from the console or from config.
    pub fn origin(&self, slug: &str) -> Option<IdentityOrigin> {
        self.snapshot().person(slug).map(|person| person.origin)
    }

    /// The person key `entry` holds, or would hold, once it is part of the directory: its
    /// explicit slug, else the config entry matching its login email that no other console
    /// person claims, else `console-<user_id>`.
    pub fn person_key(&self, entry: &ConsoleIdentity) -> String {
        if let Some(slug) = explicit_slug(entry) {
            return slug.to_string();
        }
        let directory = self.snapshot();
        if let Some(key) = directory.stored_key(entry) {
            return key.to_string();
        }
        let mut entries: Vec<ConsoleIdentity> = directory
            .entries
            .iter()
            .filter(|existing| existing.user_id != entry.user_id)
            .cloned()
            .collect();
        entries.push(entry.clone());
        entries.sort_by_key(|existing| existing.user_id);
        let keys = assign_keys(&self.seed, &entries);
        entries
            .iter()
            .zip(keys)
            .find(|(existing, _)| existing.user_id == entry.user_id)
            .map(|(_, key)| key)
            .unwrap_or_else(|| console_key(entry.user_id))
    }

    /// The config entry whose email matches `email` case-insensitively (lowest key on ties).
    pub fn configured_for_email(&self, email: &str) -> Option<(String, UserConfig)> {
        let key = self.seed.keys_for_email(email).first()?;
        let user = self.seed.users.get(key)?;
        Some((key.clone(), user.clone()))
    }

    /// The person key that claims `value` for `field`.
    ///
    /// For [`IdentityField::Slug`] only console persons count as owners, because an admin
    /// links a console user to a config-only person by giving them that person's key.
    /// Notification targets can be shared between people and never have an owner.
    pub fn owner(&self, field: IdentityField, value: &str) -> Option<String> {
        let directory = self.snapshot();
        let person = match field {
            IdentityField::Slug => directory
                .person(value)
                .filter(|person| person.origin == IdentityOrigin::Console),
            _ => directory.handle_owner(field, value),
        };
        person.map(|person| person.key.clone())
    }

    /// The Discord id of the person who owns this GitHub login (the person
    /// [`Self::resolve`] finds), if they have one.
    pub fn discord_id_for_github(&self, login: &str) -> Option<String> {
        self.snapshot()
            .handle_owner(IdentityField::GithubUsernames, login)
            .and_then(|person| person.user.discord_id.clone())
    }

    /// Resolve a notifier reference: a person key becomes that person's address on
    /// `channel`; anything else, or a person without that address, stays literal.
    pub fn resolve_reference(&self, channel: NotificationChannel, reference: &str) -> String {
        self.snapshot().resolve_reference(channel, reference)
    }

    /// [`Self::resolve_reference`] for every reference, against one snapshot.
    pub fn resolve_references(
        &self,
        channel: NotificationChannel,
        references: &[String],
    ) -> Vec<String> {
        let directory = self.snapshot();
        references
            .iter()
            .map(|reference| directory.resolve_reference(channel, reference))
            .collect()
    }

    /// Check if the registry has any people.
    pub fn is_empty(&self) -> bool {
        self.snapshot().persons.is_empty()
    }

    fn snapshot(&self) -> Arc<Directory> {
        Arc::clone(
            &self
                .directory
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

impl From<&UserConfig> for Identities {
    fn from(user: &UserConfig) -> Self {
        Self {
            linear_names: user.linear_names.clone(),
            github_usernames: user.github_usernames.clone(),
            sentry_usernames: user.sentry_usernames.clone(),
            jira_usernames: user.jira_usernames.clone(),
            gitlab_usernames: user.gitlab_usernames.clone(),
            discord_id: user.discord_id.clone(),
            slack_id: user.slack_id.clone(),
            email: user.email.clone(),
            push_user_key: user.push_user_key.clone(),
            sms_number: user.sms_number.clone(),
            whatsapp_number: user.whatsapp_number.clone(),
            telegram_chat_id: user.telegram_chat_id.clone(),
        }
    }
}

impl From<Identities> for UserConfig {
    fn from(identities: Identities) -> Self {
        Self {
            linear_names: identities.linear_names,
            github_usernames: identities.github_usernames,
            sentry_usernames: identities.sentry_usernames,
            jira_usernames: identities.jira_usernames,
            gitlab_usernames: identities.gitlab_usernames,
            discord_id: identities.discord_id,
            slack_id: identities.slack_id,
            email: identities.email,
            push_user_key: identities.push_user_key,
            sms_number: identities.sms_number,
            whatsapp_number: identities.whatsapp_number,
            telegram_chat_id: identities.telegram_chat_id,
        }
    }
}

#[derive(Default)]
struct Seed {
    users: HashMap<String, UserConfig>,
    emails: HashMap<String, Vec<String>>,
}

impl Seed {
    /// Config keys with the reserved [`CONSOLE_KEY_PREFIX`] are dropped, so they can never
    /// shadow a console alias or be linked to a console user by email.
    fn new(mut users: HashMap<String, UserConfig>) -> Self {
        users.retain(|key, _| !key.starts_with(CONSOLE_KEY_PREFIX));
        let mut emails: HashMap<String, Vec<String>> = HashMap::new();
        for (key, user) in &users {
            if let Some(email) = user.email.as_deref().and_then(email_key) {
                emails.entry(email).or_default().push(key.clone());
            }
        }
        for keys in emails.values_mut() {
            keys.sort();
        }
        Self { users, emails }
    }

    /// Config keys whose email matches, in ascending key order.
    fn keys_for_email(&self, email: &str) -> &[String] {
        email_key(email)
            .and_then(|email| self.emails.get(&email))
            .map_or(&[], Vec::as_slice)
    }
}

struct Person {
    key: String,
    origin: IdentityOrigin,
    user: UserConfig,
}

impl Person {
    fn resolved(&self) -> ResolvedUser {
        ResolvedUser {
            slug: self.key.clone(),
            discord_id: self.user.discord_id.clone(),
            slack_id: self.user.slack_id.clone(),
            email: self.user.email.clone(),
            push_user_key: self.user.push_user_key.clone(),
            sms_number: self.user.sms_number.clone(),
        }
    }
}

#[derive(Hash, PartialEq, Eq)]
struct Handle {
    field: IdentityField,
    value: String,
}

impl Handle {
    fn new(field: IdentityField, value: &str) -> Self {
        Self {
            field,
            value: field.comparison_key(value),
        }
    }
}

/// An immutable merged view. Persons are ordered console first (ascending user id, aligned
/// with `entries`), then config (ascending key); the first person to claim a handle owns it.
#[derive(Default)]
struct Directory {
    entries: Vec<ConsoleIdentity>,
    consoles: HashMap<i64, usize>,
    persons: Vec<Person>,
    keys: HashMap<String, usize>,
    handles: HashMap<Handle, usize>,
}

impl Directory {
    fn build(seed: &Seed, mut entries: Vec<ConsoleIdentity>) -> Self {
        entries.sort_by_key(|entry| entry.user_id);
        entries.dedup_by_key(|entry| entry.user_id);
        let console_keys = assign_keys(seed, &entries);

        let claimed: HashSet<&str> = console_keys.iter().map(String::as_str).collect();
        let mut config_keys: Vec<&String> = seed
            .users
            .keys()
            .filter(|key| !claimed.contains(key.as_str()))
            .collect();
        config_keys.sort();

        let console_persons = entries
            .iter()
            .zip(&console_keys)
            .map(|(entry, key)| Person {
                key: key.clone(),
                origin: IdentityOrigin::Console,
                user: UserConfig::from(entry.identities.clone()),
            });
        let config_persons = config_keys.into_iter().map(|key| Person {
            key: key.clone(),
            origin: IdentityOrigin::Config,
            user: seed.users[key].clone(),
        });
        let persons: Vec<Person> = console_persons.chain(config_persons).collect();

        let mut keys: HashMap<String, usize> = HashMap::with_capacity(persons.len());
        let mut handles: HashMap<Handle, usize> = HashMap::new();
        for (index, person) in persons.iter().enumerate() {
            keys.entry(person.key.clone()).or_insert(index);
            for field in IdentityField::EXCLUSIVE {
                for value in exclusive_values(&person.user, field) {
                    handles.entry(Handle::new(field, value)).or_insert(index);
                }
            }
        }
        let mut consoles: HashMap<i64, usize> = HashMap::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            keys.insert(console_key(entry.user_id), index);
            consoles.insert(entry.user_id, index);
        }

        Self {
            entries,
            consoles,
            persons,
            keys,
            handles,
        }
    }

    /// The key the stored console person for `entry.user_id` holds, when `entry` has the
    /// same slug and login email, since nothing else about the entry affects its key.
    fn stored_key(&self, entry: &ConsoleIdentity) -> Option<&str> {
        let &index = self.consoles.get(&entry.user_id)?;
        let stored = &self.entries[index];
        let unchanged = explicit_slug(stored) == explicit_slug(entry)
            && email_key(&stored.login_email) == email_key(&entry.login_email);
        unchanged.then(|| self.persons[index].key.as_str())
    }

    fn person(&self, key: &str) -> Option<&Person> {
        self.keys.get(key).map(|&index| &self.persons[index])
    }

    fn handle_owner(&self, field: IdentityField, value: &str) -> Option<&Person> {
        self.handles
            .get(&Handle::new(field, value))
            .map(|&index| &self.persons[index])
    }

    fn resolve_reference(&self, channel: NotificationChannel, reference: &str) -> String {
        self.person(reference)
            .and_then(|person| channel.address(&person.user))
            .unwrap_or(reference)
            .to_string()
    }
}

/// Person keys for `entries` (sorted by user id), aligned by index: explicit slugs first,
/// then login-email matches to config by ascending user id, then `console-<id>`.
fn assign_keys(seed: &Seed, entries: &[ConsoleIdentity]) -> Vec<String> {
    let mut keys: Vec<Option<String>> = vec![None; entries.len()];
    let mut claimed: HashSet<String> = HashSet::new();
    for (key, entry) in keys.iter_mut().zip(entries) {
        if let Some(slug) = explicit_slug(entry) {
            if claimed.insert(slug.to_string()) {
                *key = Some(slug.to_string());
            }
        }
    }
    for (key, entry) in keys.iter_mut().zip(entries) {
        if key.is_some() {
            continue;
        }
        if let Some(matched) = seed
            .keys_for_email(&entry.login_email)
            .iter()
            .find(|candidate| !claimed.contains(candidate.as_str()))
        {
            claimed.insert(matched.clone());
            *key = Some(matched.clone());
        }
    }
    keys.into_iter()
        .zip(entries)
        .map(|(key, entry)| key.unwrap_or_else(|| console_key(entry.user_id)))
        .collect()
}

fn explicit_slug(entry: &ConsoleIdentity) -> Option<&str> {
    entry
        .slug
        .as_deref()
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
}

fn console_key(user_id: i64) -> String {
    format!("{CONSOLE_KEY_PREFIX}{user_id}")
}

fn email_key(email: &str) -> Option<String> {
    let trimmed = email.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_lowercase())
}

fn source_field(source: &str) -> Option<IdentityField> {
    SOURCES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(source))
        .map(|(_, field)| *field)
}

fn exclusive_values(user: &UserConfig, field: IdentityField) -> Vec<&str> {
    fn list(values: &[String]) -> Vec<&str> {
        values.iter().map(String::as_str).collect()
    }
    match field {
        IdentityField::LinearNames => list(&user.linear_names),
        IdentityField::GithubUsernames => list(&user.github_usernames),
        IdentityField::SentryUsernames => list(&user.sentry_usernames),
        IdentityField::JiraUsernames => list(&user.jira_usernames),
        IdentityField::GitlabUsernames => list(&user.gitlab_usernames),
        IdentityField::DiscordId => user.discord_id.as_deref().into_iter().collect(),
        IdentityField::SlackId => user.slack_id.as_deref().into_iter().collect(),
        IdentityField::Slug
        | IdentityField::Email
        | IdentityField::PushUserKey
        | IdentityField::SmsNumber
        | IdentityField::WhatsappNumber
        | IdentityField::TelegramChatId => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_core::types::Issue;

    fn test_users() -> HashMap<String, UserConfig> {
        let mut users = HashMap::new();
        users.insert(
            "jake".to_string(),
            UserConfig {
                linear_names: vec!["Jake Barnwell".to_string()],
                github_usernames: vec!["jakebarnby".to_string()],
                sentry_usernames: vec!["jake".to_string()],
                jira_usernames: vec!["jake.barnby".to_string()],
                gitlab_usernames: vec!["jakebarnby".to_string()],
                discord_id: Some("123456789".to_string()),
                slack_id: Some("U_JAKE_SLACK".to_string()),
                email: Some("jake@example.com".to_string()),
                push_user_key: Some("push_key_jake".to_string()),
                sms_number: Some("+1234567890".to_string()),
                whatsapp_number: None,
                telegram_chat_id: None,
            },
        );
        users.insert(
            "alice".to_string(),
            UserConfig {
                linear_names: vec!["Alice Smith".to_string()],
                github_usernames: vec!["alicesmith".to_string()],
                sentry_usernames: vec![],
                jira_usernames: vec![],
                gitlab_usernames: vec![],
                discord_id: Some("987654321".to_string()),
                slack_id: None,
                email: Some("alice@example.com".to_string()),
                push_user_key: None,
                sms_number: None,
                whatsapp_number: None,
                telegram_chat_id: None,
            },
        );
        users
    }

    #[test]
    fn test_resolve_linear_assignee() {
        let registry = UserRegistry::new(test_users());
        let resolved = registry.resolve("linear", "Jake Barnwell").unwrap();
        assert_eq!(resolved.slug, "jake");
        assert_eq!(resolved.discord_id.as_deref(), Some("123456789"));
        assert_eq!(resolved.email.as_deref(), Some("jake@example.com"));
        assert_eq!(resolved.push_user_key.as_deref(), Some("push_key_jake"));
        assert_eq!(resolved.sms_number.as_deref(), Some("+1234567890"));
    }

    #[test]
    fn test_resolve_github_username() {
        let registry = UserRegistry::new(test_users());
        let resolved = registry.resolve("github", "alicesmith").unwrap();
        assert_eq!(resolved.slug, "alice");
        assert_eq!(resolved.discord_id.as_deref(), Some("987654321"));
        assert_eq!(resolved.email.as_deref(), Some("alice@example.com"));
        assert!(resolved.push_user_key.is_none());
        assert!(resolved.sms_number.is_none());
    }

    #[test]
    fn test_resolve_sentry_username() {
        let registry = UserRegistry::new(test_users());
        let resolved = registry.resolve("sentry", "jake").unwrap();
        assert_eq!(resolved.slug, "jake");
        assert_eq!(resolved.discord_id.as_deref(), Some("123456789"));
    }

    #[test]
    fn test_resolve_no_match() {
        let registry = UserRegistry::new(test_users());
        assert!(registry.resolve("linear", "Unknown Person").is_none());
    }

    #[test]
    fn test_resolve_jira_username() {
        let registry = UserRegistry::new(test_users());
        let resolved = registry.resolve("jira", "jake.barnby").unwrap();
        assert_eq!(resolved.slug, "jake");
        assert_eq!(resolved.discord_id.as_deref(), Some("123456789"));
    }

    #[test]
    fn test_resolve_unknown_source() {
        let registry = UserRegistry::new(test_users());
        assert!(registry.resolve("pagerduty", "Jake Barnwell").is_none());
    }

    #[test]
    fn test_resolve_case_sensitive_source() {
        let registry = UserRegistry::new(test_users());
        // Source is case-insensitive (lowercased), so "LINEAR" should work
        let resolved = registry.resolve("LINEAR", "Jake Barnwell");
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().slug, "jake");

        // But assignee value is case-sensitive
        assert!(registry.resolve("LINEAR", "jake barnwell").is_none());
    }

    #[test]
    fn test_get_by_slug() {
        let registry = UserRegistry::new(test_users());
        let jake = registry.get_by_slug("jake");
        assert!(jake.is_some());
        assert_eq!(jake.unwrap().linear_names, vec!["Jake Barnwell"]);

        assert!(registry.get_by_slug("unknown").is_none());
    }

    #[test]
    fn test_empty_registry() {
        let registry = UserRegistry::new(HashMap::new());
        assert!(registry.is_empty());
        assert!(registry.resolve("linear", "Anyone").is_none());
    }

    #[test]
    fn test_resolve_and_store_in_issue_metadata() {
        let registry = UserRegistry::new(test_users());

        // Create an issue with an assignee
        let mut issue = Issue::new(
            "issue-1",
            "PROJ-1",
            "Fix the bug",
            "https://example.com/issue-1",
            "linear",
        );
        issue.set_metadata("assignee", "Jake Barnwell");

        // Resolve the assignee
        let assignee_value: String = issue.get_metadata("assignee").unwrap();
        let resolved = registry.resolve(&issue.source, &assignee_value).unwrap();

        // Store the resolved user slug in metadata
        issue.set_metadata("resolved_user", &resolved.slug);
        issue.set_metadata(
            "resolved_discord_id",
            resolved.discord_id.as_deref().unwrap_or(""),
        );

        // Verify
        assert_eq!(
            issue.get_metadata::<String>("resolved_user"),
            Some("jake".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("resolved_discord_id"),
            Some("123456789".to_string())
        );
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

    fn handles(github: &[&str], discord_id: Option<&str>) -> Identities {
        Identities {
            github_usernames: github.iter().map(|login| login.to_string()).collect(),
            discord_id: discord_id.map(str::to_string),
            ..Identities::default()
        }
    }

    fn config_user(github: &[&str], discord_id: Option<&str>, email: Option<&str>) -> UserConfig {
        UserConfig {
            github_usernames: github.iter().map(|login| login.to_string()).collect(),
            discord_id: discord_id.map(str::to_string),
            email: email.map(str::to_string),
            ..UserConfig::default()
        }
    }

    #[test]
    fn config_key_with_console_prefix_never_shadows_a_console_alias() {
        let mut users = test_users();
        users.insert(
            "console-3".to_string(),
            config_user(&["impostor"], None, Some("impostor@example.com")),
        );
        let registry = UserRegistry::new(users);
        assert!(registry.get_by_slug("console-3").is_none());
        assert!(registry.resolve("github", "impostor").is_none());

        let identities = handles(&["real-console"], None);
        let entry = console(3, "impostor@example.com", None, identities.clone());
        registry.replace_console(vec![entry.clone()]);

        assert_eq!(
            registry.get_by_slug("console-3"),
            Some(UserConfig::from(identities))
        );
        assert_eq!(registry.origin("console-3"), Some(IdentityOrigin::Console));
        assert_eq!(registry.person_key(&entry), "console-3");
    }

    #[test]
    fn console_alias_wins_over_a_console_slug_naming_another_user() {
        let registry = UserRegistry::new(test_users());
        registry.replace_console(vec![
            console(
                3,
                "three@example.com",
                Some("console-5"),
                handles(&["three"], None),
            ),
            console(5, "five@example.com", None, handles(&["five"], None)),
        ]);

        assert_eq!(
            registry.get_by_slug("console-5").unwrap().github_usernames,
            vec!["five"]
        );
        assert_eq!(
            registry.get_by_slug("console-3").unwrap().github_usernames,
            vec!["three"]
        );
    }

    #[test]
    fn console_overrides_config_by_explicit_slug_wholesale() {
        let registry = UserRegistry::new(test_users());
        let identities = handles(&["jake-console"], Some("452316113016193024"));
        registry.replace_console(vec![console(
            7,
            "someone@appwrite.io",
            Some("jake"),
            identities.clone(),
        )]);

        assert_eq!(
            registry.get_by_slug("jake"),
            Some(UserConfig::from(identities))
        );
        assert!(registry.resolve("linear", "Jake Barnwell").is_none());
        assert!(registry.resolve("github", "jakebarnby").is_none());
        let resolved = registry.resolve("github", "jake-console").unwrap();
        assert_eq!(resolved.slug, "jake");
        assert!(resolved.sms_number.is_none());
        assert_eq!(registry.origin("jake"), Some(IdentityOrigin::Console));
        assert_eq!(registry.origin("alice"), Some(IdentityOrigin::Config));
        assert_eq!(registry.origin("unknown"), None);
    }

    #[test]
    fn console_overrides_config_by_login_email_match() {
        let registry = UserRegistry::new(test_users());
        registry.replace_console(vec![console(
            3,
            " JAKE@Example.com ",
            None,
            handles(&["jake-console"], None),
        )]);

        let jake = registry.get_by_slug("jake").unwrap();
        assert_eq!(jake.github_usernames, vec!["jake-console"]);
        assert!(jake.sms_number.is_none());
        assert!(jake.linear_names.is_empty());
        assert_eq!(
            registry.resolve("github", "jake-console").unwrap().slug,
            "jake"
        );
        assert_eq!(registry.origin("jake"), Some(IdentityOrigin::Console));
        assert!(registry.get_by_slug("console-3").is_some());
    }

    #[test]
    fn identity_email_matching_config_does_not_link() {
        let registry = UserRegistry::new(test_users());
        let identities = Identities {
            github_usernames: vec!["someone".to_string()],
            email: Some("jake@example.com".to_string()),
            ..Identities::default()
        };
        let entry = console(4, "someone@appwrite.io", None, identities);
        registry.replace_console(vec![entry.clone()]);

        assert_eq!(registry.person_key(&entry), "console-4");
        assert_eq!(
            registry.resolve("github", "someone").unwrap().slug,
            "console-4"
        );
        assert_eq!(
            registry.resolve("github", "jakebarnby").unwrap().slug,
            "jake"
        );
        assert_eq!(registry.origin("jake"), Some(IdentityOrigin::Config));
        assert_eq!(
            registry.get_by_slug("jake").unwrap().linear_names,
            vec!["Jake Barnwell"]
        );
    }

    #[test]
    fn unmatched_console_user_gets_console_key() {
        let registry = UserRegistry::new(test_users());
        registry.replace_console(vec![console(
            9,
            "new@appwrite.io",
            None,
            handles(&["newcomer"], None),
        )]);

        assert_eq!(
            registry.resolve("github", "newcomer").unwrap().slug,
            "console-9"
        );
        assert_eq!(registry.origin("console-9"), Some(IdentityOrigin::Console));
        assert!(registry.users().contains_key("console-9"));
    }

    #[test]
    fn console_alias_resolves_after_slug_is_added() {
        let registry = UserRegistry::new(test_users());
        registry.replace_console(vec![console(
            5,
            "amy@appwrite.io",
            None,
            handles(&["amy"], Some("452316113016193024")),
        )]);
        assert_eq!(registry.resolve("github", "amy").unwrap().slug, "console-5");

        registry.replace_console(vec![console(
            5,
            "amy@appwrite.io",
            Some("amy"),
            handles(&["amy"], Some("452316113016193024")),
        )]);

        assert_eq!(registry.resolve("github", "amy").unwrap().slug, "amy");
        let alias = registry.get_by_slug("console-5").unwrap();
        assert_eq!(alias.discord_id.as_deref(), Some("452316113016193024"));
        assert_eq!(registry.get_by_slug("amy"), Some(alias));
        assert_eq!(registry.origin("console-5"), Some(IdentityOrigin::Console));
        assert_eq!(
            registry.resolve_reference(NotificationChannel::Discord, "console-5"),
            "452316113016193024"
        );
        let users = registry.users();
        assert!(users.contains_key("amy"));
        assert!(!users.contains_key("console-5"));
    }

    #[test]
    fn clones_share_replace_console() {
        let registry = UserRegistry::new(test_users());
        let clone = registry.clone();

        registry.replace_console(vec![console(
            1,
            "a@appwrite.io",
            None,
            handles(&["a"], None),
        )]);
        assert_eq!(clone.resolve("github", "a").unwrap().slug, "console-1");

        clone.replace_console(vec![console(
            2,
            "b@appwrite.io",
            None,
            handles(&["b"], None),
        )]);
        assert!(registry.resolve("github", "a").is_none());
        assert_eq!(registry.resolve("github", "b").unwrap().slug, "console-2");
    }

    #[test]
    fn empty_replace_console_restores_config() {
        let registry = UserRegistry::new(test_users());
        registry.replace_console(vec![console(
            3,
            "jake@example.com",
            None,
            handles(&["jake-console"], None),
        )]);
        registry.replace_console(Vec::new());

        assert_eq!(registry.users(), test_users());
        assert_eq!(registry.origin("jake"), Some(IdentityOrigin::Config));
        assert!(registry.get_by_slug("console-3").is_none());
        assert_eq!(
            registry.resolve("github", "jakebarnby").unwrap().slug,
            "jake"
        );
    }

    #[test]
    fn console_beats_config_on_handle_collision() {
        let registry = UserRegistry::new(test_users());
        registry.replace_console(vec![console(
            2,
            "other@appwrite.io",
            None,
            handles(&["AliceSmith"], None),
        )]);

        assert_eq!(
            registry.resolve("github", "alicesmith").unwrap().slug,
            "console-2"
        );
        assert_eq!(
            registry.owner(IdentityField::GithubUsernames, "alicesmith"),
            Some("console-2".to_string())
        );
    }

    #[test]
    fn lower_key_and_lower_user_id_win_collisions() {
        for _ in 0..16 {
            let mut users = HashMap::new();
            users.insert("carol".to_string(), config_user(&["shared"], None, None));
            users.insert("bob".to_string(), config_user(&["shared"], None, None));
            users.insert("dave".to_string(), config_user(&["shared"], None, None));
            let registry = UserRegistry::new(users);
            assert_eq!(registry.resolve("github", "shared").unwrap().slug, "bob");

            registry.replace_console(vec![
                console(12, "x@appwrite.io", None, handles(&["twin"], None)),
                console(11, "y@appwrite.io", None, handles(&["twin"], None)),
            ]);
            assert_eq!(
                registry.resolve("github", "twin").unwrap().slug,
                "console-11"
            );
        }
    }

    #[test]
    fn explicit_slug_beats_email_match_and_lower_id_wins_email_ties() {
        let registry = UserRegistry::new(test_users());
        registry.replace_console(vec![
            console(2, "jake@example.com", None, handles(&["by-email"], None)),
            console(
                8,
                "boss@appwrite.io",
                Some("jake"),
                handles(&["by-slug"], None),
            ),
            console(3, "alice@example.com", None, handles(&["alice-low"], None)),
            console(4, "ALICE@example.com", None, handles(&["alice-high"], None)),
        ]);

        assert_eq!(registry.resolve("github", "by-slug").unwrap().slug, "jake");
        assert_eq!(
            registry.resolve("github", "by-email").unwrap().slug,
            "console-2"
        );
        assert_eq!(
            registry.resolve("github", "alice-low").unwrap().slug,
            "alice"
        );
        assert_eq!(
            registry.resolve("github", "alice-high").unwrap().slug,
            "console-4"
        );
    }

    #[test]
    fn owner_checks_lists_and_scalars() {
        let registry = UserRegistry::new(test_users());
        let jake = Some("jake".to_string());

        assert_eq!(
            registry.owner(IdentityField::LinearNames, "Jake Barnwell"),
            jake
        );
        assert_eq!(
            registry.owner(IdentityField::LinearNames, "jake barnwell"),
            None
        );
        assert_eq!(
            registry.owner(IdentityField::GithubUsernames, "JakeBarnby"),
            jake
        );
        assert_eq!(
            registry.owner(IdentityField::GitlabUsernames, "JAKEBARNBY"),
            jake
        );
        assert_eq!(
            registry.owner(IdentityField::JiraUsernames, "jake.barnby"),
            jake
        );
        assert_eq!(registry.owner(IdentityField::SentryUsernames, "jake"), jake);
        assert_eq!(registry.owner(IdentityField::DiscordId, "123456789"), jake);
        assert_eq!(registry.owner(IdentityField::SlackId, "U_JAKE_SLACK"), jake);
        assert_eq!(registry.owner(IdentityField::DiscordId, "000"), None);
        assert_eq!(
            registry.owner(IdentityField::Email, "jake@example.com"),
            None
        );
        assert_eq!(
            registry.owner(IdentityField::SmsNumber, "+1234567890"),
            None
        );
        assert_eq!(
            registry.owner(IdentityField::PushUserKey, "push_key_jake"),
            None
        );
        assert_eq!(registry.owner(IdentityField::Slug, "jake"), None);

        registry.replace_console(vec![console(
            7,
            "boss@appwrite.io",
            Some("jake"),
            Identities::default(),
        )]);
        assert_eq!(registry.owner(IdentityField::Slug, "jake"), jake);
        assert_eq!(registry.owner(IdentityField::Slug, "console-7"), jake);
        assert_eq!(registry.owner(IdentityField::Slug, "alice"), None);
        assert_eq!(registry.owner(IdentityField::DiscordId, "123456789"), None);
    }

    #[test]
    fn discord_id_for_github_is_case_insensitive() {
        let mut users = test_users();
        users.insert("silent".to_string(), config_user(&["shared"], None, None));
        users.insert(
            "talker".to_string(),
            config_user(&["Shared"], Some("452316113016193024"), None),
        );
        let registry = UserRegistry::new(users);

        assert_eq!(
            registry.discord_id_for_github("JAKEBARNBY").as_deref(),
            Some("123456789")
        );
        assert_eq!(registry.discord_id_for_github("Shared"), None);
        assert_eq!(registry.discord_id_for_github("nobody"), None);

        registry.replace_console(vec![console(
            1,
            "jake@example.com",
            None,
            handles(&["JakeBarnby"], Some("999999999999999999")),
        )]);
        assert_eq!(
            registry.discord_id_for_github("jakebarnby").as_deref(),
            Some("999999999999999999")
        );
    }

    #[test]
    fn resolve_reference_maps_person_keys_and_keeps_literals() {
        let registry = UserRegistry::new(test_users());

        assert_eq!(
            registry.resolve_reference(NotificationChannel::Discord, "jake"),
            "123456789"
        );
        assert_eq!(
            registry.resolve_reference(NotificationChannel::Discord, "452316113016193024"),
            "452316113016193024"
        );
        assert_eq!(
            registry.resolve_reference(NotificationChannel::Whatsapp, "jake"),
            "jake"
        );
        assert_eq!(
            registry.resolve_references(
                NotificationChannel::Email,
                &[
                    "jake".to_string(),
                    "alice".to_string(),
                    "ops@appwrite.io".to_string()
                ]
            ),
            vec!["jake@example.com", "alice@example.com", "ops@appwrite.io"]
        );

        registry.replace_console(vec![console(
            1,
            "jake@example.com",
            None,
            handles(&[], Some("452316113016193024")),
        )]);
        assert_eq!(
            registry.resolve_reference(NotificationChannel::Discord, "jake"),
            "452316113016193024"
        );
        assert_eq!(
            registry.resolve_reference(NotificationChannel::Email, "jake"),
            "jake"
        );
    }

    #[test]
    fn users_returns_merged_snapshot() {
        let registry = UserRegistry::new(test_users());
        let identities = handles(&["jake-console"], None);
        registry.replace_console(vec![
            console(1, "boss@appwrite.io", Some("jake"), identities.clone()),
            console(9, "new@appwrite.io", None, Identities::default()),
        ]);

        let users = registry.users();
        let mut keys: Vec<&str> = users.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, vec!["alice", "console-9", "jake"]);
        assert_eq!(users["jake"], UserConfig::from(identities));
        assert_eq!(users["alice"], test_users()["alice"]);
    }

    #[test]
    fn debug_contains_no_field_values() {
        let registry = UserRegistry::new(test_users());
        registry.replace_console(vec![console(
            1,
            "secret-login@appwrite.io",
            Some("secret-slug"),
            Identities {
                discord_id: Some("452316113016193024".to_string()),
                sms_number: Some("+14155550123".to_string()),
                ..Identities::default()
            },
        )]);

        let debug = format!("{registry:?}");
        assert_eq!(debug, "UserRegistry { persons: 3 }");
        for value in [
            "jake",
            "alice",
            "123456789",
            "jake@example.com",
            "+1234567890",
            "push_key_jake",
            "secret",
            "452316113016193024",
            "+14155550123",
        ] {
            assert!(!debug.contains(value), "debug output leaked {value}");
        }
    }

    #[test]
    fn resolve_is_case_insensitive_for_github_and_gitlab_only() {
        let registry = UserRegistry::new(test_users());
        assert_eq!(
            registry.resolve("github", "JakeBarnby").unwrap().slug,
            "jake"
        );
        assert_eq!(
            registry.resolve("GitLab", "JAKEBARNBY").unwrap().slug,
            "jake"
        );
        assert!(registry.resolve("sentry", "JAKE").is_none());
        assert!(registry.resolve("jira", "Jake.Barnby").is_none());
    }

    #[test]
    fn configured_for_email_matches_seed_case_insensitively() {
        let mut users = test_users();
        users.insert(
            "aardvark".to_string(),
            config_user(&[], None, Some("Alice@Example.com")),
        );
        let registry = UserRegistry::new(users);

        let (key, user) = registry.configured_for_email(" JAKE@example.COM ").unwrap();
        assert_eq!(key, "jake");
        assert_eq!(user.linear_names, vec!["Jake Barnwell"]);
        assert_eq!(
            registry
                .configured_for_email("alice@example.com")
                .unwrap()
                .0,
            "aardvark"
        );
        assert!(registry
            .configured_for_email("nobody@example.com")
            .is_none());
        assert!(registry.configured_for_email("").is_none());
    }

    #[test]
    fn person_key_follows_the_merge_rule() {
        let registry = UserRegistry::new(test_users());
        let unsaved = console(6, "Jake@Example.com", None, Identities::default());
        assert_eq!(registry.person_key(&unsaved), "jake");
        assert_eq!(
            registry.person_key(&console(
                6,
                "x@appwrite.io",
                Some("chosen"),
                Identities::default()
            )),
            "chosen"
        );
        assert_eq!(
            registry.person_key(&console(6, "x@appwrite.io", None, Identities::default())),
            "console-6"
        );

        registry.replace_console(vec![console(
            2,
            "boss@appwrite.io",
            Some("jake"),
            Identities::default(),
        )]);
        assert_eq!(registry.person_key(&unsaved), "console-6");

        let loaded = console(3, "alice@example.com", None, Identities::default());
        registry.replace_console(vec![loaded.clone()]);
        assert_eq!(registry.person_key(&loaded), "alice");
        assert_eq!(
            registry.person_key(&console(
                1,
                "alice@example.com",
                None,
                Identities::default()
            )),
            "alice"
        );
    }

    #[test]
    fn poisoned_lock_recovers() {
        let registry = UserRegistry::new(test_users());
        let poisoner = registry.clone();
        let outcome = std::thread::spawn(move || {
            let _guard = poisoner.directory.write().unwrap();
            panic!("poison the directory lock");
        })
        .join();
        assert!(outcome.is_err());
        assert!(registry.directory.is_poisoned());

        assert!(registry.get_by_slug("jake").is_some());
        registry.replace_console(vec![console(
            1,
            "a@appwrite.io",
            None,
            handles(&["a"], None),
        )]);
        assert_eq!(registry.resolve("github", "a").unwrap().slug, "console-1");
    }

    #[test]
    fn user_config_and_identities_convert_both_ways() {
        let jake = test_users().remove("jake").unwrap();
        let identities = Identities::from(&jake);
        assert_eq!(identities.github_usernames, jake.github_usernames);
        assert_eq!(identities.sms_number, jake.sms_number);
        assert_eq!(UserConfig::from(identities), jake);
    }

    #[test]
    fn default_registry_is_empty() {
        let registry = UserRegistry::default();
        assert!(registry.is_empty());
        assert!(registry.users().is_empty());
        registry.replace_console(vec![console(
            1,
            "a@appwrite.io",
            None,
            Identities::default(),
        )]);
        assert!(!registry.is_empty());
    }

    #[test]
    fn discord_id_for_github_follows_the_handle_owner() {
        let users = HashMap::from([
            ("amy".to_string(), config_user(&["shared"], None, None)),
            (
                "bob".to_string(),
                config_user(&["shared"], Some("452316113016193024"), None),
            ),
        ]);
        let registry = UserRegistry::new(users);

        assert_eq!(registry.resolve("github", "shared").unwrap().slug, "amy");
        assert_eq!(registry.discord_id_for_github("shared"), None);

        registry.replace_console(vec![console(
            4,
            "carl@appwrite.io",
            None,
            handles(&["SHARED"], Some("111111111111111111")),
        )]);
        assert_eq!(
            registry.resolve("github", "shared").unwrap().slug,
            "console-4"
        );
        assert_eq!(
            registry.discord_id_for_github("Shared").as_deref(),
            Some("111111111111111111")
        );
    }

    #[test]
    fn person_key_recomputes_when_a_stored_entry_changes() {
        let registry = UserRegistry::new(test_users());
        let boss = console(2, "boss@appwrite.io", Some("jake"), Identities::default());
        let amy = console(3, "alice@example.com", None, Identities::default());
        registry.replace_console(vec![boss.clone(), amy.clone()]);

        assert_eq!(registry.person_key(&boss), "jake");
        assert_eq!(registry.person_key(&amy), "alice");
        let renamed = ConsoleIdentity {
            name: "Amy Renamed".to_string(),
            identities: handles(&["amy-gh"], None),
            ..amy.clone()
        };
        assert_eq!(registry.person_key(&renamed), "alice");

        let unlinked = ConsoleIdentity {
            slug: None,
            ..boss.clone()
        };
        assert_eq!(registry.person_key(&unlinked), "console-2");
        let moved = ConsoleIdentity {
            slug: None,
            login_email: "Jake@Example.com".to_string(),
            ..boss
        };
        assert_eq!(registry.person_key(&moved), "jake");
        let claims_jake = ConsoleIdentity {
            login_email: "jake@example.com".to_string(),
            ..amy
        };
        assert_eq!(registry.person_key(&claims_jake), "console-3");
    }

    #[test]
    fn reload_never_applies_an_older_snapshot_after_a_newer_one() {
        let registry = UserRegistry::new(HashMap::new());
        let stale = vec![console(1, "a@appwrite.io", None, handles(&["old"], None))];
        let fresh = vec![console(1, "a@appwrite.io", None, handles(&["new"], None))];
        let (listed, started) = std::sync::mpsc::channel::<()>();
        let (finished, newer_done) = std::sync::mpsc::channel::<()>();

        let older = {
            let registry = registry.clone();
            std::thread::spawn(move || {
                registry.reload(|| {
                    listed.send(()).unwrap();
                    let _ = newer_done.recv_timeout(std::time::Duration::from_millis(300));
                    Ok::<_, ()>(stale)
                })
            })
        };
        started.recv().unwrap();
        let newer = {
            let registry = registry.clone();
            std::thread::spawn(move || {
                let outcome = registry.reload(|| Ok::<_, ()>(fresh));
                let _ = finished.send(());
                outcome
            })
        };
        older.join().unwrap().unwrap();
        newer.join().unwrap().unwrap();

        assert!(registry.resolve("github", "old").is_none());
        assert_eq!(registry.resolve("github", "new").unwrap().slug, "console-1");
    }

    #[test]
    fn failed_reload_keeps_the_directory() {
        let registry = UserRegistry::new(HashMap::new());
        registry
            .reload(|| {
                Ok::<_, ()>(vec![console(
                    1,
                    "a@appwrite.io",
                    None,
                    handles(&["a"], None),
                )])
            })
            .unwrap();

        assert_eq!(registry.reload(|| Err("storage down")), Err("storage down"));

        assert_eq!(registry.resolve("github", "a").unwrap().slug, "console-1");
    }
}
