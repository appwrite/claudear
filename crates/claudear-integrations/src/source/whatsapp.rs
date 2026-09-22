//! WhatsApp message source adapter.
//!
//! Receives WhatsApp Cloud API webhook messages via an internal buffer and
//! converts them into issues. Unlike polling-based sources, WhatsApp delivers
//! messages via webhooks so this source drains a push-based buffer.

use super::IssueSource;
use crate::ask_reply_inbox;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use claudear_config::config::WhatsAppConfig;
use claudear_core::error::{Error, Result};
use claudear_core::types::{Issue, MatchPriority, MatchResult};
use std::sync::RwLock;

/// A single WhatsApp message received from the Cloud API webhook.
#[derive(Debug, Clone)]
pub struct WhatsAppMessage {
    /// WhatsApp message ID (e.g. "wamid.HBg...").
    pub id: String,
    /// Sender phone number in E.164 format.
    pub from: String,
    /// Message text body.
    pub text: String,
    /// Unix timestamp string from the webhook payload.
    pub timestamp: String,
    /// Optional message ID being replied to (webhook `context.id`).
    pub context_message_id: Option<String>,
}

/// WhatsApp source that converts webhook-delivered messages into issues.
pub struct WhatsAppSource {
    config: WhatsAppConfig,
    /// Internal message buffer populated by webhooks, drained by fetch_issues.
    buffer: RwLock<Vec<WhatsAppMessage>>,
    /// Cache of recently processed messages for get_issue lookups.
    cache: RwLock<std::collections::HashMap<String, WhatsAppMessage>>,
}

