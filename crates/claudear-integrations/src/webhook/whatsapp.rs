//! WhatsApp Cloud API webhook handler.

use super::WebhookHandler;
use crate::ask_reply_inbox;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use claudear_config::config::WhatsAppConfig;
use claudear_core::error::Result;
use claudear_core::secret::OptionalSecretExt;
use claudear_core::types::{Issue, MatchPriority, MatchResult};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use subtle::ConstantTimeEq;

/// Webhook handler for WhatsApp inbound messages.
pub struct WhatsAppWebhookHandler {
    config: WhatsAppConfig,
}

impl WhatsAppWebhookHandler {
    /// Create a new WhatsApp webhook handler.
    pub fn new(config: WhatsAppConfig) -> Self {
        Self { config }
    }

    fn listen_phone_number_id(&self) -> Option<&str> {
        self.config
            .listen_phone_number_id
            .as_deref()
            .or(self.config.phone_number_id.as_deref())
    }

    fn extract_title(text: &str) -> String {
        let first_line = text.lines().next().unwrap_or(text);
        if first_line.len() > 100 {
            format!("{}...", &first_line[..first_line.floor_char_boundary(97)])
        } else {
            first_line.to_string()
        }
    }
}

#[async_trait]
impl WebhookHandler for WhatsAppWebhookHandler {
    fn source_name(&self) -> &str {
        "whatsapp"
    }

