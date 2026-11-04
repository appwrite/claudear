use super::field::IdentityField;
use super::validation::validate_value;
use super::violation::IdentityViolation;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

const REDACTED: &str = "<redacted>";

/// Every handle a person is known by, across sources, chat and notification channels.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Identities {
    pub linear_names: Vec<String>,
    pub github_usernames: Vec<String>,
    pub sentry_usernames: Vec<String>,
    pub jira_usernames: Vec<String>,
    pub gitlab_usernames: Vec<String>,
    pub discord_id: Option<String>,
    pub slack_id: Option<String>,
    pub email: Option<String>,
    pub push_user_key: Option<String>,
    pub sms_number: Option<String>,
    pub whatsapp_number: Option<String>,
    pub telegram_chat_id: Option<String>,
}

enum Slot<'a> {
    List(&'a [String]),
    Scalar(Option<&'a str>),
}

impl<'a> Slot<'a> {
    fn values(&self) -> Vec<&'a str> {
        match self {
            Self::List(values) => values.iter().map(String::as_str).collect(),
            Self::Scalar(value) => value.iter().copied().collect(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::List(values) => values.is_empty(),
            Self::Scalar(value) => value.is_none(),
        }
    }
}

enum SlotMut<'a> {
    List(&'a mut Vec<String>),
    Scalar(&'a mut Option<String>),
}

impl SlotMut<'_> {
    fn clear(self) {
        match self {
            Self::List(values) => values.clear(),
            Self::Scalar(value) => *value = None,
        }
    }
}

impl Identities {
    /// Trim every value, drop empty ones, strip a leading `@` from GitHub and GitLab logins,
    /// and remove duplicates (case-insensitively for GitHub and GitLab), keeping first-seen order.
    pub fn normalized(mut self) -> Self {
        for field in IdentityField::ALL {
            match self.slot_mut(field) {
                Some(SlotMut::List(values)) => {
                    let mut seen = HashSet::new();
                    let normalized: Vec<String> = std::mem::take(values)
                        .iter()
                        .filter_map(|value| normalize_value(field, value))
                        .filter(|value| seen.insert(deduplication_key(field, value)))
                        .collect();
                    *values = normalized;
                }
                Some(SlotMut::Scalar(value)) => {
                    *value = value
                        .take()
                        .and_then(|current| normalize_value(field, &current));
                }
                None => {}
            }
        }
        self
    }

    /// Check every value, returning all violations rather than stopping at the first.
    pub fn validate(&self) -> Result<(), Vec<IdentityViolation>> {
        let violations: Vec<IdentityViolation> = IdentityField::ALL
            .into_iter()
            .filter_map(|field| self.slot(field).map(|slot| (field, slot)))
            .flat_map(|(field, slot)| {
                slot.values()
                    .into_iter()
                    .filter_map(move |value| validate_value(field, value).err())
            })
            .collect();
        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    /// A copy with every sensitive field cleared.
    pub fn redacted(&self) -> Self {
        let mut redacted = self.clone();
        for field in IdentityField::ALL
            .into_iter()
            .filter(|field| field.is_sensitive())
        {
            if let Some(slot) = redacted.slot_mut(field) {
                slot.clear();
            }
        }
        redacted
    }

    pub fn is_empty(&self) -> bool {
        IdentityField::ALL
            .into_iter()
            .filter_map(|field| self.slot(field))
            .all(|slot| slot.is_empty())
    }

    fn slot(&self, field: IdentityField) -> Option<Slot<'_>> {
        let slot = match field {
            IdentityField::Slug => return None,
            IdentityField::LinearNames => Slot::List(&self.linear_names),
            IdentityField::GithubUsernames => Slot::List(&self.github_usernames),
            IdentityField::SentryUsernames => Slot::List(&self.sentry_usernames),
            IdentityField::JiraUsernames => Slot::List(&self.jira_usernames),
            IdentityField::GitlabUsernames => Slot::List(&self.gitlab_usernames),
            IdentityField::DiscordId => Slot::Scalar(self.discord_id.as_deref()),
            IdentityField::SlackId => Slot::Scalar(self.slack_id.as_deref()),
            IdentityField::Email => Slot::Scalar(self.email.as_deref()),
            IdentityField::PushUserKey => Slot::Scalar(self.push_user_key.as_deref()),
            IdentityField::SmsNumber => Slot::Scalar(self.sms_number.as_deref()),
            IdentityField::WhatsappNumber => Slot::Scalar(self.whatsapp_number.as_deref()),
            IdentityField::TelegramChatId => Slot::Scalar(self.telegram_chat_id.as_deref()),
        };
        Some(slot)
    }

    fn slot_mut(&mut self, field: IdentityField) -> Option<SlotMut<'_>> {
        let slot = match field {
            IdentityField::Slug => return None,
            IdentityField::LinearNames => SlotMut::List(&mut self.linear_names),
            IdentityField::GithubUsernames => SlotMut::List(&mut self.github_usernames),
            IdentityField::SentryUsernames => SlotMut::List(&mut self.sentry_usernames),
            IdentityField::JiraUsernames => SlotMut::List(&mut self.jira_usernames),
            IdentityField::GitlabUsernames => SlotMut::List(&mut self.gitlab_usernames),
            IdentityField::DiscordId => SlotMut::Scalar(&mut self.discord_id),
            IdentityField::SlackId => SlotMut::Scalar(&mut self.slack_id),
            IdentityField::Email => SlotMut::Scalar(&mut self.email),
            IdentityField::PushUserKey => SlotMut::Scalar(&mut self.push_user_key),
            IdentityField::SmsNumber => SlotMut::Scalar(&mut self.sms_number),
            IdentityField::WhatsappNumber => SlotMut::Scalar(&mut self.whatsapp_number),
            IdentityField::TelegramChatId => SlotMut::Scalar(&mut self.telegram_chat_id),
        };
        Some(slot)
    }
}

