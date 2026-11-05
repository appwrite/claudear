//! Per-mailbox HelpScout profiles and the Security inbox configuration.

use crate::config::ReplyAs;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

pub const HELPSCOUT_SOURCE: &str = "helpscout";
pub const MAILBOX_ID_METADATA_KEY: &str = "mailbox_id";
pub const MAILBOX_PROFILE_METADATA_KEY: &str = "mailbox_profile";
pub const ATTACHMENTS_METADATA_KEY: &str = "attachments";
pub const CUSTOMER_THREAD_COUNT_METADATA_KEY: &str = "customer_thread_count";

pub const DEFAULT_SECURITY_SCOPE: &str = "Out of scope: self-XSS; missing security headers or cookie flags without a demonstrated impact; clickjacking on pages with no sensitive actions; rate limiting on non-authentication endpoints; denial of service and volumetric attacks; social engineering; reports from automated scanners without a proof of concept; vulnerabilities in third-party services or outdated browsers; issues that require physical or privileged device access.";

const SIGNATURE: &str = "\n\nBest regards,\nAppwrite Security Team";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MailboxProfile {
    #[default]
    Support,
    Security,
}

impl MailboxProfile {
    pub const ALL: [Self; 2] = [Self::Support, Self::Security];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Support => "support",
            Self::Security => "security",
        }
    }
}

impl std::fmt::Display for MailboxProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for MailboxProfile {
    type Err = String;

    fn from_str(name: &str) -> std::result::Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|profile| profile.as_str() == name)
            .ok_or_else(|| format!("Unknown mailbox profile '{name}'"))
    }
}

/// Overrides for one HelpScout mailbox. Unset fields inherit the top-level
/// `[issues.helpscout]` values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelpScoutMailboxConfig {
    pub id: String,
    #[serde(default)]
    pub profile: MailboxProfile,
    #[serde(default)]
    pub reply_as: Option<ReplyAs>,
    #[serde(default)]
    pub trigger_tags: Option<Vec<String>>,
    #[serde(default)]
    pub trigger_status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpamAction {
    #[default]
    Close,
    Spam,
}

impl SpamAction {
    pub const ALL: [Self; 2] = [Self::Close, Self::Spam];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Close => "close",
            Self::Spam => "spam",
        }
    }
}

impl std::fmt::Display for SpamAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for SpamAction {
    type Err = String;

    fn from_str(name: &str) -> std::result::Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == name)
            .ok_or_else(|| format!("Unknown spam action '{name}'"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityInboxConfig {
    pub enabled: bool,
    pub process_after: Option<DateTime<Utc>>,
    pub owner_mention: Option<String>,
    pub dashboard_url: Option<String>,
    pub spam_action: SpamAction,
    pub spam_confidence: f64,
    pub acknowledge_valid: bool,
    pub close_after_reply: bool,
    pub assessment_timeout_secs: u64,
    pub max_assessment_attempts: u32,
    pub duplicate_similarity: f64,
    pub duplicate_candidates: usize,
    pub default_repository: Option<String>,
    pub scope: String,
    pub reward: SecurityRewardConfig,
    pub templates: SecurityTemplates,
    pub deployment: SecurityDeploymentConfig,
}

impl Default for SecurityInboxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            process_after: None,
            owner_mention: None,
            dashboard_url: None,
            spam_action: SpamAction::Close,
            spam_confidence: 0.9,
            acknowledge_valid: true,
            close_after_reply: true,
            assessment_timeout_secs: 1800,
            max_assessment_attempts: 3,
            duplicate_similarity: 0.75,
            duplicate_candidates: 5,
            default_repository: None,
            scope: DEFAULT_SECURITY_SCOPE.to_string(),
            reward: SecurityRewardConfig::default(),
            templates: SecurityTemplates::default(),
            deployment: SecurityDeploymentConfig::default(),
        }
    }
}

