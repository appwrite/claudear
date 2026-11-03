//! Linear webhook handler.

use super::WebhookHandler;
use async_trait::async_trait;
use claudear_config::config::LinearConfig;
use claudear_core::error::Result;
use claudear_core::types::{Issue, IssuePriority, IssueStatus, MatchPriority, MatchResult};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use subtle::ConstantTimeEq;

/// Webhook handler for Linear.
pub struct LinearWebhookHandler {
    config: LinearConfig,
}

impl LinearWebhookHandler {
    /// Create a new Linear webhook handler.
    pub fn new(config: LinearConfig) -> Self {
        Self { config }
    }

    fn map_priority(priority: Option<i64>) -> IssuePriority {
        match priority {
            Some(1) => IssuePriority::Critical,
            Some(2) => IssuePriority::High,
            Some(3) => IssuePriority::Medium,
            Some(4) => IssuePriority::Low,
            _ => IssuePriority::None,
        }
    }

    fn map_status(state_type: Option<&str>) -> IssueStatus {
        match state_type {
            Some("completed") | Some("canceled") => IssueStatus::Resolved,
            Some("started") => IssueStatus::InProgress,
            _ => IssueStatus::Open,
        }
    }
}

#[async_trait]
impl WebhookHandler for LinearWebhookHandler {
    fn source_name(&self) -> &str {
        "linear"
    }