fn normalize_value(field: IdentityField, value: &str) -> Option<String> {
    let trimmed = value.trim();
    let handle = if field.is_case_insensitive() {
        trimmed.strip_prefix('@').unwrap_or(trimmed)
    } else {
        trimmed
    };
    (!handle.is_empty()).then(|| handle.to_string())
}

fn deduplication_key(field: IdentityField, value: &str) -> String {
    if field.is_case_insensitive() {
        value.to_lowercase()
    } else {
        value.to_string()
    }
}

impl fmt::Debug for Identities {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Identities");
        for field in IdentityField::ALL {
            let Some(slot) = self.slot(field) else {
                continue;
            };
            if field.is_sensitive() && !slot.is_empty() {
                debug.field(field.as_str(), &format_args!("{REDACTED}"));
                continue;
            }
            match slot {
                Slot::List(values) => debug.field(field.as_str(), &values),
                Slot::Scalar(value) => debug.field(field.as_str(), &value),
            };
        }
        debug.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMAIL: &str = "jake@appwrite.io";
    const PUSH_USER_KEY: &str = "uQiRzpo4DXghDmr9QzzfQu27cmVRsG";
    const SMS_NUMBER: &str = "+14155550123";
    const WHATSAPP_NUMBER: &str = "+14155550199";
    const TELEGRAM_CHAT_ID: &str = "-1001234567890";