impl SecurityInboxConfig {
    pub fn assessment_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.assessment_timeout_secs)
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(0.0..=1.0).contains(&self.spam_confidence) {
            return Err(format!(
                "issues.helpscout.security.spam_confidence must be between 0 and 1, got {}",
                self.spam_confidence
            ));
        }
        if !(0.0..=1.0).contains(&self.duplicate_similarity) {
            return Err(format!(
                "issues.helpscout.security.duplicate_similarity must be between 0 and 1, got {}",
                self.duplicate_similarity
            ));
        }
        if self.assessment_timeout_secs == 0 {
            return Err(
                "issues.helpscout.security.assessment_timeout_secs must be greater than 0"
                    .to_string(),
            );
        }
        if self.enabled && self.process_after.is_none() {
            return Err(
                "issues.helpscout.security.process_after is required when the security inbox is enabled, so the existing backlog is never processed"
                    .to_string(),
            );
        }
        self.templates.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityRewardConfig {
    pub campaign: String,
    pub credits: u32,
}

impl Default for SecurityRewardConfig {
    fn default() -> Self {
        Self {
            campaign: "SecurityResearchers".to_string(),
            credits: 100,
        }
    }
}

/// Fixed customer-facing plain-text reply templates. `{reason}` and
/// `{fixed_on}` are substituted by [`render_security_template`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityTemplates {
    pub confirmed: String,
    pub not_reproducible: String,
    pub already_fixed: String,
    pub not_vulnerability: String,
    pub out_of_scope: String,
    pub duplicate: String,
    pub fix_live: String,
    pub fix_live_rewarded: String,
}

impl Default for SecurityTemplates {
    fn default() -> Self {
        let signed = |body: &str| format!("Hi,\n\n{body}{SIGNATURE}");
        Self {
            confirmed: signed(
                "Thank you for your report. We have confirmed the issue and our team is working on a fix. We will let you know as soon as the fix is live on Appwrite Cloud.",
            ),
            not_reproducible: signed(
                "Thank you for taking the time to report this. We investigated the behaviour you described against the current version of Appwrite but were unable to reproduce it. If you can share more details, such as exact steps, the affected endpoint and the response you observed, please reply to this email and our team will take another look.",
            ),
            already_fixed: signed(
                "Thank you for your report. The issue you described had already been identified and fixed on {fixed_on}, before we received your report, so it is not eligible for a reward. We appreciate you helping keep Appwrite secure.",
            ),
            not_vulnerability: signed(
                "Thank you for your report. After reviewing it, we do not consider this a security vulnerability as stated. {reason}\n\nIf you believe we have misunderstood the impact, please reply with a proof of concept that demonstrates it and our team will review it again.",
            ),
            out_of_scope: signed(
                "Thank you for your report. The issue you described is outside the scope of our security program. {reason}\n\nWe appreciate the time you spent looking into it.",
            ),
            duplicate: signed(
                "Thank you for your report. This issue was already reported to us by another researcher, so your report has been marked as a duplicate and is not eligible for a reward. We appreciate the time you spent on it.",
            ),
            fix_live: signed(
                "Good news: the fix for the issue you reported is now live on Appwrite Cloud. Thank you again for reporting it responsibly.",
            ),
            fix_live_rewarded: signed(
                "Good news: the fix for the issue you reported is now live on Appwrite Cloud. As the first researcher to report this issue, you are eligible for a security report reward. A member of our team will be in touch shortly with your Appwrite Cloud credit coupon.",
            ),
        }
    }
}

impl SecurityTemplates {
    fn named(&self) -> [(&'static str, &str); 8] {
        [
            ("confirmed", &self.confirmed),
            ("not_reproducible", &self.not_reproducible),
            ("already_fixed", &self.already_fixed),
            ("not_vulnerability", &self.not_vulnerability),
            ("out_of_scope", &self.out_of_scope),
            ("duplicate", &self.duplicate),
            ("fix_live", &self.fix_live),
            ("fix_live_rewarded", &self.fix_live_rewarded),
        ]
    }

    fn required_placeholder(name: &str) -> Option<&'static str> {
        match name {
            "not_vulnerability" | "out_of_scope" => Some(REASON_PLACEHOLDER),
            "already_fixed" => Some(FIXED_ON_PLACEHOLDER),
            _ => None,
        }
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        for (name, template) in self.named() {
            let field = format!("issues.helpscout.security.templates.{name}");
            if template.trim().is_empty() {
                return Err(format!("{field} must not be empty"));
            }
            if let Some(unknown) =
                placeholders(template).find(|placeholder| !KNOWN_PLACEHOLDERS.contains(placeholder))
            {
                return Err(format!(
                    "{field} uses unknown placeholder {unknown}; only {REASON_PLACEHOLDER} and {FIXED_ON_PLACEHOLDER} are filled in"
                ));
            }
            if let Some(required) = Self::required_placeholder(name) {
                if !template.contains(required) {
                    return Err(format!("{field} must contain {required}"));
                }
            }
        }
        Ok(())
    }
}