    fn verify_signature(&self, payload: &[u8], headers: &HashMap<String, String>) -> bool {
        let secret = match &self.config.webhook_secret {
            Some(s) => s,
            None => {
                tracing::error!(
                    source = "linear",
                    "No webhook secret configured - rejecting request for security"
                );
                return false;
            }
        };

        let signature = match headers.get("linear-signature") {
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
        // Only process Issue events
        let event_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if event_type != "Issue" {
            return Ok(None);
        }

        // Only process create and update actions
        let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("");

        if action != "create" && action != "update" {
            return Ok(None);
        }

        let data = match payload.get("data") {
            Some(d) => d,
            None => return Ok(None),
        };

        let id = data
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let identifier = data
            .get("identifier")
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();

        let title = data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled")
            .to_string();

        let url = data
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("https://linear.app/issue/{}", identifier));

        let description = data
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let priority = data.get("priority").and_then(|v| v.as_i64());

        let state = data.get("state");
        let state_name = state.and_then(|s| s.get("name")).and_then(|v| v.as_str());
        let state_type = state.and_then(|s| s.get("type")).and_then(|v| v.as_str());

        let labels: Vec<String> = data
            .get("labels")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| {
                        l.get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();

        let team = data.get("team");
        let team_id = team
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let team_name = team
            .and_then(|t| t.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let project = data.get("project");
        let project_id = project
            .and_then(|p| p.get("id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let project_name = project
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let assignee_name = data
            .get("assignee")
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mut issue = Issue::new(&id, &identifier, &title, &url, "linear");
        issue.description = description;
        issue.priority = Self::map_priority(priority);
        issue.status = Self::map_status(state_type);

        // Store metadata
        if let Some(name) = state_name {
            issue.set_metadata("state_name", name);
        }
        if let Some(st) = state_type {
            issue.set_metadata("state_type", st);
        }
        issue.set_metadata("labels", &labels);
        if let Some(id) = team_id {
            issue.set_metadata("team_id", &id);
        }
        if let Some(name) = team_name {
            issue.set_metadata("team", &name);
        }
        if let Some(id) = project_id {
            issue.set_metadata("project_id", &id);
        }
        if let Some(name) = project_name {
            issue.set_metadata("project", &name);
        }
        if let Some(name) = assignee_name {
            issue.set_metadata("assignee", &name);
        }

        Ok(Some(issue))
    }

    fn matches_criteria(&self, issue: &Issue) -> MatchResult {
        let state_name: Option<String> = issue.get_metadata("state_name");
        let state_type: Option<String> = issue.get_metadata("state_type");
        let labels: Vec<String> = issue.get_metadata("labels").unwrap_or_default();
        let team_id: Option<String> = issue.get_metadata("team_id");
        let project_id: Option<String> = issue.get_metadata("project_id");

        // Check team filter
        if let Some(ref filter_team) = self.config.team_id {
            if team_id.as_ref() != Some(filter_team) {
                return MatchResult::not_matched("Team does not match filter");
            }
        }

        // Check project filter
        if let Some(ref filter_project) = self.config.project_id {
            if project_id.as_ref() != Some(filter_project) {
                return MatchResult::not_matched("Project does not match filter");
            }
        }

        // Check state
        if !self.config.trigger_states.is_empty() {
            let state_name_lower = state_name.as_deref().unwrap_or("").to_lowercase();
            let state_type_lower = state_type.as_deref().unwrap_or("").to_lowercase();

            let state_matches = self.config.trigger_states.iter().any(|s| {
                let s_lower = s.to_lowercase();
                state_name_lower.contains(&s_lower) || state_type_lower.contains(&s_lower)
            });

            if !state_matches {
                return MatchResult::not_matched(format!(
                    "State \"{}\" not in trigger states",
                    state_name.as_deref().unwrap_or("unknown")
                ));
            }
        }

        // Check assignee
        if let Some(ref trigger_assignee) = self.config.trigger_assignee {
            let issue_assignee: Option<String> = issue.get_metadata("assignee");
            let assignee_matches = issue_assignee
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(trigger_assignee));

            if !assignee_matches {
                return MatchResult::not_matched(format!(
                    "Assignee \"{}\" does not match trigger assignee \"{}\"",
                    issue_assignee.as_deref().unwrap_or("unassigned"),
                    trigger_assignee
                ));
            }
        }

        // Check labels (skip if trigger_assignee is set and trigger_labels is empty)
        let skip_label_check =
            self.config.trigger_assignee.is_some() && self.config.trigger_labels.is_empty();
        if !skip_label_check && !self.config.trigger_labels.is_empty() {
            let label_matches = self.config.trigger_labels.iter().any(|trigger| {
                labels
                    .iter()
                    .any(|l| l.to_lowercase() == trigger.to_lowercase())
            });

            if !label_matches {
                return MatchResult::not_matched("No matching trigger labels");
            }
        }

        let priority = match issue.priority {
            IssuePriority::Critical => MatchPriority::Urgent,
            IssuePriority::High => MatchPriority::High,
            IssuePriority::Low => MatchPriority::Low,
            _ => MatchPriority::Normal,
        };

        MatchResult::matched("Matches state and label criteria", priority)
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        let mut context = format!("# Linear Issue: {}\n\n", issue.short_id);
        context.push_str(&format!("**Title:** {}\n", issue.title));
        context.push_str(&format!("**URL:** {}\n", issue.url));
        context.push_str(&format!("**Priority:** {}\n", issue.priority));
        context.push_str(&format!("**Status:** {}\n\n", issue.status));

        if let Some(ref description) = issue.description {
            context.push_str(&format!("## Description\n{}\n\n", description));
        }

        if let Some(team) = issue.get_metadata::<String>("team") {
            context.push_str(&format!("**Team:** {}\n", team));
        }
        if let Some(project) = issue.get_metadata::<String>("project") {
            context.push_str(&format!("**Project:** {}\n", project));
        }
        if let Some(assignee) = issue.get_metadata::<String>("assignee") {
            context.push_str(&format!("**Assignee:** {}\n", assignee));
        }

        Ok(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> LinearConfig {
        LinearConfig {
            enabled: true,
            api_key: "test".into(),
            trigger_labels: vec!["auto-implement".to_string()],
            trigger_states: vec!["backlog".to_string(), "todo".to_string()],
            team_id: None,
            project_id: None,
            webhook_secret: Some("test_secret".into()),
            ..Default::default()
        }
    }

    #[test]
    fn test_verify_signature_with_secret() {
        let handler = LinearWebhookHandler::new(test_config());
        let payload = b"test payload";

        // Calculate expected signature
        let mut mac = Hmac::<Sha256>::new_from_slice(b"test_secret").unwrap();
        mac.update(payload);
        let expected = hex::encode(mac.finalize().into_bytes());

        let mut headers = HashMap::new();
        headers.insert("linear-signature".to_string(), expected);

        assert!(handler.verify_signature(payload, &headers));
    }

    #[test]
    fn test_verify_signature_no_header() {
        let handler = LinearWebhookHandler::new(test_config());
        let headers = HashMap::new();
        assert!(!handler.verify_signature(b"test", &headers));
    }

    #[test]
    fn test_verify_signature_no_secret() {
        let mut config = test_config();
        config.webhook_secret = None;
        let handler = LinearWebhookHandler::new(config);

        let headers = HashMap::new();
        // Should return false when no secret configured (fail closed for security)
        assert!(!handler.verify_signature(b"test", &headers));
    }

    #[tokio::test]
    async fn test_parse_payload_issue_create() {
        let handler = LinearWebhookHandler::new(test_config());

        let payload = serde_json::json!({
            "type": "Issue",
            "action": "create",
            "data": {
                "id": "123",
                "identifier": "PROJ-123",
                "title": "Fix bug",
                "url": "https://linear.app/team/issue/PROJ-123",
                "priority": 2,
                "state": {
                    "name": "Backlog",
                    "type": "backlog"
                },
                "labels": [
                    { "name": "auto-implement" }
                ],
                "team": {
                    "id": "team-1",
                    "name": "Engineering"
                }
            }
        });

        let issue = handler.parse_payload(&payload).await.unwrap().unwrap();
        assert_eq!(issue.id, "123");
        assert_eq!(issue.short_id, "PROJ-123");
        assert_eq!(issue.title, "Fix bug");
        assert_eq!(issue.priority, IssuePriority::High);
    }

    #[tokio::test]
    async fn test_parse_payload_non_issue() {
        let handler = LinearWebhookHandler::new(test_config());

        let payload = serde_json::json!({
            "type": "Comment",
            "action": "create",
            "data": {}
        });

        let result = handler.parse_payload(&payload).await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_source_name() {
        let handler = LinearWebhookHandler::new(test_config());
        assert_eq!(handler.source_name(), "linear");
    }

    #[test]
    fn test_map_priority() {
        assert_eq!(
            LinearWebhookHandler::map_priority(Some(1)),
            IssuePriority::Critical
        );
        assert_eq!(
            LinearWebhookHandler::map_priority(Some(2)),
            IssuePriority::High
        );
        assert_eq!(
            LinearWebhookHandler::map_priority(Some(3)),
            IssuePriority::Medium
        );
        assert_eq!(
            LinearWebhookHandler::map_priority(Some(4)),
            IssuePriority::Low
        );
        assert_eq!(
            LinearWebhookHandler::map_priority(Some(0)),
            IssuePriority::None
        );
        assert_eq!(
            LinearWebhookHandler::map_priority(Some(5)),
            IssuePriority::None
        );
        assert_eq!(
            LinearWebhookHandler::map_priority(None),
            IssuePriority::None
        );
    }

    #[test]
    fn test_map_status() {
        assert_eq!(
            LinearWebhookHandler::map_status(Some("completed")),
            IssueStatus::Resolved
        );
        assert_eq!(
            LinearWebhookHandler::map_status(Some("canceled")),
            IssueStatus::Resolved
        );
        assert_eq!(
            LinearWebhookHandler::map_status(Some("started")),
            IssueStatus::InProgress
        );
        assert_eq!(
            LinearWebhookHandler::map_status(Some("backlog")),
            IssueStatus::Open
        );
        assert_eq!(
            LinearWebhookHandler::map_status(Some("triage")),
            IssueStatus::Open
        );
        assert_eq!(LinearWebhookHandler::map_status(None), IssueStatus::Open);
    }

    #[test]
    fn test_matches_criteria_with_labels() {
        let handler = LinearWebhookHandler::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.set_metadata("state_type", "backlog");

        let result = handler.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_no_matching_labels() {
        let handler = LinearWebhookHandler::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["other-label".to_string()]);
        issue.set_metadata("state_type", "backlog");

        let result = handler.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("label"));
    }

    #[test]
    fn test_matches_criteria_with_team_filter() {
        let mut config = test_config();
        config.team_id = Some("team-1".to_string());
        let handler = LinearWebhookHandler::new(config);

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("team_id", "team-1");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.set_metadata("state_type", "backlog");

        let result = handler.matches_criteria(&issue);
        assert!(result.matches);

        // Different team should not match
        issue.set_metadata("team_id", "team-2");
        let result = handler.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("Team"));
    }

    #[test]
    fn test_matches_criteria_with_project_filter() {
        let mut config = test_config();
        config.project_id = Some("project-1".to_string());
        let handler = LinearWebhookHandler::new(config);

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("project_id", "project-1");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.set_metadata("state_type", "backlog");

        let result = handler.matches_criteria(&issue);
        assert!(result.matches);

        // Different project should not match
        issue.set_metadata("project_id", "project-2");
        let result = handler.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("Project"));
    }

    #[test]
    fn test_matches_criteria_state_not_matching() {
        let handler = LinearWebhookHandler::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.set_metadata("state_name", "In Progress");
        issue.set_metadata("state_type", "started");

        let result = handler.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("State"));
    }

    #[test]
    fn test_matches_criteria_priority_mapping() {
        let handler = LinearWebhookHandler::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.set_metadata("state_type", "backlog");

        issue.priority = IssuePriority::Critical;
        let result = handler.matches_criteria(&issue);
        assert_eq!(result.priority, MatchPriority::Urgent);

        issue.priority = IssuePriority::High;
        let result = handler.matches_criteria(&issue);
        assert_eq!(result.priority, MatchPriority::High);

        issue.priority = IssuePriority::Medium;
        let result = handler.matches_criteria(&issue);
        assert_eq!(result.priority, MatchPriority::Normal);

        issue.priority = IssuePriority::Low;
        let result = handler.matches_criteria(&issue);
        assert_eq!(result.priority, MatchPriority::Low);
    }

    #[tokio::test]
    async fn test_parse_payload_update_action() {
        let handler = LinearWebhookHandler::new(test_config());

        let payload = serde_json::json!({
            "type": "Issue",
            "action": "update",
            "data": {
                "id": "456",
                "identifier": "PROJ-456",
                "title": "Updated bug",
                "url": "https://linear.app/team/issue/PROJ-456",
                "priority": 1
            }
        });

        let issue = handler.parse_payload(&payload).await.unwrap().unwrap();
        assert_eq!(issue.id, "456");
        assert_eq!(issue.short_id, "PROJ-456");
        assert_eq!(issue.priority, IssuePriority::Critical);
    }

    #[tokio::test]
    async fn test_parse_payload_delete_action() {
        let handler = LinearWebhookHandler::new(test_config());

        let payload = serde_json::json!({
            "type": "Issue",
            "action": "delete",
            "data": { "id": "789" }
        });

        let result = handler.parse_payload(&payload).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_minimal() {
        let handler = LinearWebhookHandler::new(test_config());

        let payload = serde_json::json!({
            "type": "Issue",
            "action": "create",
            "data": {
                "id": "minimal-123"
            }
        });

        let issue = handler.parse_payload(&payload).await.unwrap().unwrap();
        assert_eq!(issue.id, "minimal-123");
        assert_eq!(issue.short_id, "minimal-123"); // Falls back to id
        assert_eq!(issue.title, "Untitled");
    }

    #[tokio::test]
    async fn test_parse_payload_with_all_fields() {
        let handler = LinearWebhookHandler::new(test_config());

        let payload = serde_json::json!({
            "type": "Issue",
            "action": "create",
            "data": {
                "id": "full-123",
                "identifier": "PROJ-999",
                "title": "Complete issue",
                "url": "https://linear.app/team/issue/PROJ-999",
                "description": "Full description here",
                "priority": 3,
                "state": {
                    "name": "Todo",
                    "type": "unstarted"
                },
                "labels": [
                    { "name": "auto-implement" },
                    { "name": "priority" }
                ],
                "team": {
                    "id": "team-abc",
                    "name": "Platform Team"
                },
                "project": {
                    "id": "proj-xyz",
                    "name": "Q1 Goals"
                },
                "assignee": {
                    "name": "John Doe"
                }
            }
        });

        let issue = handler.parse_payload(&payload).await.unwrap().unwrap();
        assert_eq!(issue.id, "full-123");
        assert_eq!(issue.short_id, "PROJ-999");
        assert_eq!(issue.title, "Complete issue");
        assert_eq!(issue.description, Some("Full description here".to_string()));
        assert_eq!(issue.priority, IssuePriority::Medium);

        let labels: Vec<String> = issue.get_metadata("labels").unwrap();
        assert_eq!(labels.len(), 2);
        assert!(labels.contains(&"auto-implement".to_string()));

        let team: String = issue.get_metadata("team").unwrap();
        assert_eq!(team, "Platform Team");

        let assignee: String = issue.get_metadata("assignee").unwrap();
        assert_eq!(assignee, "John Doe");
    }

    #[tokio::test]
    async fn test_build_issue_context() {
        let handler = LinearWebhookHandler::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://linear.app/123",
            "linear",
        );
        issue.description = Some("Test description".to_string());
        issue.set_metadata("team", "Engineering");
        issue.set_metadata("project", "Q1 Goals");
        issue.set_metadata("assignee", "Jane Doe");

        let context = handler.build_issue_context(&issue).await.unwrap();

        assert!(context.contains("PROJ-123"));
        assert!(context.contains("Test Issue"));
        assert!(context.contains("Test description"));
        assert!(context.contains("Engineering"));
        assert!(context.contains("Q1 Goals"));
        assert!(context.contains("Jane Doe"));
    }

    #[test]
    fn test_verify_signature_invalid() {
        let handler = LinearWebhookHandler::new(test_config());

        let mut headers = HashMap::new();
        headers.insert(
            "linear-signature".to_string(),
            "invalid_signature".to_string(),
        );

        assert!(!handler.verify_signature(b"test payload", &headers));
    }

    #[test]
    fn test_matches_criteria_empty_filters() {
        let mut config = test_config();
        config.trigger_labels = vec![];
        config.trigger_states = vec![];
        let handler = LinearWebhookHandler::new(config);

        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );

        let result = handler.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_parse_payload_no_data() {
        let handler = LinearWebhookHandler::new(test_config());

        let payload = serde_json::json!({
            "type": "Issue",
            "action": "create"
            // Missing data field
        });

        let result = handler.parse_payload(&payload).await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_matches_criteria_trigger_assignee_no_labels() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test".into(),
            trigger_labels: vec![],
            trigger_assignee: Some("Jane Smith".to_string()),
            trigger_states: vec!["backlog".to_string()],
            webhook_secret: Some("test_secret".into()),
            ..Default::default()
        };
        let handler = LinearWebhookHandler::new(config);

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("assignee", "Jane Smith");
        issue.set_metadata("state_type", "backlog");

        let result = handler.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_trigger_assignee_wrong_assignee() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test".into(),
            trigger_labels: vec![],
            trigger_assignee: Some("Jane Smith".to_string()),
            trigger_states: vec!["backlog".to_string()],
            webhook_secret: Some("test_secret".into()),
            ..Default::default()
        };
        let handler = LinearWebhookHandler::new(config);

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("assignee", "John Doe");
        issue.set_metadata("state_type", "backlog");

        let result = handler.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("Assignee"));
    }

    #[test]
    fn test_matches_criteria_trigger_assignee_and_labels_both_required() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test".into(),
            trigger_labels: vec!["auto-implement".to_string()],
            trigger_assignee: Some("Jane Smith".to_string()),
            trigger_states: vec!["backlog".to_string()],
            webhook_secret: Some("test_secret".into()),
            ..Default::default()
        };
        let handler = LinearWebhookHandler::new(config);

        // Both match
        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("assignee", "Jane Smith");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.set_metadata("state_type", "backlog");

        let result = handler.matches_criteria(&issue);
        assert!(result.matches);

        // Right assignee, wrong label
        issue.set_metadata("labels", vec!["other".to_string()]);
        let result = handler.matches_criteria(&issue);
        assert!(!result.matches);
    }
}