    fn verify_signature(&self, payload: &[u8], headers: &HashMap<String, String>) -> bool {
        let secret = match self.config.app_secret.expose_as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                tracing::error!(
                    source = "whatsapp",
                    "No WhatsApp app_secret configured - rejecting webhook for security"
                );
                return false;
            }
        };

        let signature = match headers.get("x-hub-signature-256") {
            Some(v) => v,
            None => return false,
        };
        let sig_hex = match signature.strip_prefix("sha256=") {
            Some(v) => v,
            None => return false,
        };

        let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
            Ok(m) => m,
            Err(_) => return false,
        };
        mac.update(payload);
        let expected_hex = hex::encode(mac.finalize().into_bytes());
        sig_hex.as_bytes().ct_eq(expected_hex.as_bytes()).into()
    }

    async fn parse_payload(&self, payload: &serde_json::Value) -> Result<Option<Issue>> {
        let entries = match payload.get("entry").and_then(|v| v.as_array()) {
            Some(v) => v,
            None => return Ok(None),
        };

        for entry in entries {
            let changes = match entry.get("changes").and_then(|v| v.as_array()) {
                Some(v) => v,
                None => continue,
            };

            for change in changes {
                let value = match change.get("value") {
                    Some(v) => v,
                    None => continue,
                };

                let phone_number_id = value
                    .get("metadata")
                    .and_then(|m| m.get("phone_number_id"))
                    .and_then(|v| v.as_str());
                if let (Some(expected), Some(actual)) =
                    (self.listen_phone_number_id(), phone_number_id)
                {
                    if expected != actual {
                        continue;
                    }
                }

                let messages = match value.get("messages").and_then(|v| v.as_array()) {
                    Some(v) => v,
                    None => continue,
                };

                for msg in messages {
                    let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if msg_type != "text" {
                        continue;
                    }

                    let text = match msg
                        .get("text")
                        .and_then(|t| t.get("body"))
                        .and_then(|v| v.as_str())
                    {
                        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
                        _ => continue,
                    };

                    let message_id = match msg.get("id").and_then(|v| v.as_str()) {
                        Some(v) if !v.is_empty() => v.to_string(),
                        _ => continue,
                    };
                    let from = msg
                        .get("from")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let timestamp = msg
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let context_message_id = msg
                        .get("context")
                        .and_then(|c| c.get("id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    let replied_at = timestamp
                        .parse::<i64>()
                        .ok()
                        .and_then(|secs| Utc.timestamp_opt(secs, 0).single())
                        .unwrap_or_else(Utc::now);

                    ask_reply_inbox::record_whatsapp_message(
                        ask_reply_inbox::WhatsAppInboundMessage {
                            message_id: message_id.clone(),
                            from: from.clone(),
                            text: text.clone(),
                            replied_at,
                            context_message_id: context_message_id.clone(),
                        },
                    );

                    let short_id = format!("WA-{}", message_id.chars().take(8).collect::<String>());
                    let title = Self::extract_title(&text);
                    let mut issue = Issue::new(&message_id, &short_id, &title, "", "whatsapp");
                    issue.description = Some(text);
                    issue.set_metadata("author_phone", &from);
                    issue.set_metadata("message_id", &message_id);
                    if let Some(phone_number_id) = phone_number_id {
                        issue.set_metadata("phone_number_id", phone_number_id);
                    }

                    return Ok(Some(issue));
                }
            }
        }

        Ok(None)
    }

    fn matches_criteria(&self, _issue: &Issue) -> MatchResult {
        MatchResult::matched("whatsapp_message", MatchPriority::Normal)
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        let mut context = format!("WhatsApp Message Issue: {}\n", issue.title);
        if let Some(desc) = &issue.description {
            context.push_str("\nMessage:\n");
            context.push_str(desc);
            context.push('\n');
        }
        if let Some(phone) = issue.get_metadata::<String>("author_phone") {
            context.push_str(&format!("\nAuthor Phone: {}\n", phone));
        }
        Ok(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_config::config::WhatsAppConfig;
    use claudear_core::secret::SecretValue;

    fn make_handler(
        app_secret: Option<&str>,
        phone_number_id: Option<&str>,
    ) -> WhatsAppWebhookHandler {
        WhatsAppWebhookHandler::new(WhatsAppConfig {
            app_secret: app_secret.map(SecretValue::from),
            listen_phone_number_id: phone_number_id.map(|s| s.to_string()),
            ..Default::default()
        })
    }

    #[test]
    fn test_source_name() {
        let handler = make_handler(None, None);
        assert_eq!(handler.source_name(), "whatsapp");
    }

    #[test]
    fn test_listen_phone_number_id_from_listen_field() {
        let handler = make_handler(None, Some("12345"));
        assert_eq!(handler.listen_phone_number_id(), Some("12345"));
    }

    #[test]
    fn test_listen_phone_number_id_falls_back() {
        let handler = WhatsAppWebhookHandler::new(WhatsAppConfig {
            phone_number_id: Some("67890".to_string()),
            ..Default::default()
        });
        assert_eq!(handler.listen_phone_number_id(), Some("67890"));
    }

    #[test]
    fn test_listen_phone_number_id_none() {
        let handler = make_handler(None, None);
        assert_eq!(handler.listen_phone_number_id(), None);
    }

    #[test]
    fn test_extract_title_short() {
        assert_eq!(WhatsAppWebhookHandler::extract_title("Hello"), "Hello");
    }

    #[test]
    fn test_extract_title_long() {
        let long = "b".repeat(200);
        let title = WhatsAppWebhookHandler::extract_title(&long);
        assert!(title.len() <= 103);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn test_verify_signature_no_secret() {
        let handler = make_handler(None, None);
        assert!(!handler.verify_signature(b"payload", &HashMap::new()));
    }

    #[test]
    fn test_verify_signature_missing_header() {
        let handler = make_handler(Some("secret"), None);
        assert!(!handler.verify_signature(b"payload", &HashMap::new()));
    }

    #[test]
    fn test_verify_signature_no_sha256_prefix() {
        let handler = make_handler(Some("secret"), None);
        let mut headers = HashMap::new();
        headers.insert("x-hub-signature-256".to_string(), "noprefix".to_string());
        assert!(!handler.verify_signature(b"payload", &headers));
    }

    #[test]
    fn test_verify_signature_valid() {
        let secret = "wa_secret";
        let handler = make_handler(Some(secret), None);
        let payload = b"test_body";

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        let mut headers = HashMap::new();
        headers.insert("x-hub-signature-256".to_string(), sig);
        assert!(handler.verify_signature(payload, &headers));
    }

    #[test]
    fn test_verify_signature_wrong() {
        let handler = make_handler(Some("secret"), None);
        let mut headers = HashMap::new();
        headers.insert(
            "x-hub-signature-256".to_string(),
            "sha256=wrong".to_string(),
        );
        assert!(!handler.verify_signature(b"payload", &headers));
    }

    #[tokio::test]
    async fn test_parse_payload_no_entry() {
        let handler = make_handler(None, None);
        let payload = serde_json::json!({});
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_empty_entries() {
        let handler = make_handler(None, None);
        let payload = serde_json::json!({"entry": []});
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_no_changes() {
        let handler = make_handler(None, None);
        let payload = serde_json::json!({"entry": [{}]});
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_no_messages() {
        let handler = make_handler(None, None);
        let payload = serde_json::json!({
            "entry": [{"changes": [{"value": {}}]}]
        });
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_non_text_message() {
        let handler = make_handler(None, None);
        let payload = serde_json::json!({
            "entry": [{"changes": [{"value": {
                "messages": [{"type": "image", "id": "abc"}]
            }}]}]
        });
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_wrong_phone_number_id() {
        let handler = make_handler(None, Some("99999"));
        let payload = serde_json::json!({
            "entry": [{"changes": [{"value": {
                "metadata": {"phone_number_id": "11111"},
                "messages": [{"type": "text", "id": "wam123", "from": "+1234", "text": {"body": "hello"}, "timestamp": "1700000000"}]
            }}]}]
        });
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_valid_message() {
        let handler = make_handler(None, None);
        let payload = serde_json::json!({
            "entry": [{"changes": [{"value": {
                "metadata": {"phone_number_id": "12345"},
                "messages": [{
                    "type": "text",
                    "id": "wamid_abc12345",
                    "from": "+1555000111",
                    "text": {"body": "Bug: payment failing"},
                    "timestamp": "1700000000"
                }]
            }}]}]
        });
        let issue = handler.parse_payload(&payload).await.unwrap().unwrap();
        assert_eq!(issue.source, "whatsapp");
        assert!(issue.short_id.starts_with("WA-"));
        assert_eq!(issue.title, "Bug: payment failing");
        assert_eq!(
            issue.get_metadata::<String>("author_phone").unwrap(),
            "+1555000111"
        );
        assert_eq!(
            issue.get_metadata::<String>("phone_number_id").unwrap(),
            "12345"
        );
    }

    #[tokio::test]
    async fn test_parse_payload_empty_text() {
        let handler = make_handler(None, None);
        let payload = serde_json::json!({
            "entry": [{"changes": [{"value": {
                "messages": [{
                    "type": "text",
                    "id": "wamid_abc",
                    "from": "+1555",
                    "text": {"body": "  "}
                }]
            }}]}]
        });
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_parse_payload_empty_id() {
        let handler = make_handler(None, None);
        let payload = serde_json::json!({
            "entry": [{"changes": [{"value": {
                "messages": [{
                    "type": "text",
                    "id": "",
                    "from": "+1555",
                    "text": {"body": "hello"}
                }]
            }}]}]
        });
        assert!(handler.parse_payload(&payload).await.unwrap().is_none());
    }

    #[test]
    fn test_matches_criteria() {
        let handler = make_handler(None, None);
        let issue = Issue::new("1", "WA-1", "title", "url", "whatsapp");
        assert!(handler.matches_criteria(&issue).matches);
    }

    #[tokio::test]
    async fn test_build_issue_context() {
        let handler = make_handler(None, None);
        let mut issue = Issue::new("1", "WA-1", "Bug title", "", "whatsapp");
        issue.description = Some("Message content".to_string());
        issue.set_metadata("author_phone", "+1555");
        let ctx = handler.build_issue_context(&issue).await.unwrap();
        assert!(ctx.contains("Bug title"));
        assert!(ctx.contains("Message content"));
        assert!(ctx.contains("+1555"));
    }
}