/// Every `{identifier}` token in `template`.
fn placeholders(template: &str) -> impl Iterator<Item = &str> {
    template.match_indices('{').filter_map(|(start, _)| {
        let rest = &template[start + 1..];
        let length =
            rest.find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))?;
        (length > 0 && rest[length..].starts_with('}'))
            .then(|| &template[start..start + length + 2])
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SecurityReplyVariables<'a> {
    pub reason: Option<&'a str>,
    pub fixed_on: Option<NaiveDate>,
}

const REASON_PLACEHOLDER: &str = "{reason}";
const FIXED_ON_PLACEHOLDER: &str = "{fixed_on}";
const KNOWN_PLACEHOLDERS: [&str; 2] = [REASON_PLACEHOLDER, FIXED_ON_PLACEHOLDER];

/// Substitute `{reason}` and `{fixed_on}` (as "4 November 2026") in one pass,
/// so substituted text is never re-scanned. Missing values render empty and
/// every other brace is left untouched.
pub fn render_security_template(template: &str, variables: &SecurityReplyVariables<'_>) -> String {
    let fixed_on = variables
        .fixed_on
        .map(|date| date.format("%-d %B %Y").to_string())
        .unwrap_or_default();
    let reason = variables.reason.unwrap_or_default();

    let mut rendered = String::with_capacity(template.len() + reason.len() + fixed_on.len());
    let mut remaining = template;
    while let Some(position) = remaining.find('{') {
        rendered.push_str(&remaining[..position]);
        let candidate = &remaining[position..];
        if let Some(after) = candidate.strip_prefix(REASON_PLACEHOLDER) {
            rendered.push_str(reason);
            remaining = after;
        } else if let Some(after) = candidate.strip_prefix(FIXED_ON_PLACEHOLDER) {
            rendered.push_str(&fixed_on);
            remaining = after;
        } else {
            rendered.push('{');
            remaining = &candidate[1..];
        }
    }
    rendered.push_str(remaining);
    rendered
}

/// Where production versions are pinned, and which repositories they ship.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityDeploymentConfig {
    pub configuration_repository: String,
    pub configuration_reference: String,
    pub applications: Vec<DeploymentApplicationConfig>,
}