    fn complete() -> Identities {
        Identities {
            linear_names: vec!["Jake Barnby".to_string()],
            github_usernames: vec!["jakebarnby".to_string()],
            sentry_usernames: vec!["jake.sentry".to_string()],
            jira_usernames: vec!["jake.jira".to_string()],
            gitlab_usernames: vec!["jake.gitlab".to_string()],
            discord_id: Some("452316113016193024".to_string()),
            slack_id: Some("U0123456789".to_string()),
            email: Some(EMAIL.to_string()),
            push_user_key: Some(PUSH_USER_KEY.to_string()),
            sms_number: Some(SMS_NUMBER.to_string()),
            whatsapp_number: Some(WHATSAPP_NUMBER.to_string()),
            telegram_chat_id: Some(TELEGRAM_CHAT_ID.to_string()),
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn complete_identities_are_valid() {
        assert_eq!(complete().validate(), Ok(()));
        assert_eq!(Identities::default().validate(), Ok(()));
    }

    #[test]
    fn normalized_trims_and_drops_empty_values() {
        let identities = Identities {
            linear_names: strings(&["  Jake Barnby  ", "", "   "]),
            jira_usernames: strings(&["\tjake\n"]),
            discord_id: Some(" 452316113016193024 ".to_string()),
            slack_id: Some(String::new()),
            email: Some("   ".to_string()),
            telegram_chat_id: Some(" -100 ".to_string()),
            ..Identities::default()
        }
        .normalized();
        assert_eq!(identities.linear_names, strings(&["Jake Barnby"]));
        assert_eq!(identities.jira_usernames, strings(&["jake"]));
        assert_eq!(identities.discord_id.as_deref(), Some("452316113016193024"));
        assert_eq!(identities.slack_id, None);
        assert_eq!(identities.email, None);
        assert_eq!(identities.telegram_chat_id.as_deref(), Some("-100"));
    }

    #[test]
    fn normalized_strips_one_leading_at_from_github_and_gitlab() {
        let identities = Identities {
            github_usernames: strings(&["@jakebarnby", " @octocat ", "@@double", "@"]),
            gitlab_usernames: strings(&["@jake.gitlab"]),
            linear_names: strings(&["@Jake"]),
            sentry_usernames: strings(&["@jake"]),
            ..Identities::default()
        }
        .normalized();
        assert_eq!(
            identities.github_usernames,
            strings(&["jakebarnby", "octocat", "@double"])
        );
        assert_eq!(identities.gitlab_usernames, strings(&["jake.gitlab"]));
        assert_eq!(identities.linear_names, strings(&["@Jake"]));
        assert_eq!(identities.sentry_usernames, strings(&["@jake"]));
    }

    #[test]
    fn normalized_deduplicates_github_and_gitlab_case_insensitively() {
        let identities = Identities {
            github_usernames: strings(&["JakeBarnby", "jakebarnby", "@JAKEBARNBY", "octocat"]),
            gitlab_usernames: strings(&["Jake", "jake"]),
            ..Identities::default()
        }
        .normalized();
        assert_eq!(
            identities.github_usernames,
            strings(&["JakeBarnby", "octocat"])
        );
        assert_eq!(identities.gitlab_usernames, strings(&["Jake"]));
    }

    #[test]
    fn normalized_deduplicates_other_lists_exactly() {
        let identities = Identities {
            linear_names: strings(&["Jake", "jake", "Jake", " Jake "]),
            sentry_usernames: strings(&["a", "a"]),
            jira_usernames: strings(&["X", "x"]),
            ..Identities::default()
        }
        .normalized();
        assert_eq!(identities.linear_names, strings(&["Jake", "jake"]));
        assert_eq!(identities.sentry_usernames, strings(&["a"]));
        assert_eq!(identities.jira_usernames, strings(&["X", "x"]));
    }

    #[test]
    fn normalized_keeps_complete_identities_unchanged() {
        assert_eq!(complete().normalized(), complete());
    }

    #[test]
    fn validate_collects_every_violation() {
        let identities = Identities {
            github_usernames: strings(&["a--b", "ok", "-a"]),
            discord_id: Some("123".to_string()),
            email: Some("not-an-email".to_string()),
            sms_number: Some("1234567".to_string()),
            ..Identities::default()
        };
        let violations = identities.validate().expect_err("invalid identities");
        let fields: Vec<IdentityField> =
            violations.iter().map(|violation| violation.field).collect();
        assert_eq!(
            fields,
            vec![
                IdentityField::GithubUsernames,
                IdentityField::GithubUsernames,
                IdentityField::DiscordId,
                IdentityField::Email,
                IdentityField::SmsNumber,
            ]
        );
        assert_eq!(violations[0].message, "'a--b' is not a valid GitHub login");
        assert_eq!(violations[1].message, "'-a' is not a valid GitHub login");
    }

    #[test]
    fn validate_reports_each_field_rule() {
        let cases: Vec<(IdentityField, Identities)> = vec![
            (
                IdentityField::LinearNames,
                Identities {
                    linear_names: strings(&["bad\u{7}"]),
                    ..Identities::default()
                },
            ),
            (
                IdentityField::SentryUsernames,
                Identities {
                    sentry_usernames: strings(&[""]),
                    ..Identities::default()
                },
            ),
            (
                IdentityField::JiraUsernames,
                Identities {
                    jira_usernames: vec!["a".repeat(256)],
                    ..Identities::default()
                },
            ),
            (
                IdentityField::GitlabUsernames,
                Identities {
                    gitlab_usernames: strings(&["_jake"]),
                    ..Identities::default()
                },
            ),
            (
                IdentityField::SlackId,
                Identities {
                    slack_id: Some("C0123456789".to_string()),
                    ..Identities::default()
                },
            ),
            (
                IdentityField::PushUserKey,
                Identities {
                    push_user_key: Some("short".to_string()),
                    ..Identities::default()
                },
            ),
            (
                IdentityField::WhatsappNumber,
                Identities {
                    whatsapp_number: Some("+0123456789".to_string()),
                    ..Identities::default()
                },
            ),
            (
                IdentityField::TelegramChatId,
                Identities {
                    telegram_chat_id: Some("chat".to_string()),
                    ..Identities::default()
                },
            ),
        ];
        for (field, identities) in cases {
            let violations = identities.validate().expect_err("invalid identities");
            assert_eq!(violations.len(), 1, "{field} must produce one violation");
            assert_eq!(violations[0].field, field);
        }
    }

    #[test]
    fn redacted_clears_exactly_the_sensitive_fields() {
        let identities = complete();
        let redacted = identities.redacted();
        assert_eq!(
            redacted,
            Identities {
                email: None,
                push_user_key: None,
                sms_number: None,
                whatsapp_number: None,
                telegram_chat_id: None,
                ..identities.clone()
            }
        );
        assert_eq!(redacted.github_usernames, identities.github_usernames);
        assert_eq!(redacted.discord_id, identities.discord_id);
        assert_eq!(redacted.slack_id, identities.slack_id);
    }

    #[test]
    fn redacted_json_has_null_sensitive_fields() {
        let json = serde_json::to_value(complete().redacted()).expect("serialize");
        for field in IdentityField::ALL
            .into_iter()
            .filter(|field| field.is_sensitive())
        {
            assert_eq!(json[field.as_str()], serde_json::Value::Null, "{field}");
        }
        assert_eq!(json["github_usernames"], serde_json::json!(["jakebarnby"]));
    }

    #[test]
    fn debug_never_prints_sensitive_values() {
        let output = format!("{:?}", complete());
        for secret in [
            EMAIL,
            PUSH_USER_KEY,
            SMS_NUMBER,
            WHATSAPP_NUMBER,
            TELEGRAM_CHAT_ID,
        ] {
            assert!(!output.contains(secret), "Debug leaked a sensitive value");
        }
        assert!(output.contains("email: <redacted>"));
        assert!(output.contains("jakebarnby"));
        assert!(output.contains("452316113016193024"));

        let pretty = format!("{:#?}", complete());
        for secret in [
            EMAIL,
            PUSH_USER_KEY,
            SMS_NUMBER,
            WHATSAPP_NUMBER,
            TELEGRAM_CHAT_ID,
        ] {
            assert!(
                !pretty.contains(secret),
                "pretty Debug leaked a sensitive value"
            );
        }
    }

    #[test]
    fn debug_shows_unset_sensitive_fields_as_none() {
        let output = format!("{:?}", Identities::default());
        assert!(output.contains("email: None"));
        assert!(!output.contains(REDACTED));
    }

    #[test]
    fn is_empty_considers_every_field() {
        assert!(Identities::default().is_empty());
        assert!(!complete().is_empty());
        assert!(!Identities {
            telegram_chat_id: Some("1".to_string()),
            ..Identities::default()
        }
        .is_empty());
        assert!(!Identities {
            jira_usernames: strings(&["jake"]),
            ..Identities::default()
        }
        .is_empty());
    }

    #[test]
    fn serde_round_trips_and_defaults_missing_fields() {
        let identities = complete();
        let json = serde_json::to_string(&identities).expect("serialize");
        let parsed: Identities = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed, identities);

        let sparse: Identities =
            serde_json::from_str(r#"{"github_usernames": ["jakebarnby"]}"#).expect("parse sparse");
        assert_eq!(
            sparse,
            Identities {
                github_usernames: strings(&["jakebarnby"]),
                ..Identities::default()
            }
        );
    }

    #[test]
    fn serialized_keys_match_identity_field_names() {
        let json = serde_json::to_value(Identities::default()).expect("serialize");
        let keys: HashSet<&str> = json
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        let expected: HashSet<&str> = IdentityField::ALL
            .into_iter()
            .filter(|field| *field != IdentityField::Slug)
            .map(IdentityField::as_str)
            .collect();
        assert_eq!(keys, expected);
    }
}
