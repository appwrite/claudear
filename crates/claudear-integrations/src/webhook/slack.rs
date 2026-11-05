//! Slack Events API webhook handler.

use super::WebhookHandler;
use async_trait::async_trait;
use chrono::Utc;
use claudear_config::config::SlackSourceConfig;
use claudear_core::error::Result;
use claudear_core::secret::OptionalSecretExt;
use claudear_core::types::{Issue, MatchPriority, MatchResult};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use subtle::ConstantTimeEq;

/// Slack webhook handler for message events.
pub struct SlackWebhookHandler {
    config: SlackSourceConfig,
}

impl SlackWebhookHandler {
    /// Create a new Slack webhook handler.
    pub fn new(config: SlackSourceConfig) -> Self {
        Self { config }
    }

    fn listen_channel_id(&self) -> Option<&str> {
        self.config
            .listen_channel_id
            .as_deref()
            .or(self.config.channel_id.as_deref())
    }

    fn message_url(&self, channel_id: &str, ts: &str) -> String {
        let ts_nodot = ts.replace('.', "");
        match &self.config.workspace {
            Some(workspace) => format!(
                "https://{}.slack.com/archives/{}/p{}",
                workspace, channel_id, ts_nodot
            ),
            None => format!("https://slack.com/archives/{}/p{}", channel_id, ts_nodot),
        }
    }

    fn extract_title(content: &str) -> String {
        let first_line = content.lines().next().unwrap_or(content);
        if first_line.len() > 100 {
            format!("{}...", &first_line[..first_line.floor_char_boundary(97)])
        } else {
            first_line.to_string()
        }
    }
}

#[async_trait]
impl WebhookHandler for SlackWebhookHandler {
    fn source_name(&self) -> &str {
        "slack"
    }