impl WhatsAppSource {
    /// Create a new WhatsApp source from config.
    pub fn new(config: WhatsAppConfig) -> Self {
        Self {
            config,
            buffer: RwLock::new(Vec::new()),
            cache: RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Push an incoming webhook message into the buffer for later processing.
    pub fn push_message(&self, msg: WhatsAppMessage) {
        if !msg.text.trim().is_empty() {
            let replied_at = msg
                .timestamp
                .parse::<i64>()
                .ok()
                .and_then(|secs| Utc.timestamp_opt(secs, 0).single())
                .unwrap_or_else(Utc::now);
            ask_reply_inbox::record_whatsapp_message(ask_reply_inbox::WhatsAppInboundMessage {
                message_id: msg.id.clone(),
                from: msg.from.clone(),
                text: msg.text.clone(),
                replied_at,
                context_message_id: msg.context_message_id.clone(),
            });
        }

        let mut buf = self.buffer.write().unwrap_or_else(|e| e.into_inner());
        buf.push(msg);
    }

    /// Get the phone number ID to listen on (listen_phone_number_id or fallback
    /// to phone_number_id).
    fn listen_phone_number_id(&self) -> Option<&str> {
        self.config
            .listen_phone_number_id
            .as_deref()
            .or(self.config.phone_number_id.as_deref())
    }

    /// Extract a title from message text (first line, max 100 chars).
    fn extract_title(text: &str) -> String {
        let first_line = text.lines().next().unwrap_or(text);
        if first_line.len() > 100 {
            format!("{}...", &first_line[..first_line.floor_char_boundary(97)])
        } else {
            first_line.to_string()
        }
    }

    /// Convert a WhatsApp message to an Issue.
    fn message_to_issue(msg: &WhatsAppMessage) -> Issue {
        let short_id = format!("WA-{}", msg.id.chars().take(8).collect::<String>());
        let title = Self::extract_title(&msg.text);

        let mut issue = Issue::new(&msg.id, &short_id, &title, "", "whatsapp");
        issue.description = Some(msg.text.clone());
        issue.set_metadata("author_phone", &msg.from);
        issue.set_metadata("message_id", &msg.id);

        issue
    }
}

#[async_trait]
impl IssueSource for WhatsAppSource {
    fn name(&self) -> &str {
        "whatsapp"
    }

    fn display_name(&self) -> &str {
        "WhatsApp Messages"
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        // Drain all buffered messages atomically.
        let messages: Vec<WhatsAppMessage> = {
            let mut buf = self.buffer.write().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *buf)
        };

        if messages.is_empty() {
            return Ok(vec![]);
        }

        // Cache messages for later get_issue lookups, then convert to issues.
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        let issues: Vec<Issue> = messages
            .into_iter()
            .filter(|msg| !msg.text.trim().is_empty())
            .map(|msg| {
                let issue = Self::message_to_issue(&msg);
                cache.insert(msg.id.clone(), msg);
                issue
            })
            .collect();

        Ok(issues)
    }

    fn matches_criteria(&self, _issue: &Issue) -> MatchResult {
        // All WhatsApp messages that pass filtering are valid issues.
        MatchResult::matched("whatsapp_message", MatchPriority::Normal)
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        let mut context = format!("WhatsApp Message Issue: {}\n", issue.title);

        if let Some(ref desc) = issue.description {
            context.push_str(&format!("\nMessage:\n{}\n", desc));
        }

        if let Some(phone) = issue.get_metadata::<String>("author_phone") {
            context.push_str(&format!("\nAuthor Phone: {}\n", phone));
        }

        if !issue.url.is_empty() {
            context.push_str(&format!("\nURL: {}\n", issue.url));
        }

        Ok(context)
    }

    async fn get_issue(&self, issue_id: &str) -> Result<Issue> {
        let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
        match cache.get(issue_id) {
            Some(msg) => Ok(Self::message_to_issue(msg)),
            None => Err(Error::issue_not_found("whatsapp", issue_id)),
        }
    }

    async fn create_issue(
        &self,
        title: &str,
        description: &str,
        _labels: &[String],
    ) -> Result<Issue> {
        let phone_number_id = self
            .listen_phone_number_id()
            .ok_or_else(|| {
                Error::config("WhatsApp phone_number_id is required to create an issue")
            })?
            .to_string();

        let access_token = self
            .config
            .access_token
            .as_ref()
            .map(|s| s.expose())
            .ok_or_else(|| Error::config("WhatsApp access_token is required to create an issue"))?;

        let content = if description.is_empty() {
            title.to_string()
        } else {
            format!("{}\n\n{}", title, description)
        };

        // Send to each configured recipient via the WhatsApp Cloud API.
        if self.config.to_numbers.is_empty() {
            return Err(Error::config(
                "WhatsApp to_numbers must contain at least one recipient",
            ));
        }

        let http = claudear_core::tls::client();
        let url = format!(
            "https://graph.facebook.com/v21.0/{}/messages",
            phone_number_id
        );

        // Send to the first recipient and use the response to build the issue.
        let recipient = &self.config.to_numbers[0];
        let payload = serde_json::json!({
            "messaging_product": "whatsapp",
            "to": recipient,
            "type": "text",
            "text": { "body": content }
        });

        let resp = http
            .post(&url)
            .bearer_auth(access_token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Failed to send WhatsApp message: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "WhatsApp API returned {}: {}",
                status, body
            )));
        }

        let resp_body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Failed to parse WhatsApp API response: {}", e)))?;

        // Extract the message ID from the response.
        let msg_id = resp_body["messages"][0]["id"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        let msg = WhatsAppMessage {
            id: msg_id,
            from: phone_number_id,
            text: content,
            timestamp: chrono::Utc::now().timestamp().to_string(),
            context_message_id: None,
        };

        let issue = Self::message_to_issue(&msg);

        // Cache the sent message for future lookups.
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        cache.insert(msg.id.clone(), msg);

        Ok(issue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> WhatsAppConfig {
        WhatsAppConfig {
            phone_number_id: Some("123456789".to_string()),
            access_token: Some("test-token".into()),
            business_account_id: None,
            app_secret: None,
            webhook_verify_token: None,
            to_numbers: vec!["+1234567890".to_string()],
            source_enabled: true,
            listen_phone_number_id: None,
            poll_interval_ms: None,
        }
    }

    fn make_message(id: &str, from: &str, text: &str) -> WhatsAppMessage {
        WhatsAppMessage {
            id: id.to_string(),
            from: from.to_string(),
            text: text.to_string(),
            timestamp: "1700000000".to_string(),
            context_message_id: None,
        }
    }

    #[test]
    fn test_extract_title_short() {
        assert_eq!(WhatsAppSource::extract_title("Short title"), "Short title");
    }

    #[test]
    fn test_extract_title_multiline() {
        assert_eq!(
            WhatsAppSource::extract_title("First line\nSecond line\nThird"),
            "First line"
        );
    }

    #[test]
    fn test_extract_title_long() {
        let long = "a".repeat(150);
        let title = WhatsAppSource::extract_title(&long);
        assert_eq!(title.len(), 100);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn test_extract_title_empty() {
        assert_eq!(WhatsAppSource::extract_title(""), "");
    }

    #[test]
    fn test_extract_title_exactly_100_chars() {
        let exact = "b".repeat(100);
        let title = WhatsAppSource::extract_title(&exact);
        assert_eq!(title.len(), 100);
        assert!(!title.ends_with("..."));
    }

    #[test]
    fn test_message_to_issue_basic() {
        let msg = make_message(
            "wamid_abc12345xyz",
            "+15551234567",
            "Fix the login bug\nDetails",
        );
        let issue = WhatsAppSource::message_to_issue(&msg);

        assert_eq!(issue.id, "wamid_abc12345xyz");
        assert_eq!(issue.short_id, "WA-wamid_ab");
        assert_eq!(issue.title, "Fix the login bug");
        assert_eq!(issue.url, "");
        assert_eq!(issue.source, "whatsapp");
        assert_eq!(
            issue.description.as_deref(),
            Some("Fix the login bug\nDetails")
        );
        assert_eq!(
            issue.get_metadata::<String>("author_phone"),
            Some("+15551234567".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("message_id"),
            Some("wamid_abc12345xyz".to_string())
        );
    }

    #[test]
    fn test_message_to_issue_short_id_truncation() {
        let msg = make_message("ab", "+1", "text");
        let issue = WhatsAppSource::message_to_issue(&msg);
        // Short IDs shorter than 8 chars use whatever is available.
        assert_eq!(issue.short_id, "WA-ab");
    }

    #[test]
    fn test_push_message_adds_to_buffer() {
        let source = WhatsAppSource::new(make_config());
        assert_eq!(source.buffer.read().unwrap().len(), 0);

        source.push_message(make_message("1", "+1", "hello"));
        assert_eq!(source.buffer.read().unwrap().len(), 1);
    }

    #[test]
    fn test_push_multiple_messages() {
        let source = WhatsAppSource::new(make_config());

        source.push_message(make_message("1", "+1", "first"));
        source.push_message(make_message("2", "+2", "second"));
        source.push_message(make_message("3", "+3", "third"));

        assert_eq!(source.buffer.read().unwrap().len(), 3);
    }

    #[test]
    fn test_buffer_preserves_order() {
        let source = WhatsAppSource::new(make_config());

        source.push_message(make_message("a", "+1", "first"));
        source.push_message(make_message("b", "+2", "second"));

        let buf = source.buffer.read().unwrap();
        assert_eq!(buf[0].id, "a");
        assert_eq!(buf[1].id, "b");
    }

    #[tokio::test]
    async fn test_fetch_issues_empty_buffer() {
        let source = WhatsAppSource::new(make_config());
        let issues = source.fetch_issues().await.unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_issues_drains_buffer() {
        let source = WhatsAppSource::new(make_config());

        source.push_message(make_message("1", "+1", "Bug report"));
        source.push_message(make_message("2", "+2", "Feature request"));

        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 2);

        // Buffer should be empty after drain.
        assert!(source.buffer.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_fetch_issues_populates_cache() {
        let source = WhatsAppSource::new(make_config());

        source.push_message(make_message("msg-1", "+1", "Test message"));
        let _ = source.fetch_issues().await.unwrap();

        let cache = source.cache.read().unwrap();
        assert!(cache.contains_key("msg-1"));
    }

    #[tokio::test]
    async fn test_fetch_issues_skips_empty_text() {
        let source = WhatsAppSource::new(make_config());

        source.push_message(make_message("1", "+1", "Valid text"));
        source.push_message(make_message("2", "+2", "   "));
        source.push_message(make_message("3", "+3", ""));

        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "1");
    }

    #[tokio::test]
    async fn test_fetch_issues_consecutive_calls() {
        let source = WhatsAppSource::new(make_config());

        source.push_message(make_message("1", "+1", "first batch"));
        let batch1 = source.fetch_issues().await.unwrap();
        assert_eq!(batch1.len(), 1);

        // Second call with empty buffer returns nothing.
        let batch2 = source.fetch_issues().await.unwrap();
        assert!(batch2.is_empty());

        // Push more messages, third call returns them.
        source.push_message(make_message("2", "+2", "second batch"));
        let batch3 = source.fetch_issues().await.unwrap();
        assert_eq!(batch3.len(), 1);
        assert_eq!(batch3[0].id, "2");
    }

    #[test]
    fn test_matches_criteria_always_matches() {
        let source = WhatsAppSource::new(make_config());
        let issue = Issue::new("1", "WA-1", "Test", "", "whatsapp");
        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.reason, "whatsapp_message");
    }

    #[test]
    fn test_matches_criteria_other_source() {
        let source = WhatsAppSource::new(make_config());
        // Should still match -- matches_criteria does not filter by source.
        let issue = Issue::new("1", "LIN-1", "Test", "http://test.com", "linear");
        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_build_issue_context_full() {
        let source = WhatsAppSource::new(make_config());
        let mut issue = Issue::new("1", "WA-1", "Fix login", "", "whatsapp");
        issue.description = Some("The login page is broken".to_string());
        issue.set_metadata("author_phone", "+15551234567");

        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("Fix login"));
        assert!(context.contains("The login page is broken"));
        assert!(context.contains("+15551234567"));
        // URL is empty, so no URL line should appear.
        assert!(!context.contains("URL:"));
    }

    #[tokio::test]
    async fn test_build_issue_context_no_description() {
        let source = WhatsAppSource::new(make_config());
        let issue = Issue::new("1", "WA-1", "Fix login", "", "whatsapp");

        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("Fix login"));
        assert!(!context.contains("Message:"));
    }

    #[tokio::test]
    async fn test_build_issue_context_no_phone() {
        let source = WhatsAppSource::new(make_config());
        let mut issue = Issue::new("1", "WA-1", "Fix login", "", "whatsapp");
        issue.description = Some("details".to_string());

        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(!context.contains("Author Phone:"));
    }

    #[tokio::test]
    async fn test_get_issue_from_cache() {
        let source = WhatsAppSource::new(make_config());

        // Push and fetch to populate cache.
        source.push_message(make_message("cached-1", "+1", "Cached message"));
        let _ = source.fetch_issues().await.unwrap();

        let issue = source.get_issue("cached-1").await.unwrap();
        assert_eq!(issue.id, "cached-1");
        assert_eq!(issue.source, "whatsapp");
    }

    #[tokio::test]
    async fn test_get_issue_not_found() {
        let source = WhatsAppSource::new(make_config());
        let result = source.get_issue("nonexistent").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Issue not found"));
        assert!(err.to_string().contains("whatsapp"));
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn test_name() {
        let source = WhatsAppSource::new(make_config());
        assert_eq!(source.name(), "whatsapp");
    }

    #[test]
    fn test_display_name() {
        let source = WhatsAppSource::new(make_config());
        assert_eq!(source.display_name(), "WhatsApp Messages");
    }

    #[test]
    fn test_listen_phone_number_id_fallback() {
        let source = WhatsAppSource::new(make_config());
        assert_eq!(source.listen_phone_number_id(), Some("123456789"));
    }

    #[test]
    fn test_listen_phone_number_id_explicit() {
        let mut config = make_config();
        config.listen_phone_number_id = Some("override-id".to_string());
        let source = WhatsAppSource::new(config);
        assert_eq!(source.listen_phone_number_id(), Some("override-id"));
    }

    #[test]
    fn test_listen_phone_number_id_none() {
        let mut config = make_config();
        config.phone_number_id = None;
        config.listen_phone_number_id = None;
        let source = WhatsAppSource::new(config);
        assert_eq!(source.listen_phone_number_id(), None);
    }

    #[test]
    fn test_new_empty_buffer() {
        let source = WhatsAppSource::new(make_config());
        assert!(source.buffer.read().unwrap().is_empty());
    }

    #[test]
    fn test_new_empty_cache() {
        let source = WhatsAppSource::new(make_config());
        assert!(source.cache.read().unwrap().is_empty());
    }

    #[test]
    fn test_whatsapp_message_clone() {
        let msg = make_message("1", "+1", "hello");
        let cloned = msg.clone();
        assert_eq!(cloned.id, msg.id);
        assert_eq!(cloned.from, msg.from);
        assert_eq!(cloned.text, msg.text);
        assert_eq!(cloned.timestamp, msg.timestamp);
    }

    #[test]
    fn test_whatsapp_message_debug() {
        let msg = make_message("1", "+1", "hello");
        let debug_str = format!("{:?}", msg);
        assert!(debug_str.contains("WhatsAppMessage"));
        assert!(debug_str.contains("hello"));
    }

    #[tokio::test]
    async fn test_create_issue_no_phone_number_id() {
        let mut config = make_config();
        config.phone_number_id = None;
        let source = WhatsAppSource::new(config);

        let result = source.create_issue("title", "desc", &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("phone_number_id"));
    }

    #[tokio::test]
    async fn test_create_issue_no_access_token() {
        let mut config = make_config();
        config.access_token = None;
        let source = WhatsAppSource::new(config);

        let result = source.create_issue("title", "desc", &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("access_token"));
    }

    #[tokio::test]
    async fn test_create_issue_no_recipients() {
        let mut config = make_config();
        config.to_numbers = vec![];
        let source = WhatsAppSource::new(config);

        let result = source.create_issue("title", "desc", &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("to_numbers"));
    }

    // --- Additional coverage tests ---

    #[tokio::test]
    async fn test_build_issue_context_with_url() {
        let source = WhatsAppSource::new(make_config());
        let mut issue = Issue::new(
            "1",
            "WA-1",
            "Fix login",
            "https://wa.me/msg/123",
            "whatsapp",
        );
        issue.description = Some("Details here".to_string());

        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("Fix login"));
        assert!(context.contains("Details here"));
        assert!(context.contains("URL: https://wa.me/msg/123"));
    }

    #[tokio::test]
    async fn test_cache_accumulates_across_fetches() {
        let source = WhatsAppSource::new(make_config());

        source.push_message(make_message("msg-a", "+1", "first"));
        let _ = source.fetch_issues().await.unwrap();

        source.push_message(make_message("msg-b", "+2", "second"));
        let _ = source.fetch_issues().await.unwrap();

        let cache = source.cache.read().unwrap();
        assert_eq!(cache.len(), 2);
        assert!(cache.contains_key("msg-a"));
        assert!(cache.contains_key("msg-b"));
    }

    #[test]
    fn test_message_to_issue_empty_text() {
        let msg = make_message("id1", "+1", "");
        let issue = WhatsAppSource::message_to_issue(&msg);
        assert_eq!(issue.title, "");
        assert_eq!(issue.description.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn test_fetch_issues_preserves_order() {
        let source = WhatsAppSource::new(make_config());

        source.push_message(make_message("1", "+1", "alpha"));
        source.push_message(make_message("2", "+2", "beta"));
        source.push_message(make_message("3", "+3", "gamma"));

        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 3);
        assert_eq!(issues[0].id, "1");
        assert_eq!(issues[1].id, "2");
        assert_eq!(issues[2].id, "3");
    }

    #[tokio::test]
    async fn test_get_issue_after_multiple_fetches() {
        let source = WhatsAppSource::new(make_config());

        source.push_message(make_message("first", "+1", "First message"));
        let _ = source.fetch_issues().await.unwrap();

        source.push_message(make_message("second", "+2", "Second message"));
        let _ = source.fetch_issues().await.unwrap();

        let first = source.get_issue("first").await.unwrap();
        assert_eq!(first.title, "First message");
        let second = source.get_issue("second").await.unwrap();
        assert_eq!(second.title, "Second message");
    }

    #[test]
    fn test_matches_criteria_priority_is_normal() {
        let source = WhatsAppSource::new(make_config());
        let issue = Issue::new("1", "WA-1", "Test", "", "whatsapp");
        let result = source.matches_criteria(&issue);
        assert_eq!(result.priority, MatchPriority::Normal);
    }

    #[tokio::test]
    async fn test_default_resolve_issue() {
        let source = WhatsAppSource::new(make_config());
        let result = source.resolve_issue("any-id").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_default_add_comment() {
        let source = WhatsAppSource::new(make_config());
        let result = source.add_comment("any-id", "test comment").await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_default_is_terminal_status() {
        let source = WhatsAppSource::new(make_config());
        assert!(source.is_terminal_status("completed"));
        assert!(source.is_terminal_status("resolved"));
        assert!(source.is_terminal_status("cancelled"));
        assert!(source.is_terminal_status("canceled"));
        assert!(source.is_terminal_status("ignored"));
        assert!(source.is_terminal_status("closed"));
        assert!(source.is_terminal_status("done"));
        assert!(!source.is_terminal_status("open"));
        assert!(!source.is_terminal_status("in_progress"));
    }

    #[tokio::test]
    async fn test_default_list_open_issues() {
        let source = WhatsAppSource::new(make_config());
        let result = source.list_open_issues("filter").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_default_find_or_create_label() {
        let source = WhatsAppSource::new(make_config());
        let result = source.find_or_create_label("bug").await;
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_title_101_chars() {
        let s = "a".repeat(101);
        let title = WhatsAppSource::extract_title(&s);
        assert_eq!(title.len(), 100);
        assert!(title.ends_with("..."));
    }

    #[tokio::test]
    async fn test_fetch_issues_whitespace_only_filtered() {
        let source = WhatsAppSource::new(make_config());

        source.push_message(make_message("1", "+1", "\t\n  "));
        source.push_message(make_message("2", "+2", "valid"));

        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "2");
    }

    #[test]
    fn test_message_to_issue_long_id() {
        let msg = make_message(
            "wamid.HBgLMTIzNDU2Nzg5MBUCABI3MTIzNDU2Nzg5MA==",
            "+1",
            "text",
        );
        let issue = WhatsAppSource::message_to_issue(&msg);
        assert_eq!(issue.short_id, "WA-wamid.HB");
    }

    #[tokio::test]
    async fn test_fetch_issues_empty_text_not_cached() {
        let source = WhatsAppSource::new(make_config());

        source.push_message(make_message("empty-1", "+1", "   "));

        let issues = source.fetch_issues().await.unwrap();
        assert!(issues.is_empty());

        let cache = source.cache.read().unwrap();
        assert!(!cache.contains_key("empty-1"));
    }

    #[tokio::test]
    async fn test_build_issue_context_empty_url() {
        let source = WhatsAppSource::new(make_config());
        let issue = Issue::new("1", "WA-1", "Title", "", "whatsapp");
        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(!context.contains("URL:"));
    }

    #[test]
    fn test_message_to_issue_sets_source() {
        let msg = make_message("1", "+1", "text");
        let issue = WhatsAppSource::message_to_issue(&msg);
        assert_eq!(issue.source, "whatsapp");
    }

    #[test]
    fn test_push_message_recovery() {
        let source = WhatsAppSource::new(make_config());
        source.push_message(make_message("1", "+1", "test"));
        assert_eq!(source.buffer.read().unwrap().len(), 1);
    }
}
