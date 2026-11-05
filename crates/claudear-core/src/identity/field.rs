use serde::{Deserialize, Serialize};
use std::fmt;

/// One identity attribute of a person, named as it appears in JSON and in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityField {
    Slug,
    LinearNames,
    GithubUsernames,
    SentryUsernames,
    JiraUsernames,
    GitlabUsernames,
    DiscordId,
    SlackId,
    Email,
    PushUserKey,
    SmsNumber,
    WhatsappNumber,
    TelegramChatId,
}

impl IdentityField {
    pub const ALL: [IdentityField; 13] = [
        Self::Slug,
        Self::LinearNames,
        Self::GithubUsernames,
        Self::SentryUsernames,
        Self::JiraUsernames,
        Self::GitlabUsernames,
        Self::DiscordId,
        Self::SlackId,
        Self::Email,
        Self::PushUserKey,
        Self::SmsNumber,
        Self::WhatsappNumber,
        Self::TelegramChatId,
    ];

    /// Handles that identify exactly one person. Notification targets (email, phone numbers,
    /// push keys, chat ids) may be shared, for example by a team inbox, so they are not here.
    pub const EXCLUSIVE: [IdentityField; 7] = [
        Self::LinearNames,
        Self::GithubUsernames,
        Self::SentryUsernames,
        Self::JiraUsernames,
        Self::GitlabUsernames,
        Self::DiscordId,
        Self::SlackId,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Slug => "slug",
            Self::LinearNames => "linear_names",
            Self::GithubUsernames => "github_usernames",
            Self::SentryUsernames => "sentry_usernames",
            Self::JiraUsernames => "jira_usernames",
            Self::GitlabUsernames => "gitlab_usernames",
            Self::DiscordId => "discord_id",
            Self::SlackId => "slack_id",
            Self::Email => "email",
            Self::PushUserKey => "push_user_key",
            Self::SmsNumber => "sms_number",
            Self::WhatsappNumber => "whatsapp_number",
            Self::TelegramChatId => "telegram_chat_id",
        }
    }

    /// Personal contact details and delivery keys, hidden from non-admins viewing other people.
    pub fn is_sensitive(self) -> bool {
        matches!(
            self,
            Self::Email
                | Self::PushUserKey
                | Self::SmsNumber
                | Self::WhatsappNumber
                | Self::TelegramChatId
        )
    }

    /// GitHub and GitLab logins compare without regard to case.
    pub fn is_case_insensitive(self) -> bool {
        matches!(self, Self::GithubUsernames | Self::GitlabUsernames)
    }

    /// The form two values of this field are compared in: lowercase for GitHub and GitLab,
    /// unchanged otherwise.
    pub fn comparison_key(self, value: &str) -> String {
        if self.is_case_insensitive() {
            value.to_lowercase()
        } else {
            value.to_string()
        }
    }
}

impl fmt::Display for IdentityField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn all_lists_every_field_once() {
        let unique: HashSet<IdentityField> = IdentityField::ALL.into_iter().collect();
        assert_eq!(unique.len(), IdentityField::ALL.len());
    }

    #[test]
    fn as_str_matches_serde_name() {
        for field in IdentityField::ALL {
            let serialized = serde_json::to_string(&field).expect("serialize field");
            assert_eq!(serialized, format!("\"{}\"", field.as_str()));
            let parsed: IdentityField = serde_json::from_str(&serialized).expect("parse field");
            assert_eq!(parsed, field);
            assert_eq!(field.to_string(), field.as_str());
        }
    }

    #[test]
    fn sensitive_set_is_contact_details_only() {
        let sensitive: HashSet<IdentityField> = IdentityField::ALL
            .into_iter()
            .filter(|field| field.is_sensitive())
            .collect();
        let expected: HashSet<IdentityField> = [
            IdentityField::Email,
            IdentityField::PushUserKey,
            IdentityField::SmsNumber,
            IdentityField::WhatsappNumber,
            IdentityField::TelegramChatId,
        ]
        .into_iter()
        .collect();
        assert_eq!(sensitive, expected);
    }

    #[test]
    fn exclusive_set_is_every_handle_but_notification_targets() {
        let exclusive: HashSet<IdentityField> = IdentityField::EXCLUSIVE.into_iter().collect();
        let expected: HashSet<IdentityField> = IdentityField::ALL
            .into_iter()
            .filter(|field| *field != IdentityField::Slug && !field.is_sensitive())
            .collect();
        assert_eq!(exclusive, expected);
        assert_eq!(exclusive.len(), IdentityField::EXCLUSIVE.len());
    }

    #[test]
    fn comparison_key_lowercases_only_case_insensitive_fields() {
        assert_eq!(
            IdentityField::GithubUsernames.comparison_key("JakeBarnby"),
            "jakebarnby"
        );
        assert_eq!(
            IdentityField::GitlabUsernames.comparison_key("Jake.Gitlab"),
            "jake.gitlab"
        );
        assert_eq!(
            IdentityField::LinearNames.comparison_key("Jake Barnby"),
            "Jake Barnby"
        );
        assert_eq!(IdentityField::SlackId.comparison_key("U0ABC"), "U0ABC");
    }

    #[test]
    fn case_insensitive_set_is_github_and_gitlab() {
        let insensitive: Vec<IdentityField> = IdentityField::ALL
            .into_iter()
            .filter(|field| field.is_case_insensitive())
            .collect();
        assert_eq!(
            insensitive,
            vec![
                IdentityField::GithubUsernames,
                IdentityField::GitlabUsernames
            ]
        );
    }
}
