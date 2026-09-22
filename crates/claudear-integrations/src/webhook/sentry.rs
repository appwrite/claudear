//! Sentry webhook handler.

use super::WebhookHandler;
use async_trait::async_trait;
use claudear_config::config::SentryConfig;
use claudear_core::error::Result;
use claudear_core::types::{Issue, IssuePriority, IssueStatus, MatchPriority, MatchResult};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use subtle::ConstantTimeEq;

/// Map a Sentry severity level and event count to an issue priority.
pub(crate) fn map_priority(level: &str, event_count: i64) -> IssuePriority {
    if level == "fatal" || (level == "error" && event_count > 1000) {
        IssuePriority::Critical
    } else if level == "error" {
        IssuePriority::High
    } else if level == "warning" {
        IssuePriority::Medium
    } else {
        IssuePriority::Low
    }
}

/// Map a Sentry status string to an `IssueStatus`.
pub(crate) fn map_status(status: &str) -> IssueStatus {
    match status {
        "resolved" => IssueStatus::Resolved,
        "ignored" => IssueStatus::Ignored,
        _ => IssueStatus::Open,
    }
}

/// Webhook handler for Sentry.
pub struct SentryWebhookHandler {
    config: SentryConfig,
}

impl SentryWebhookHandler {
    /// Create a new Sentry webhook handler.
    pub fn new(config: SentryConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl WebhookHandler for SentryWebhookHandler {
    fn source_name(&self) -> &str {
        "sentry"
    }

    fn verify_signature(&self, payload: &[u8], headers: &HashMap<String, String>) -> bool {
        let secret = match &self.config.client_secret {
            Some(s) => s,
            None => {
                tracing::error!(
                    source = "sentry",
                    "No client secret configured - rejecting request for security"
                );
                return false;
            }
        };

        let signature = match headers.get("sentry-hook-signature") {
            Some(s) => s,
            None => return false,
        };

        let mut mac = match Hmac::<Sha256>::new_from_slice(secret.expose().as_bytes()) {
            Ok(m) => m,
            Err(_) => return false,
        };

        mac.update(payload);
        let expected = mac.finalize().into_bytes();
        let expected_hex = hex::encode(expected);

        signature.as_bytes().ct_eq(expected_hex.as_bytes()).into()
    }

    async fn parse_payload(&self, payload: &serde_json::Value) -> Result<Option<Issue>> {
        // Only process issue events with created action
        let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("");

        if action != "created" {
            return Ok(None);
        }

        let issue_data = match payload.get("data").and_then(|d| d.get("issue")) {
            Some(i) => i,
            None => return Ok(None),
        };

        let project = match issue_data.get("project") {
            Some(p) => p,
            None => return Ok(None),
        };

        let project_slug = project
            .get("slug")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        // Check project filter
        if !self.config.project_slugs.is_empty()
            && !self
                .config
                .project_slugs
                .contains(&project_slug.to_string())
        {
            return Ok(None);
        }

        let id = issue_data
            .get("id")
            .map(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_default();

        let short_id = issue_data
            .get("shortId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let title = issue_data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let url = format!(
            "https://sentry.io/organizations/{}/issues/{}/",
            self.config.org_slug, id
        );

        let status = issue_data
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unresolved");

        let level = issue_data
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("error");

        let count = issue_data
            .get("count")
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(1);

        let user_count = issue_data
            .get("userCount")
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(0);

        let culprit = issue_data
            .get("culprit")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let metadata = issue_data.get("metadata");
        let error_type = metadata
            .and_then(|m| m.get("type"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let error_value = metadata
            .and_then(|m| m.get("value"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let filename = metadata
            .and_then(|m| m.get("filename"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let function = metadata
            .and_then(|m| m.get("function"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let project_name = project
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let first_seen = issue_data
            .get("firstSeen")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        let last_seen = issue_data
            .get("lastSeen")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        let mut issue = Issue::new(&id, &short_id, &title, &url, "sentry");
        issue.description = error_value.clone();
        issue.priority = map_priority(level, count);
        issue.status = map_status(status);
        issue.created_at = first_seen;
        issue.updated_at = last_seen;

        // Store metadata
        if let Some(c) = culprit {
            issue.set_metadata("culprit", &c);
        }
        issue.set_metadata("level", level);
        issue.set_metadata("project", project_name);
        issue.set_metadata("project_slug", project_slug);
        issue.set_metadata("event_count", count);
        issue.set_metadata("user_count", user_count);
        if let Some(t) = error_type {
            issue.set_metadata("error_type", &t);
        }
        if let Some(v) = error_value {
            issue.set_metadata("error_value", &v);
        }
        if let Some(f) = filename {
            issue.set_metadata("filename", &f);
        }
        if let Some(f) = function {
            issue.set_metadata("function", &f);
        }

        Ok(Some(issue))
    }

    fn matches_criteria(&self, issue: &Issue) -> MatchResult {
        let event_count: i64 = issue.get_metadata("event_count").unwrap_or(0);
        let level: String = issue.get_metadata("level").unwrap_or_default();

        // Check minimum event count
        if event_count < i64::try_from(self.config.min_event_count).unwrap_or(i64::MAX) {
            return MatchResult::not_matched(format!(
                "Event count {} below threshold {}",
                event_count, self.config.min_event_count
            ));
        }

        // Check if resolved
        if issue.status == IssueStatus::Resolved {
            return MatchResult::not_matched("Issue is already resolved");
        }

        // Determine priority
        let (priority, reason) = if level == "fatal" || event_count > 1000 {
            (
                MatchPriority::Urgent,
                format!("New {} with {} events", level, event_count),
            )
        } else if level == "error" && event_count > 100 {
            (
                MatchPriority::High,
                format!("New {} with {} events", level, event_count),
            )
        } else {
            (
                MatchPriority::Normal,
                format!("New {} with {} events", level, event_count),
            )
        };

        MatchResult::matched(reason, priority)
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        let mut context = format!("# Sentry Issue: {}\n\n", issue.short_id);
        context.push_str(&format!("**Title:** {}\n", issue.title));
        context.push_str(&format!("**URL:** {}\n", issue.url));

        if let Some(level) = issue.get_metadata::<String>("level") {
            context.push_str(&format!("**Level:** {}\n", level));
        }
        context.push_str(&format!("**Status:** {}\n", issue.status));

        if let Some(event_count) = issue.get_metadata::<i64>("event_count") {
            context.push_str(&format!("**Event Count:** {}\n", event_count));
        }
        if let Some(user_count) = issue.get_metadata::<i64>("user_count") {
            context.push_str(&format!("**User Count:** {}\n", user_count));
        }
        if let Some(project) = issue.get_metadata::<String>("project") {
            context.push_str(&format!("**Project:** {}\n\n", project));
        }

        if let Some(culprit) = issue.get_metadata::<String>("culprit") {
            if !culprit.is_empty() {
                context.push_str(&format!("**Culprit:** {}\n\n", culprit));
            }
        }

        // Error details
        let error_type: Option<String> = issue.get_metadata("error_type");
        let error_value: Option<String> = issue.get_metadata("error_value");
        let filename: Option<String> = issue.get_metadata("filename");
        let function: Option<String> = issue.get_metadata("function");

        if error_type.is_some() || error_value.is_some() {
            context.push_str("## Error Details\n");
            if let Some(ref t) = error_type {
                context.push_str(&format!("- **Type:** {}\n", t));
            }
            if let Some(ref v) = error_value {
                context.push_str(&format!("- **Value:** {}\n", v));
            }
            if let Some(ref f) = filename {
                context.push_str(&format!("- **File:** {}\n", f));
            }
            if let Some(ref f) = function {
                context.push_str(&format!("- **Function:** {}\n", f));
            }
            context.push('\n');
        }

        context.push_str("\n**Note:** This is from a webhook event. For full stack trace, check the Sentry dashboard.\n");

        Ok(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_config::config::TopIssuesPeriod;

    fn test_config() -> SentryConfig {
        SentryConfig {
            enabled: true,
            auth_token: "test".into(),
            org_slug: "test-org".to_string(),
            project_slugs: vec![],
            top_issues_count: 100,
            top_issues_period: TopIssuesPeriod::OneDay,
            min_event_count: 10,
            escalation_threshold_percent: 50,
            client_secret: Some("test_secret".into()),
            ..Default::default()
        }
    }

    #[test]
    fn test_verify_signature() {
        let handler = SentryWebhookHandler::new(test_config());
        let payload = b"test payload";

        let mut mac = Hmac::<Sha256>::new_from_slice(b"test_secret").unwrap();
        mac.update(payload);
        let expected = hex::encode(mac.finalize().into_bytes());

        let mut headers = HashMap::new();
        headers.insert("sentry-hook-signature".to_string(), expected);

        assert!(handler.verify_signature(payload, &headers));
    }

    #[tokio::test]
    async fn test_parse_payload_issue_created() {
        let handler = SentryWebhookHandler::new(test_config());

        let payload = serde_json::json!({
            "action": "created",
            "data": {
                "issue": {
                    "id": "123",
                    "shortId": "PROJ-ABC",
                    "title": "TypeError: Cannot read property",
                    "status": "unresolved",
                    "level": "error",
                    "count": 42,
                    "userCount": 5,
                    "firstSeen": "2024-01-01T00:00:00Z",
                    "lastSeen": "2024-01-01T01:00:00Z",
                    "project": {
                        "id": "1",
                        "name": "My Project",
                        "slug": "my-project"
                    },
                    "metadata": {
                        "type": "TypeError",
                        "value": "Cannot read property 'x' of undefined"
                    }
                }
            }
        });

        let issue = handler.parse_payload(&payload).await.unwrap().unwrap();
        assert_eq!(issue.id, "123");
        assert_eq!(issue.short_id, "PROJ-ABC");
        assert_eq!(issue.priority, IssuePriority::High);
    }

    #[tokio::test]
    async fn test_parse_payload_non_created() {
        let handler = SentryWebhookHandler::new(test_config());

        let payload = serde_json::json!({
            "action": "resolved",
            "data": {}
        });

        let result = handler.parse_payload(&payload).await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_matches_criteria_min_event_count() {
        let handler = SentryWebhookHandler::new(test_config());

        let mut issue = Issue::new("123", "PROJ-123", "Error", "https://sentry.io", "sentry");
        issue.set_metadata("event_count", 5i64);
        issue.set_metadata("level", "error");

        let result = handler.matches_criteria(&issue);
        assert!(!result.matches);
    }

    #[test]
    fn test_matches_criteria_success() {
        let handler = SentryWebhookHandler::new(test_config());

        let mut issue = Issue::new("123", "PROJ-123", "Error", "https://sentry.io", "sentry");
        issue.set_metadata("event_count", 100i64);
        issue.set_metadata("level", "error");

        let result = handler.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_source_name() {
        let handler = SentryWebhookHandler::new(test_config());
        assert_eq!(handler.source_name(), "sentry");
    }

    #[test]
    fn test_verify_signature_missing_header() {
        let handler = SentryWebhookHandler::new(test_config());
        let payload = b"test";
        let headers = HashMap::new();
        assert!(!handler.verify_signature(payload, &headers));
    }

    #[test]
    fn test_verify_signature_no_secret() {
        let mut config = test_config();
        config.client_secret = None;
        let handler = SentryWebhookHandler::new(config);
        let payload = b"test";
        let headers = HashMap::new();
        // Should return false when no secret configured (fail closed for security)
        assert!(!handler.verify_signature(payload, &headers));
    }

    #[test]
    fn test_verify_signature_invalid() {
        let handler = SentryWebhookHandler::new(test_config());
        let payload = b"test payload";
        let mut headers = HashMap::new();
        headers.insert(
            "sentry-hook-signature".to_string(),
            "invalid_signature".to_string(),
        );
        assert!(!handler.verify_signature(payload, &headers));
    }

    #[test]
    fn test_map_priority_fatal() {
        let priority = map_priority("fatal", 1);
        assert_eq!(priority, IssuePriority::Critical);
    }

    #[test]
    fn test_map_priority_error_high_count() {
        let priority = map_priority("error", 1001);
        assert_eq!(priority, IssuePriority::Critical);
    }

    #[test]
    fn test_map_priority_error_normal() {
        let priority = map_priority("error", 100);
        assert_eq!(priority, IssuePriority::High);
    }

    #[test]
    fn test_map_priority_warning() {
        let priority = map_priority("warning", 50);
        assert_eq!(priority, IssuePriority::Medium);
    }

    #[test]
    fn test_map_priority_info() {
        let priority = map_priority("info", 10);
        assert_eq!(priority, IssuePriority::Low);
    }

    #[test]
    fn test_map_status_resolved() {
        let status = map_status("resolved");
        assert_eq!(status, IssueStatus::Resolved);
    }

    #[test]
    fn test_map_status_ignored() {
        let status = map_status("ignored");
        assert_eq!(status, IssueStatus::Ignored);
    }

    #[test]
    fn test_map_status_unresolved() {
        let status = map_status("unresolved");
        assert_eq!(status, IssueStatus::Open);
    }

    #[tokio::test]
    async fn test_parse_payload_missing_data() {
        let handler = SentryWebhookHandler::new(test_config());
        let payload = serde_json::json!({
            "action": "created"
        });
        let result = handler.parse_payload(&payload).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_missing_issue() {
        let handler = SentryWebhookHandler::new(test_config());
        let payload = serde_json::json!({
            "action": "created",
            "data": {}
        });
        let result = handler.parse_payload(&payload).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_missing_project() {
        let handler = SentryWebhookHandler::new(test_config());
        let payload = serde_json::json!({
            "action": "created",
            "data": {
                "issue": {
                    "id": "123"
                }
            }
        });
        let result = handler.parse_payload(&payload).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_project_filter() {
        let mut config = test_config();
        config.project_slugs = vec!["allowed-project".to_string()];
        let handler = SentryWebhookHandler::new(config);

        let payload = serde_json::json!({
            "action": "created",
            "data": {
                "issue": {
                    "id": "123",
                    "shortId": "PROJ-123",
                    "title": "Error",
                    "status": "unresolved",
                    "level": "error",
                    "count": 10,
                    "project": {
                        "slug": "disallowed-project",
                        "name": "Disallowed Project"
                    }
                }
            }
        });

        let result = handler.parse_payload(&payload).await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_matches_criteria_resolved_issue() {
        let handler = SentryWebhookHandler::new(test_config());

        let mut issue = Issue::new("123", "PROJ-123", "Error", "https://sentry.io", "sentry");
        issue.set_metadata("event_count", 100i64);
        issue.set_metadata("level", "error");
        issue.status = IssueStatus::Resolved;

        let result = handler.matches_criteria(&issue);
        assert!(!result.matches);
    }

    #[test]
    fn test_matches_criteria_urgent_priority() {
        let handler = SentryWebhookHandler::new(test_config());

        let mut issue = Issue::new("123", "PROJ-123", "Error", "https://sentry.io", "sentry");
        issue.set_metadata("event_count", 1500i64);
        issue.set_metadata("level", "error");

        let result = handler.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Urgent);
    }

    #[test]
    fn test_matches_criteria_high_priority() {
        let handler = SentryWebhookHandler::new(test_config());

        let mut issue = Issue::new("123", "PROJ-123", "Error", "https://sentry.io", "sentry");
        issue.set_metadata("event_count", 150i64);
        issue.set_metadata("level", "error");

        let result = handler.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::High);
    }

    #[tokio::test]
    async fn test_build_issue_context() {
        let handler = SentryWebhookHandler::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "TypeError",
            "https://sentry.io/123",
            "sentry",
        );
        issue.set_metadata("level", "error");
        issue.set_metadata("project", "my-project");
        issue.set_metadata("event_count", 42i64);
        issue.set_metadata("user_count", 5i64);
        issue.set_metadata("culprit", "src/main.js in function");
        issue.set_metadata("error_type", "TypeError");
        issue.set_metadata("error_value", "Cannot read property");
        issue.set_metadata("filename", "src/main.js");
        issue.set_metadata("function", "handleClick");

        let context = handler.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("PROJ-123"));
        assert!(context.contains("TypeError"));
        assert!(context.contains("error"));
        assert!(context.contains("my-project"));
        assert!(context.contains("42"));
        assert!(context.contains("src/main.js"));
        assert!(context.contains("handleClick"));
    }

    #[tokio::test]
    async fn test_parse_payload_with_full_metadata() {
        let handler = SentryWebhookHandler::new(test_config());

        let payload = serde_json::json!({
            "action": "created",
            "data": {
                "issue": {
                    "id": "123",
                    "shortId": "PROJ-ABC",
                    "title": "TypeError in handler",
                    "status": "unresolved",
                    "level": "error",
                    "count": 42,
                    "userCount": 10,
                    "culprit": "src/handler.ts",
                    "firstSeen": "2024-01-01T00:00:00Z",
                    "lastSeen": "2024-01-01T01:00:00Z",
                    "project": {
                        "id": "1",
                        "name": "My Project",
                        "slug": "my-project"
                    },
                    "metadata": {
                        "type": "TypeError",
                        "value": "undefined is not a function",
                        "filename": "src/handler.ts",
                        "function": "processRequest"
                    }
                }
            }
        });

        let issue = handler.parse_payload(&payload).await.unwrap().unwrap();
        assert_eq!(issue.id, "123");
        assert_eq!(issue.short_id, "PROJ-ABC");
        assert_eq!(issue.priority, IssuePriority::High);

        let event_count: i64 = issue.get_metadata("event_count").unwrap();
        assert_eq!(event_count, 42);

        let culprit: String = issue.get_metadata("culprit").unwrap();
        assert_eq!(culprit, "src/handler.ts");
    }
}
