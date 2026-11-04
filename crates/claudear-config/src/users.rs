//! User registry for resolving issue assignees to notification targets.

use crate::config::UserConfig;
use std::collections::HashMap;

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
#[derive(Debug, Clone)]
pub struct UserRegistry {
    users: HashMap<String, UserConfig>,
}

impl UserRegistry {
    /// Create a new user registry from config.
    pub fn new(users: HashMap<String, UserConfig>) -> Self {
        Self { users }
    }

    /// Resolve an issue's assignee to a user based on the source type.
    ///
    /// For Linear: matches `assignee_value` against each user's `linear_names`
    /// For GitHub: matches against `github_usernames`
    /// For Sentry: matches against `sentry_usernames`
    pub fn resolve(&self, source: &str, assignee_value: &str) -> Option<ResolvedUser> {
        for (slug, user) in &self.users {
            let matched = match source.to_lowercase().as_str() {
                "linear" => user.linear_names.iter().any(|n| n == assignee_value),
                "github" => user.github_usernames.iter().any(|n| n == assignee_value),
                "sentry" => user.sentry_usernames.iter().any(|n| n == assignee_value),
                "jira" => user.jira_usernames.iter().any(|n| n == assignee_value),
                "gitlab" => user.gitlab_usernames.iter().any(|n| n == assignee_value),
                _ => false,
            };
            if matched {
                return Some(ResolvedUser {
                    slug: slug.clone(),
                    discord_id: user.discord_id.clone(),
                    slack_id: user.slack_id.clone(),
                    email: user.email.clone(),
                    push_user_key: user.push_user_key.clone(),
                    sms_number: user.sms_number.clone(),
                });
            }
        }
        None
    }

    /// Look up a user by slug (for resolving global config references).
    pub fn get_by_slug(&self, slug: &str) -> Option<&UserConfig> {
        self.users.get(slug)
    }

    /// Check if the registry has any users configured.
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
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
}