impl Default for SecurityDeploymentConfig {
    fn default() -> Self {
        Self {
            configuration_repository: "appwrite-labs/application-configuration".to_string(),
            configuration_reference: "main".to_string(),
            applications: vec![DeploymentApplicationConfig {
                repository: "appwrite-labs/cloud".to_string(),
                image: "appwrite/cloud".to_string(),
                directories: vec![
                    "cloud/production/cloud".to_string(),
                    "cloud/production/cloud-background".to_string(),
                ],
                packages: vec![DeploymentPackageConfig {
                    repository: "appwrite/appwrite".to_string(),
                    package: "appwrite/server-ce".to_string(),
                    lock_file: default_lock_file(),
                }],
            }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentApplicationConfig {
    pub repository: String,
    pub image: String,
    #[serde(default)]
    pub directories: Vec<String>,
    #[serde(default)]
    pub packages: Vec<DeploymentPackageConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentPackageConfig {
    pub repository: String,
    pub package: String,
    #[serde(default = "default_lock_file")]
    pub lock_file: String,
}

fn default_lock_file() -> String {
    "composer.lock".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn profile_and_spam_action_names_round_trip() {
        for profile in MailboxProfile::ALL {
            assert_eq!(MailboxProfile::from_str(profile.as_str()), Ok(profile));
            assert_eq!(profile.to_string(), profile.as_str());
            assert_eq!(
                serde_json::to_value(profile).unwrap(),
                serde_json::json!(profile.as_str())
            );
        }
        for action in SpamAction::ALL {
            assert_eq!(SpamAction::from_str(action.as_str()), Ok(action));
            assert_eq!(action.to_string(), action.as_str());
            assert_eq!(
                serde_json::to_value(action).unwrap(),
                serde_json::json!(action.as_str())
            );
        }
        assert!(MailboxProfile::from_str("billing").is_err());
        assert!(SpamAction::from_str("delete").is_err());
    }

    #[test]
    fn security_inbox_defaults() {
        let config = SecurityInboxConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.process_after, None);
        assert_eq!(config.spam_action, SpamAction::Close);
        assert_eq!(config.spam_confidence, 0.9);
        assert!(config.acknowledge_valid);
        assert!(config.close_after_reply);
        assert_eq!(
            config.assessment_timeout(),
            std::time::Duration::from_secs(1800)
        );
        assert_eq!(config.max_assessment_attempts, 3);
        assert_eq!(config.duplicate_similarity, 0.75);
        assert_eq!(config.duplicate_candidates, 5);
        assert_eq!(config.scope, DEFAULT_SECURITY_SCOPE);
        assert_eq!(config.reward.campaign, "SecurityResearchers");
        assert_eq!(config.reward.credits, 100);
        assert_eq!(
            config.deployment.configuration_repository,
            "appwrite-labs/application-configuration"
        );
        assert_eq!(config.deployment.configuration_reference, "main");
        let cloud = &config.deployment.applications[0];
        assert_eq!(cloud.repository, "appwrite-labs/cloud");
        assert_eq!(cloud.image, "appwrite/cloud");
        assert_eq!(
            cloud.directories,
            vec![
                "cloud/production/cloud",
                "cloud/production/cloud-background"
            ]
        );
        assert_eq!(cloud.packages[0].package, "appwrite/server-ce");
        assert_eq!(cloud.packages[0].repository, "appwrite/appwrite");
        assert_eq!(cloud.packages[0].lock_file, "composer.lock");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn default_templates_are_signed_plain_text() {
        let templates = SecurityTemplates::default();
        let all = [
            &templates.confirmed,
            &templates.not_reproducible,
            &templates.already_fixed,
            &templates.not_vulnerability,
            &templates.out_of_scope,
            &templates.duplicate,
            &templates.fix_live,
            &templates.fix_live_rewarded,
        ];
        for template in all {
            assert!(template.starts_with("Hi,\n\n"), "{template}");
            assert!(
                template.ends_with("\n\nBest regards,\nAppwrite Security Team"),
                "{template}"
            );
            assert!(!template.contains('\u{2014}'), "no em-dashes: {template}");
            assert!(!template.contains('<'), "plain text: {template}");
        }
        assert_eq!(
            templates.confirmed,
            "Hi,\n\nThank you for your report. We have confirmed the issue and our team is working on a fix. We will let you know as soon as the fix is live on Appwrite Cloud.\n\nBest regards,\nAppwrite Security Team"
        );
        assert!(templates.already_fixed.contains("{fixed_on}"));
        assert!(templates.not_vulnerability.contains("{reason}"));
        assert!(templates.out_of_scope.contains("{reason}"));
        assert!(templates.fix_live_rewarded.contains("eligible"));
        assert!(!templates.fix_live.contains("eligible"));
    }

    #[test]
    fn render_substitutes_reason_and_formats_fixed_on() {
        let variables = SecurityReplyVariables {
            reason: Some("The token is scoped to the caller."),
            fixed_on: NaiveDate::from_ymd_opt(2026, 11, 4),
        };
        assert_eq!(
            render_security_template("A {reason} B {fixed_on} C", &variables),
            "A The token is scoped to the caller. B 4 November 2026 C"
        );
        let rendered =
            render_security_template(&SecurityTemplates::default().already_fixed, &variables);
        assert!(rendered.contains("fixed on 4 November 2026, before"));
    }

    #[test]
    fn render_leaves_missing_values_empty_and_other_braces_untouched() {
        let variables = SecurityReplyVariables::default();
        assert_eq!(
            render_security_template("x{reason}y{fixed_on}z {other} {", &variables),
            "xyz {other} {"
        );
    }

    #[test]
    fn render_never_rescans_substituted_text() {
        let variables = SecurityReplyVariables {
            reason: Some("{fixed_on} {reason}"),
            fixed_on: NaiveDate::from_ymd_opt(2026, 1, 1),
        };
        assert_eq!(
            render_security_template("{reason}|{fixed_on}", &variables),
            "{fixed_on} {reason}|1 January 2026"
        );
    }

    #[test]
    fn validate_rejects_out_of_range_thresholds() {
        let mut config = SecurityInboxConfig {
            spam_confidence: 1.5,
            ..Default::default()
        };
        assert!(config.validate().unwrap_err().contains("spam_confidence"));
        config.spam_confidence = f64::NAN;
        assert!(config.validate().unwrap_err().contains("spam_confidence"));
        config.spam_confidence = 0.0;
        config.duplicate_similarity = -0.1;
        assert!(config
            .validate()
            .unwrap_err()
            .contains("duplicate_similarity"));
        config.duplicate_similarity = 1.0;
        assert!(config.validate().is_ok());
        config.assessment_timeout_secs = 0;
        assert!(config
            .validate()
            .unwrap_err()
            .contains("assessment_timeout_secs"));
    }

    #[test]
    fn validate_requires_process_after_when_enabled() {
        let mut config = SecurityInboxConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(config.validate().unwrap_err().contains("process_after"));
        config.process_after = Some(Utc::now());
        assert!(config.validate().is_ok());
        config.enabled = false;
        config.process_after = None;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_templates() {
        let mut config = SecurityInboxConfig::default();
        config.templates.fix_live = " \n ".to_string();
        let error = config.validate().unwrap_err();
        assert!(
            error.contains("templates.fix_live must not be empty"),
            "{error}"
        );
    }

    #[test]
    fn validate_requires_the_placeholders_each_template_fills_in() {
        for (name, edit) in [
            (
                "not_vulnerability",
                (|templates: &mut SecurityTemplates| {
                    templates.not_vulnerability = "Hi, not a vulnerability.".to_string()
                }) as fn(&mut SecurityTemplates),
            ),
            ("out_of_scope", |templates| {
                templates.out_of_scope = "Hi, out of scope.".to_string()
            }),
            ("already_fixed", |templates| {
                templates.already_fixed = "Hi, already fixed. {reason}".to_string()
            }),
        ] {
            let mut config = SecurityInboxConfig::default();
            edit(&mut config.templates);
            let error = config.validate().unwrap_err();
            assert!(
                error.contains(&format!("templates.{name} must contain")),
                "{error}"
            );
        }
    }

    #[test]
    fn validate_rejects_unknown_placeholders() {
        let mut config = SecurityInboxConfig::default();
        config.templates.duplicate = "Hi {name}, this is a duplicate.".to_string();
        let error = config.validate().unwrap_err();
        assert!(error.contains("templates.duplicate"), "{error}");
        assert!(error.contains("{name}"), "{error}");

        config.templates.duplicate = "Hi, {reason} {fixed_on} { } {\"json\": 1} {-} {".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn placeholders_finds_identifier_tokens_only() {
        assert_eq!(
            placeholders("{reason} {x_1} { y } {} {fixed_on").collect::<Vec<_>>(),
            vec!["{reason}", "{x_1}"]
        );
    }

    #[test]
    fn deployment_packages_default_their_lock_file() {
        let deployment: SecurityDeploymentConfig = toml::from_str(
            r#"
            configuration_repository = "org/config"
            [[applications]]
            repository = "org/app"
            image = "org/app"
            directories = ["prod"]
            [[applications.packages]]
            repository = "org/lib"
            package = "org/lib"
            "#,
        )
        .unwrap();
        assert_eq!(deployment.configuration_reference, "main");
        assert_eq!(deployment.applications.len(), 1);
        assert_eq!(
            deployment.applications[0].packages[0].lock_file,
            "composer.lock"
        );
    }
}