    fn verify_signature(&self, payload: &[u8], headers: &HashMap<String, String>) -> bool {
        let secret = match self.config.signing_secret.expose_as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                tracing::error!(
                    source = "slack",
                    "No Slack signing_secret configured - rejecting request"
                );
                return false;
            }
        };

        let timestamp = match headers.get("x-slack-request-timestamp") {
            Some(v) => v,
            None => return false,
        };
        let signature = match headers.get("x-slack-signature") {
            Some(v) => v,
            None => return false,
        };

        let ts = match timestamp.parse::<i64>() {
            Ok(v) => v,
            Err(_) => return false,
        };
        let now = Utc::now().timestamp();
        if (now - ts).abs() > 60 * 5 {
            tracing::warn!(
                source = "slack",
                "Slack webhook timestamp outside replay window"
            );
            return false;
        }

        let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
            Ok(m) => m,
            Err(_) => return false,
        };
        let base = format!("v0:{}:", timestamp);
        mac.update(base.as_bytes());
        mac.update(payload);
        let expected = format!("v0={}", hex::encode(mac.finalize().into_bytes()));

        signature.as_bytes().ct_eq(expected.as_bytes()).into()
    }

    async fn parse_payload(&self, payload: &serde_json::Value) -> Result<Option<Issue>> {
        let payload_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if payload_type == "url_verification" {
            return Ok(None);
        }
        if payload_type != "event_callback" {
            return Ok(None);
        }

        let event = match payload.get("event") {
            Some(v) => v,
            None => return Ok(None),
        };

        if event.get("type").and_then(|v| v.as_str()) != Some("message") {
            return Ok(None);
        }

        if event.get("bot_id").is_some() {
            return Ok(None);
        }
        if event.get("subtype").is_some() {
            return Ok(None);
        }

        let channel_id = match event.get("channel").and_then(|v| v.as_str()) {
            Some(v) if !v.is_empty() => v,
            _ => return Ok(None),
        };
        if let Some(expected_channel) = self.listen_channel_id() {
            if channel_id != expected_channel {
                return Ok(None);
            }
        }

        let text = match event.get("text").and_then(|v| v.as_str()) {
            Some(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => return Ok(None),
        };
        let ts = match event.get("ts").and_then(|v| v.as_str()) {
            Some(v) if !v.is_empty() => v,
            _ => return Ok(None),
        };

        let short_id = format!("SLACK-{}", ts.chars().take(8).collect::<String>());
        let title = Self::extract_title(&text);
        let url = self.message_url(channel_id, ts);
        let mut issue = Issue::new(ts, short_id, title, url, "slack");
        issue.description = Some(text);
        issue.set_metadata("channel_id", channel_id);
        issue.set_metadata("message_ts", ts);

        if let Some(user_id) = event.get("user").and_then(|v| v.as_str()) {
            issue.set_metadata("author_id", user_id);
        }

        Ok(Some(issue))
    }

    fn matches_criteria(&self, _issue: &Issue) -> MatchResult {
        MatchResult::matched("slack_message", MatchPriority::Normal)
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        let mut context = format!("Slack Message Issue: {}\n", issue.title);
        if let Some(desc) = &issue.description {
            context.push_str("\nMessage:\n");
            context.push_str(desc);
            context.push('\n');
        }
        if let Some(author_id) = issue.get_metadata::<String>("author_id") {
            context.push_str(&format!("\nAuthor ID: {}\n", author_id));
        }
        if let Some(channel_id) = issue.get_metadata::<String>("channel_id") {
            context.push_str(&format!("Channel: {}\n", channel_id));
        }
        Ok(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_config::config::SlackSourceConfig;
    use claudear_core::secret::SecretValue;

    fn make_handler(
        signing_secret: Option<&str>,
        channel_id: Option<&str>,
        workspace: Option<&str>,
    ) -> SlackWebhookHandler {
        SlackWebhookHandler::new(SlackSourceConfig {
            signing_secret: signing_secret.map(SecretValue::from),
            listen_channel_id: channel_id.map(|s| s.to_string()),
            workspace: workspace.map(|s| s.to_string()),
            ..Default::default()
        })
    }

    #[test]
    fn test_source_name() {
        let handler = make_handler(None, None, None);
        assert_eq!(handler.source_name(), "slack");
    }

    #[test]
    fn test_listen_channel_id_from_listen_field() {
        let handler = make_handler(None, Some("C123"), None);
        assert_eq!(handler.listen_channel_id(), Some("C123"));
    }

    #[test]
    fn test_listen_channel_id_falls_back_to_channel_id() {
        let handler = SlackWebhookHandler::new(SlackSourceConfig {
            channel_id: Some("C456".to_string()),
            ..Default::default()
        });
        assert_eq!(handler.listen_channel_id(), Some("C456"));
    }

    #[test]
    fn test_listen_channel_id_none() {
        let handler = make_handler(None, None, None);
        assert_eq!(handler.listen_channel_id(), None);
    }

    #[test]
    fn test_message_url_with_workspace() {
        let handler = make_handler(None, None, Some("myworkspace"));
        let url = handler.message_url("C123", "1234567890.123456");
        assert_eq!(
            url,
            "https://myworkspace.slack.com/archives/C123/p1234567890123456"
        );
    }

    #[test]
    fn test_message_url_without_workspace() {
        let handler = make_handler(None, None, None);
        let url = handler.message_url("C123", "1234567890.123456");
        assert_eq!(url, "https://slack.com/archives/C123/p1234567890123456");
    }

    #[test]
    fn test_extract_title_short() {
        assert_eq!(
            SlackWebhookHandler::extract_title("Short title"),
            "Short title"
        );
    }

    #[test]
    fn test_extract_title_multiline() {
        assert_eq!(
            SlackWebhookHandler::extract_title("First line\nSecond line"),
            "First line"
        );
    }

    #[test]
    fn test_extract_title_long_truncates() {
        let long = "a".repeat(200);
        let title = SlackWebhookHandler::extract_title(&long);
        assert!(title.len() <= 103);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn test_verify_signature_no_secret() {
        let handler = make_handler(None, None, None);
        assert!(!handler.verify_signature(b"payload", &HashMap::new()));
    }

    #[test]
    fn test_verify_signature_missing_timestamp() {
        let handler = make_handler(Some("secret"), None, None);
        let mut headers = HashMap::new();
        headers.insert("x-slack-signature".to_string(), "v0=abc".to_string());
        assert!(!handler.verify_signature(b"payload", &headers));
    }

    #[test]
    fn test_verify_signature_missing_signature() {
        let handler = make_handler(Some("secret"), None, None);
        let mut headers = HashMap::new();
        headers.insert(
            "x-slack-request-timestamp".to_string(),
            Utc::now().timestamp().to_string(),
        );
        assert!(!handler.verify_signature(b"payload", &headers));
    }

    #[test]
    fn test_verify_signature_invalid_timestamp() {
        let handler = make_handler(Some("secret"), None, None);
        let mut headers = HashMap::new();
        headers.insert(
            "x-slack-request-timestamp".to_string(),
            "not-a-number".to_string(),
        );
        headers.insert("x-slack-signature".to_string(), "v0=abc".to_string());
        assert!(!handler.verify_signature(b"payload", &headers));
    }

    #[test]
    fn test_verify_signature_expired_timestamp() {
        let handler = make_handler(Some("secret"), None, None);
        let mut headers = HashMap::new();
        let old_ts = (Utc::now().timestamp() - 600).to_string(); // 10 min ago
        headers.insert("x-slack-request-timestamp".to_string(), old_ts);
        headers.insert("x-slack-signature".to_string(), "v0=abc".to_string());
        assert!(!handler.verify_signature(b"payload", &headers));
    }

    #[test]
    fn test_verify_signature_valid() {
        let secret = "test_secret";
        let handler = make_handler(Some(secret), None, None);
        let payload = b"test_payload";
        let timestamp = Utc::now().timestamp().to_string();

        // Compute expected signature
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        let base = format!("v0:{}:", timestamp);
        mac.update(base.as_bytes());
        mac.update(payload);
        let expected = format!("v0={}", hex::encode(mac.finalize().into_bytes()));

        let mut headers = HashMap::new();
        headers.insert("x-slack-request-timestamp".to_string(), timestamp);
        headers.insert("x-slack-signature".to_string(), expected);

        assert!(handler.verify_signature(payload, &headers));
    }

    #[test]
    fn test_verify_signature_wrong_signature() {
        let handler = make_handler(Some("secret"), None, None);
        let mut headers = HashMap::new();
        headers.insert(
            "x-slack-request-timestamp".to_string(),
            Utc::now().timestamp().to_string(),
        );
        headers.insert("x-slack-signature".to_string(), "v0=wrong".to_string());
        assert!(!handler.verify_signature(b"payload", &headers));
    }

    #[tokio::test]
    async fn test_parse_payload_url_verification() {
        let handler = make_handler(None, None, None);
        let payload = serde_json::json!({"type": "url_verification", "challenge": "abc"});
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_unknown_type() {
        let handler = make_handler(None, None, None);
        let payload = serde_json::json!({"type": "unknown"});
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_no_event() {
        let handler = make_handler(None, None, None);
        let payload = serde_json::json!({"type": "event_callback"});
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_non_message_event() {
        let handler = make_handler(None, None, None);
        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {"type": "reaction_added"}
        });
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_bot_message() {
        let handler = make_handler(None, None, None);
        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {"type": "message", "bot_id": "B123", "channel": "C123", "text": "hi", "ts": "123"}
        });
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_subtype_message() {
        let handler = make_handler(None, None, None);
        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {"type": "message", "subtype": "channel_join", "channel": "C123", "text": "hi", "ts": "123"}
        });
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_wrong_channel() {
        let handler = make_handler(None, Some("C999"), None);
        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {"type": "message", "channel": "C123", "text": "hello", "ts": "1234567890.000000"}
        });
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_empty_text() {
        let handler = make_handler(None, None, None);
        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {"type": "message", "channel": "C123", "text": "  ", "ts": "1234567890.000000"}
        });
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_missing_ts() {
        let handler = make_handler(None, None, None);
        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {"type": "message", "channel": "C123", "text": "hello"}
        });
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_valid_message() {
        let handler = make_handler(None, None, None);
        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "channel": "C123",
                "text": "Bug: login broken",
                "ts": "1234567890.000000",
                "user": "U456"
            }
        });
        let issue = handler.parse_payload(&payload).await.unwrap().unwrap();
        assert_eq!(issue.source, "slack");
        assert_eq!(issue.title, "Bug: login broken");
        assert!(issue.short_id.starts_with("SLACK-"));
        assert_eq!(issue.get_metadata::<String>("channel_id").unwrap(), "C123");
        assert_eq!(issue.get_metadata::<String>("author_id").unwrap(), "U456");
    }

    #[tokio::test]
    async fn test_parse_payload_valid_message_correct_channel() {
        let handler = make_handler(None, Some("C123"), None);
        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "channel": "C123",
                "text": "Bug report",
                "ts": "1234567890.000000"
            }
        });
        let issue = handler.parse_payload(&payload).await.unwrap().unwrap();
        assert_eq!(issue.source, "slack");
    }

    #[test]
    fn test_matches_criteria() {
        let handler = make_handler(None, None, None);
        let issue = Issue::new("id", "SHORT-1", "title", "url", "slack");
        let result = handler.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_build_issue_context() {
        let handler = make_handler(None, None, None);
        let mut issue = Issue::new("id", "SLACK-1", "Bug title", "url", "slack");
        issue.description = Some("Detailed description".to_string());
        issue.set_metadata("author_id", "U123");
        issue.set_metadata("channel_id", "C456");
        let ctx = handler.build_issue_context(&issue).await.unwrap();
        assert!(ctx.contains("Bug title"));
        assert!(ctx.contains("Detailed description"));
        assert!(ctx.contains("U123"));
        assert!(ctx.contains("C456"));
    }

    #[tokio::test]
    async fn test_build_issue_context_no_metadata() {
        let handler = make_handler(None, None, None);
        let issue = Issue::new("id", "SLACK-1", "Bug title", "url", "slack");
        let ctx = handler.build_issue_context(&issue).await.unwrap();
        assert!(ctx.contains("Bug title"));
        assert!(!ctx.contains("Author"));
    }
}
