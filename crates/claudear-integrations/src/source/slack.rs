//! Slack message source adapter.
//!
//! Polls a Slack channel for human messages and converts them into issues.

use super::IssueSource;
use async_trait::async_trait;
use claudear_config::config::SlackConfig;
use claudear_core::error::{Error, Result};
use claudear_core::types::{Issue, MatchPriority, MatchResult};
use serde::Deserialize;
use std::sync::RwLock;

/// Slack API response for conversations.history.
#[derive(Debug, Deserialize)]
struct SlackHistoryResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    messages: Vec<SlackMessage>,
}

/// A Slack message from the API.
#[derive(Debug, Deserialize)]
struct SlackMessage {
    /// Slack timestamp (unique message ID), e.g. "1709123456.789012".
    ts: String,
    /// Message text.
    #[serde(default)]
    text: String,
    /// User ID of the sender (absent for bot messages posted via webhooks).
    user: Option<String>,
    /// Bot ID (present if sent by a bot).
    bot_id: Option<String>,
    /// Message subtype (e.g. "bot_message" for incoming webhook posts).
    /// Messages posted via `chat.postMessage` by a bot have no subtype,
    /// while incoming webhook posts have `subtype: "bot_message"`.
    subtype: Option<String>,
    /// Channel ID (may not be present in history responses).
    #[allow(dead_code)]
    #[serde(default)]
    channel: Option<String>,
}

/// Slack channel polling source that converts messages into issues.
pub struct SlackSource {
    config: SlackConfig,
    /// Last seen timestamp for incremental polling. `None` means first poll (seed).
    last_seen_ts: RwLock<Option<String>>,
    /// Reusable HTTP client.
    client: reqwest::Client,
}

impl SlackSource {
    /// Create a new Slack source from config.
    pub fn new(config: SlackConfig) -> Self {
        let client = claudear_core::tls::client_builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| claudear_core::tls::client());
        Self {
            config,
            last_seen_ts: RwLock::new(None),
            client,
        }
    }

    /// Get the channel ID to listen on (listen_channel_id or fallback to channel_id).
    fn listen_channel_id(&self) -> Option<&str> {
        self.config
            .listen_channel_id
            .as_deref()
            .or(self.config.channel_id.as_deref())
    }

    /// Check if a message is from a bot (excludes incoming webhook messages).
    ///
    /// Incoming webhook posts have `subtype: "bot_message"` and `bot_id`, while
    /// direct bot posts via `chat.postMessage` have `bot_id` but no `subtype`.
    /// Webhook messages represent external/user-triggered actions and should be
    /// treated as valid issues (analogous to Discord's `webhook_id` check).
    fn is_bot_message(msg: &SlackMessage) -> bool {
        msg.bot_id.is_some() && msg.subtype.is_none()
    }

    /// Extract a title from message content (first line, max 100 chars).
    fn extract_title(content: &str) -> String {
        let first_line = content.lines().next().unwrap_or(content);
        if first_line.len() > 100 {
            format!("{}...", &first_line[..first_line.floor_char_boundary(97)])
        } else {
            first_line.to_string()
        }
    }

    /// Build a Slack message URL.
    /// Format: https://{workspace}.slack.com/archives/{channel}/p{ts_without_dots}
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

    /// Convert a Slack message to an Issue.
    fn message_to_issue(&self, msg: &SlackMessage, channel_id: &str) -> Issue {
        let short_id = format!("SLACK-{}", msg.ts.chars().take(8).collect::<String>());
        let title = Self::extract_title(&msg.text);
        let url = self.message_url(channel_id, &msg.ts);

        let mut issue = Issue::new(&msg.ts, &short_id, &title, &url, "slack");
        issue.description = Some(msg.text.clone());

        if let Some(ref user_id) = msg.user {
            issue.set_metadata("author_id", user_id);
        }
        issue.set_metadata("channel_id", channel_id);
        issue.set_metadata("message_ts", &msg.ts);

        issue
    }

    /// Fetch messages from Slack conversations.history.
    async fn fetch_history(
        &self,
        channel_id: &str,
        oldest: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SlackMessage>> {
        let bot_token = self
            .config
            .bot_token
            .as_ref()
            .map(|s| s.expose())
            .ok_or_else(|| Error::config("Slack bot_token is required for source polling"))?;

        let mut url = format!(
            "https://slack.com/api/conversations.history?channel={}&limit={}",
            channel_id, limit
        );
        if let Some(oldest_ts) = oldest {
            url.push_str(&format!("&oldest={}", oldest_ts));
        }

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", bot_token))
            .send()
            .await?;

        let body = response.text().await.unwrap_or_default();
        let parsed: SlackHistoryResponse = serde_json::from_str(&body).map_err(|e| {
            Error::notifier(
                "slack_source",
                format!("Failed to parse Slack response: {}", e),
            )
        })?;

        if !parsed.ok {
            return Err(Error::notifier(
                "slack_source",
                format!(
                    "Slack API error: {}",
                    parsed.error.unwrap_or_else(|| "unknown".to_string())
                ),
            ));
        }

        Ok(parsed.messages)
    }
}

#[async_trait]
impl IssueSource for SlackSource {
    fn name(&self) -> &str {
        "slack"
    }

    fn display_name(&self) -> &str {
        "Slack Messages"
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        let channel_id = self
            .listen_channel_id()
            .ok_or_else(|| {
                Error::config(
                    "Slack listen_channel_id or channel_id is required for source polling",
                )
            })?
            .to_string();

        let last_seen = self
            .last_seen_ts
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        match last_seen {
            None => {
                // First poll: seed the cursor with the latest message, return no issues
                let messages = self.fetch_history(&channel_id, None, 1).await?;
                if let Some(latest) = messages.first() {
                    let mut lock = self.last_seen_ts.write().unwrap_or_else(|e| e.into_inner());
                    *lock = Some(latest.ts.clone());
                    tracing::info!(ts = %latest.ts, "Slack source seeded cursor");
                }
                Ok(vec![])
            }
            Some(oldest_ts) => {
                let messages = self
                    .fetch_history(&channel_id, Some(&oldest_ts), 100)
                    .await?;

                if messages.is_empty() {
                    return Ok(vec![]);
                }

                // Update cursor to the latest message
                // Slack returns messages newest-first by default
                if let Some(latest) = messages.first() {
                    let mut lock = self.last_seen_ts.write().unwrap_or_else(|e| e.into_inner());
                    *lock = Some(latest.ts.clone());
                }

                // Filter out bot messages and empty content, convert to issues
                let issues: Vec<Issue> = messages
                    .iter()
                    .filter(|msg| !Self::is_bot_message(msg))
                    .filter(|msg| !msg.text.trim().is_empty())
                    .map(|msg| self.message_to_issue(msg, &channel_id))
                    .collect();

                Ok(issues)
            }
        }
    }

    fn matches_criteria(&self, _issue: &Issue) -> MatchResult {
        // All Slack messages that pass filtering are valid issues
        MatchResult::matched("slack_message", MatchPriority::Normal)
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        let mut context = format!("Slack Message Issue: {}\n", issue.title);

        if let Some(ref desc) = issue.description {
            context.push_str(&format!("\nMessage:\n{}\n", desc));
        }

        if let Some(author_id) = issue.get_metadata::<String>("author_id") {
            context.push_str(&format!("\nAuthor ID: {}\n", author_id));
        }

        context.push_str(&format!("\nURL: {}\n", issue.url));

        Ok(context)
    }

    async fn create_issue(
        &self,
        title: &str,
        description: &str,
        _labels: &[String],
    ) -> Result<Issue> {
        let bot_token = self
            .config
            .bot_token
            .as_ref()
            .map(|s| s.expose())
            .ok_or_else(|| Error::config("Slack bot_token is required to create an issue"))?;

        let channel_id = self
            .listen_channel_id()
            .ok_or_else(|| Error::config("Slack channel_id is required to create an issue"))?
            .to_string();

        let content = if description.is_empty() {
            title.to_string()
        } else {
            format!("{}\n\n{}", title, description)
        };

        // Prefer webhook URL: messages posted via webhook don't have bot_id,
        // so they bypass the is_bot_message filter in poll_issues. We post via
        // webhook then fetch the latest message from history to get its ts.
        if let Some(ref webhook_url) = self.config.webhook_url {
            let resp = self
                .client
                .post(webhook_url.expose())
                .json(&serde_json::json!({ "text": content }))
                .send()
                .await
                .map_err(|e| {
                    Error::notifier(
                        "slack_source",
                        format!("Failed to post Slack webhook: {}", e),
                    )
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(Error::notifier(
                    "slack_source",
                    format!("Slack webhook returned {}: {}", status, body),
                ));
            }

            // Brief delay then fetch latest message to get its ts
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let url = format!(
                "https://slack.com/api/conversations.history?channel={}&limit=1",
                channel_id
            );
            let history_resp: serde_json::Value = self
                .client
                .get(&url)
                .header("Authorization", format!("Bearer {}", bot_token))
                .send()
                .await
                .map_err(|e| {
                    Error::notifier(
                        "slack_source",
                        format!("Failed to fetch Slack history: {}", e),
                    )
                })?
                .json()
                .await
                .map_err(|e| {
                    Error::notifier(
                        "slack_source",
                        format!("Failed to parse Slack history response: {}", e),
                    )
                })?;

            let ts = history_resp
                .pointer("/messages/0/ts")
                .and_then(|v: &serde_json::Value| v.as_str())
                .ok_or_else(|| {
                    Error::notifier("slack_source", "No ts in Slack history after webhook post")
                })?;

            let msg = SlackMessage {
                ts: ts.to_string(),
                text: content,
                user: None,
                bot_id: None,
                subtype: None,
                channel: Some(channel_id.clone()),
            };

            return Ok(self.message_to_issue(&msg, &channel_id));
        }

        // Fallback: use bot token to post (message will have bot_id set)
        let response = self
            .client
            .post("https://slack.com/api/chat.postMessage")
            .header("Authorization", format!("Bearer {}", bot_token))
            .json(&serde_json::json!({
                "channel": channel_id,
                "text": content,
            }))
            .send()
            .await?;

        let body: serde_json::Value = response.json().await.map_err(|e| {
            Error::notifier(
                "slack_source",
                format!("Failed to parse Slack response: {}", e),
            )
        })?;

        if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let error = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(Error::notifier(
                "slack_source",
                format!("Slack API error posting message: {}", error),
            ));
        }

        let ts = body.get("ts").and_then(|v| v.as_str()).ok_or_else(|| {
            Error::notifier("slack_source", "No ts in Slack postMessage response")
        })?;

        let msg = SlackMessage {
            ts: ts.to_string(),
            text: content,
            user: None,
            bot_id: None,
            subtype: None,
            channel: Some(channel_id.clone()),
        };

        Ok(self.message_to_issue(&msg, &channel_id))
    }

    async fn get_issue(&self, issue_id: &str) -> Result<Issue> {
        let channel_id = self
            .listen_channel_id()
            .ok_or_else(|| Error::config("Slack channel_id is required to fetch an issue"))?
            .to_string();

        let bot_token = self
            .config
            .bot_token
            .as_ref()
            .map(|s| s.expose())
            .ok_or_else(|| Error::config("Slack bot_token is required to fetch an issue"))?;

        // Use conversations.history with oldest=ts&latest=ts&inclusive=true to fetch
        // exactly one message by its timestamp.
        let url = format!(
            "https://slack.com/api/conversations.history?channel={}&oldest={}&latest={}&inclusive=true&limit=1",
            channel_id, issue_id, issue_id
        );

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", bot_token))
            .send()
            .await?;

        let body = response.text().await.unwrap_or_default();
        let parsed: SlackHistoryResponse = serde_json::from_str(&body).map_err(|e| {
            Error::notifier(
                "slack_source",
                format!("Failed to parse Slack response: {}", e),
            )
        })?;

        if !parsed.ok {
            return Err(Error::issue_not_found("slack", issue_id));
        }

        parsed
            .messages
            .first()
            .map(|msg| self.message_to_issue(msg, &channel_id))
            .ok_or_else(|| Error::issue_not_found("slack", issue_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> SlackConfig {
        SlackConfig {
            bot_token: Some("xoxb-test-token".into()),
            channel_id: Some("C12345".to_string()),
            source_enabled: true,
            listen_channel_id: None,
            workspace: Some("myworkspace".to_string()),
            ..Default::default()
        }
    }

    fn make_message(ts: &str, text: &str, bot: bool) -> SlackMessage {
        SlackMessage {
            ts: ts.to_string(),
            text: text.to_string(),
            user: if bot {
                None
            } else {
                Some("U12345".to_string())
            },
            bot_id: if bot {
                Some("B12345".to_string())
            } else {
                None
            },
            subtype: None,
            channel: Some("C12345".to_string()),
        }
    }

    #[test]
    fn test_extract_title_short() {
        assert_eq!(SlackSource::extract_title("Short title"), "Short title");
    }

    #[test]
    fn test_extract_title_multiline() {
        assert_eq!(
            SlackSource::extract_title("First line\nSecond line\nThird"),
            "First line"
        );
    }

    #[test]
    fn test_extract_title_long() {
        let long = "a".repeat(150);
        let title = SlackSource::extract_title(&long);
        assert_eq!(title.len(), 100);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn test_is_bot_message() {
        let bot_msg = make_message("1.0", "hello", true);
        let human_msg = make_message("2.0", "hello", false);
        assert!(SlackSource::is_bot_message(&bot_msg));
        assert!(!SlackSource::is_bot_message(&human_msg));
    }

    #[test]
    fn test_message_url_with_workspace() {
        let source = SlackSource::new(make_config());
        let url = source.message_url("C12345", "1709123456.789012");
        assert_eq!(
            url,
            "https://myworkspace.slack.com/archives/C12345/p1709123456789012"
        );
    }

    #[test]
    fn test_message_url_without_workspace() {
        let mut config = make_config();
        config.workspace = None;
        let source = SlackSource::new(config);
        let url = source.message_url("C12345", "1709123456.789012");
        assert_eq!(url, "https://slack.com/archives/C12345/p1709123456789012");
    }

    #[test]
    fn test_message_to_issue() {
        let source = SlackSource::new(make_config());
        let msg = make_message(
            "17091234.789012",
            "Fix the login bug\nMore details here",
            false,
        );
        let issue = source.message_to_issue(&msg, "C12345");

        assert_eq!(issue.id, "17091234.789012");
        assert_eq!(issue.short_id, "SLACK-17091234");
        assert_eq!(issue.title, "Fix the login bug");
        assert_eq!(issue.source, "slack");
        assert_eq!(
            issue.description.as_deref(),
            Some("Fix the login bug\nMore details here")
        );
        assert_eq!(
            issue.get_metadata::<String>("author_id"),
            Some("U12345".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("channel_id"),
            Some("C12345".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("message_ts"),
            Some("17091234.789012".to_string())
        );
    }

    #[test]
    fn test_listen_channel_id_fallback() {
        let source = SlackSource::new(make_config());
        assert_eq!(source.listen_channel_id(), Some("C12345"));
    }

    #[test]
    fn test_listen_channel_id_explicit() {
        let mut config = make_config();
        config.listen_channel_id = Some("C99999".to_string());
        let source = SlackSource::new(config);
        assert_eq!(source.listen_channel_id(), Some("C99999"));
    }

    #[test]
    fn test_name_and_display_name() {
        let source = SlackSource::new(make_config());
        assert_eq!(source.name(), "slack");
        assert_eq!(source.display_name(), "Slack Messages");
    }

    #[test]
    fn test_matches_criteria() {
        let source = SlackSource::new(make_config());
        let issue = Issue::new("1", "S-1", "Test", "http://test.com", "slack");
        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_build_issue_context() {
        let source = SlackSource::new(make_config());
        let mut issue = Issue::new(
            "1",
            "SLACK-1",
            "Fix login",
            "https://myworkspace.slack.com/archives/C12345/p1",
            "slack",
        );
        issue.description = Some("Fix the login bug please".to_string());
        issue.set_metadata("author_id", "U12345");

        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("Fix login"));
        assert!(context.contains("Fix the login bug please"));
        assert!(context.contains("U12345"));
        assert!(context.contains("https://myworkspace.slack.com/archives/C12345/p1"));
    }

    #[tokio::test]
    async fn test_get_issue_returns_not_found() {
        let source = SlackSource::new(make_config());
        let result = source.get_issue("123").await;
        assert!(result.is_err());
    }

    #[test]
    fn test_seed_behavior_initial_state() {
        let source = SlackSource::new(make_config());
        assert!(source.last_seen_ts.read().unwrap().is_none());
    }

    #[test]
    fn test_last_seen_ts_write_and_read() {
        let source = SlackSource::new(make_config());
        // Initially None
        assert!(source.last_seen_ts.read().unwrap().is_none());

        // Write a value
        {
            let mut lock = source.last_seen_ts.write().unwrap();
            *lock = Some("1709123456.789012".to_string());
        }

        // Read it back
        let ts = source.last_seen_ts.read().unwrap().clone();
        assert_eq!(ts, Some("1709123456.789012".to_string()));
    }

    #[test]
    fn test_last_seen_ts_overwrite() {
        let source = SlackSource::new(make_config());
        {
            let mut lock = source.last_seen_ts.write().unwrap();
            *lock = Some("1.0".to_string());
        }
        {
            let mut lock = source.last_seen_ts.write().unwrap();
            *lock = Some("2.0".to_string());
        }
        let ts = source.last_seen_ts.read().unwrap().clone();
        assert_eq!(ts, Some("2.0".to_string()));
    }

    #[test]
    fn test_last_seen_ts_multiple_reads_concurrent() {
        let source = SlackSource::new(make_config());
        {
            let mut lock = source.last_seen_ts.write().unwrap();
            *lock = Some("42.0".to_string());
        }
        // Multiple concurrent reads should not block each other
        let r1 = source.last_seen_ts.read().unwrap();
        let r2 = source.last_seen_ts.read().unwrap();
        assert_eq!(*r1, Some("42.0".to_string()));
        assert_eq!(*r2, Some("42.0".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_issue_returns_ok() {
        let source = SlackSource::new(make_config());
        let result = source.resolve_issue("some-issue-id").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_comment_returns_ok() {
        let source = SlackSource::new(make_config());
        let result = source.add_comment("some-issue-id", "a comment").await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_is_terminal_status_terminal_values() {
        let source = SlackSource::new(make_config());
        assert!(source.is_terminal_status("completed"));
        assert!(source.is_terminal_status("resolved"));
        assert!(source.is_terminal_status("cancelled"));
        assert!(source.is_terminal_status("canceled"));
        assert!(source.is_terminal_status("ignored"));
        assert!(source.is_terminal_status("closed"));
        assert!(source.is_terminal_status("done"));
    }

    #[test]
    fn test_is_terminal_status_case_insensitive() {
        let source = SlackSource::new(make_config());
        assert!(source.is_terminal_status("Completed"));
        assert!(source.is_terminal_status("RESOLVED"));
        assert!(source.is_terminal_status("Cancelled"));
        assert!(source.is_terminal_status("DONE"));
        assert!(source.is_terminal_status("Closed"));
    }

    #[test]
    fn test_is_terminal_status_non_terminal() {
        let source = SlackSource::new(make_config());
        assert!(!source.is_terminal_status("open"));
        assert!(!source.is_terminal_status("in_progress"));
        assert!(!source.is_terminal_status("pending"));
        assert!(!source.is_terminal_status("new"));
        assert!(!source.is_terminal_status(""));
        assert!(!source.is_terminal_status("something_random"));
    }

    #[tokio::test]
    async fn test_get_issue_status_propagates_not_found() {
        let source = SlackSource::new(make_config());
        // get_issue_status calls get_issue which returns IssueNotFound
        let result = source.get_issue_status("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_issue_error_contains_issue_id() {
        let source = SlackSource::new(make_config());
        let result = source.get_issue("SLACK-999").await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("SLACK-999") || err_msg.contains("slack"),
            "Error should reference the issue ID or source: {}",
            err_msg
        );
    }

    #[test]
    fn test_extract_title_empty_string() {
        assert_eq!(SlackSource::extract_title(""), "");
    }

    #[test]
    fn test_extract_title_exactly_100_chars() {
        let s = "a".repeat(100);
        let title = SlackSource::extract_title(&s);
        assert_eq!(title.len(), 100);
        assert!(!title.ends_with("..."));
    }

    #[test]
    fn test_extract_title_101_chars_gets_truncated() {
        let s = "a".repeat(101);
        let title = SlackSource::extract_title(&s);
        assert_eq!(title.len(), 100);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn test_extract_title_unicode_boundary() {
        // Use multibyte characters to ensure floor_char_boundary works correctly
        // Each emoji is 4 bytes. 25 emojis = 100 bytes but only 25 chars.
        // With more than 100 bytes in the first line, truncation kicks in.
        let s = "\u{1F600}".repeat(30); // 30 emojis = 120 bytes
        let title = SlackSource::extract_title(&s);
        // Should end with "..." and not panic on char boundary
        assert!(title.ends_with("..."));
        assert!(title.len() <= 100);
    }

    #[test]
    fn test_extract_title_whitespace_only_first_line() {
        assert_eq!(SlackSource::extract_title("   \nSecond line"), "   ");
    }

    #[test]
    fn test_extract_title_newline_only() {
        // lines() yields empty string for "\n"
        assert_eq!(SlackSource::extract_title("\n"), "");
    }

    #[test]
    fn test_message_to_issue_bot_message_no_author() {
        let source = SlackSource::new(make_config());
        let msg = make_message("1.0", "Bot says hello", true);
        let issue = source.message_to_issue(&msg, "C12345");

        // Bot messages have no user field, so author_id should not be set
        assert_eq!(issue.get_metadata::<String>("author_id"), None);
        assert_eq!(issue.source, "slack");
    }

    #[test]
    fn test_message_to_issue_empty_text() {
        let source = SlackSource::new(make_config());
        let msg = make_message("1.0", "", false);
        let issue = source.message_to_issue(&msg, "C12345");

        assert_eq!(issue.title, "");
        assert_eq!(issue.description, Some("".to_string()));
    }

    #[test]
    fn test_message_to_issue_short_id_format() {
        let source = SlackSource::new(make_config());
        // Timestamp with 8+ chars before take(8) truncation
        let msg = make_message("17091234.789012", "test", false);
        let issue = source.message_to_issue(&msg, "C12345");
        assert_eq!(issue.short_id, "SLACK-17091234");

        // Timestamp with fewer than 8 chars
        let msg2 = make_message("123", "test", false);
        let issue2 = source.message_to_issue(&msg2, "C12345");
        assert_eq!(issue2.short_id, "SLACK-123");
    }

    #[test]
    fn test_message_to_issue_url_included() {
        let source = SlackSource::new(make_config());
        let msg = make_message("1709123456.789012", "test", false);
        let issue = source.message_to_issue(&msg, "C99999");
        assert_eq!(
            issue.url,
            "https://myworkspace.slack.com/archives/C99999/p1709123456789012"
        );
    }

    #[tokio::test]
    async fn test_build_issue_context_no_description() {
        let source = SlackSource::new(make_config());
        let issue = Issue::new("1", "S-1", "Title only", "http://example.com", "slack");
        // No description, no author_id
        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("Title only"));
        assert!(context.contains("http://example.com"));
        assert!(!context.contains("Message:"));
        assert!(!context.contains("Author ID:"));
    }

    #[tokio::test]
    async fn test_build_issue_context_with_description_no_author() {
        let source = SlackSource::new(make_config());
        let mut issue = Issue::new("1", "S-1", "Title", "http://example.com", "slack");
        issue.description = Some("Some description text".to_string());
        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("Message:"));
        assert!(context.contains("Some description text"));
        assert!(!context.contains("Author ID:"));
    }

    #[tokio::test]
    async fn test_build_issue_context_with_author_no_description() {
        let source = SlackSource::new(make_config());
        let mut issue = Issue::new("1", "S-1", "Title", "http://example.com", "slack");
        issue.set_metadata("author_id", "UABC123");
        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("Author ID: UABC123"));
        assert!(!context.contains("Message:"));
    }

    #[test]
    fn test_matches_criteria_returns_normal_priority() {
        let source = SlackSource::new(make_config());
        let issue = Issue::new("1", "S-1", "Test", "http://test.com", "slack");
        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Normal);
        assert_eq!(result.reason, "slack_message");
    }

    #[test]
    fn test_matches_criteria_ignores_issue_content() {
        let source = SlackSource::new(make_config());
        // Criteria matching is unconditional for Slack -- any issue matches
        let issue1 = Issue::new("1", "S-1", "", "http://t.com", "other_source");
        let issue2 = Issue::new("2", "S-2", "Very important bug", "http://t.com", "slack");
        assert!(source.matches_criteria(&issue1).matches);
        assert!(source.matches_criteria(&issue2).matches);
    }

    #[test]
    fn test_listen_channel_id_both_none() {
        let mut config = make_config();
        config.channel_id = None;
        config.listen_channel_id = None;
        let source = SlackSource::new(config);
        assert_eq!(source.listen_channel_id(), None);
    }

    #[test]
    fn test_listen_channel_id_prefers_listen_over_channel() {
        let mut config = make_config();
        config.channel_id = Some("C_FALLBACK".to_string());
        config.listen_channel_id = Some("C_LISTEN".to_string());
        let source = SlackSource::new(config);
        assert_eq!(source.listen_channel_id(), Some("C_LISTEN"));
    }

    #[test]
    fn test_message_url_timestamp_no_dot() {
        let source = SlackSource::new(make_config());
        let url = source.message_url("C111", "12345");
        assert_eq!(url, "https://myworkspace.slack.com/archives/C111/p12345");
    }

    #[test]
    fn test_message_url_timestamp_multiple_dots() {
        let source = SlackSource::new(make_config());
        let url = source.message_url("C111", "1.2.3");
        assert_eq!(url, "https://myworkspace.slack.com/archives/C111/p123");
    }

    #[test]
    fn test_is_bot_message_no_bot_id_no_user() {
        // A message with neither user nor bot_id (edge case)
        let msg = SlackMessage {
            ts: "1.0".to_string(),
            text: "ghost".to_string(),
            user: None,
            bot_id: None,
            subtype: None,
            channel: None,
        };
        assert!(!SlackSource::is_bot_message(&msg));
    }

    #[test]
    fn test_is_bot_message_both_user_and_bot_id() {
        // A message with both user and bot_id, no subtype (direct bot post)
        let msg = SlackMessage {
            ts: "1.0".to_string(),
            text: "app msg".to_string(),
            user: Some("U123".to_string()),
            bot_id: Some("B456".to_string()),
            subtype: None,
            channel: None,
        };
        // Should be classified as bot because bot_id is present and no subtype
        assert!(SlackSource::is_bot_message(&msg));
    }

    #[test]
    fn test_is_bot_message_webhook_message() {
        // Incoming webhook messages have bot_id AND subtype "bot_message".
        // These should NOT be classified as bot messages (they simulate users).
        let msg = SlackMessage {
            ts: "1.0".to_string(),
            text: "webhook msg".to_string(),
            user: None,
            bot_id: Some("B456".to_string()),
            subtype: Some("bot_message".to_string()),
            channel: None,
        };
        assert!(!SlackSource::is_bot_message(&msg));
    }

    #[test]
    fn test_history_response_deserialize_ok() {
        let json = r#"{"ok":true,"messages":[{"ts":"1.0","text":"hello","user":"U1"}]}"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.messages.len(), 1);
        assert_eq!(resp.messages[0].text, "hello");
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_history_response_deserialize_error() {
        let json = r#"{"ok":false,"error":"channel_not_found"}"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error, Some("channel_not_found".to_string()));
        assert!(resp.messages.is_empty());
    }

    #[test]
    fn test_history_response_deserialize_missing_fields_defaults() {
        let json = r#"{"ok":true}"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.messages.is_empty());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_slack_message_deserialize_minimal() {
        let json = r#"{"ts":"1709.0","text":"hi"}"#;
        let msg: SlackMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.ts, "1709.0");
        assert_eq!(msg.text, "hi");
        assert!(msg.user.is_none());
        assert!(msg.bot_id.is_none());
        assert!(msg.channel.is_none());
    }

    #[test]
    fn test_slack_message_deserialize_defaults_on_missing_text() {
        let json = r#"{"ts":"1.0"}"#;
        let msg: SlackMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.text, ""); // #[serde(default)]
    }

    // Additional coverage: SlackHistoryResponse and SlackMessage
    // deserialization edge cases, extract_title boundary behavior,
    // listen_channel_id ordering, message_url formatting,
    // is_bot_message with various field combinations, and
    // message_to_issue with diverse inputs.
    #[test]
    fn test_history_response_deserialize_multiple_messages() {
        let json = r#"{
            "ok": true,
            "messages": [
                {"ts": "1.0", "text": "first", "user": "U1"},
                {"ts": "2.0", "text": "second", "user": "U2"},
                {"ts": "3.0", "text": "third", "bot_id": "B1"}
            ]
        }"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.messages.len(), 3);
        assert_eq!(resp.messages[0].ts, "1.0");
        assert_eq!(resp.messages[1].text, "second");
        assert!(resp.messages[2].bot_id.is_some());
    }

    #[test]
    fn test_history_response_deserialize_error_with_messages_defaults() {
        // Error response where messages field is absent
        let json = r#"{"ok": false, "error": "not_authed"}"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("not_authed"));
        assert!(resp.messages.is_empty());
    }

    #[test]
    fn test_history_response_deserialize_extra_fields_ignored() {
        // Slack API may return extra fields we don't model
        let json = r#"{"ok": true, "messages": [], "has_more": true, "pin_count": 5}"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.messages.is_empty());
    }

    #[test]
    fn test_slack_message_deserialize_full_fields() {
        let json = r#"{
            "ts": "1709123456.789012",
            "text": "Hello world",
            "user": "U12345",
            "bot_id": null,
            "channel": "C99999"
        }"#;
        let msg: SlackMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.ts, "1709123456.789012");
        assert_eq!(msg.text, "Hello world");
        assert_eq!(msg.user.as_deref(), Some("U12345"));
        assert!(msg.bot_id.is_none());
        assert_eq!(msg.channel.as_deref(), Some("C99999"));
    }

    #[test]
    fn test_slack_message_deserialize_bot_message() {
        let json = r#"{"ts": "1.0", "text": "bot says hi", "bot_id": "B789"}"#;
        let msg: SlackMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.bot_id.as_deref(), Some("B789"));
        assert!(msg.user.is_none());
    }

    #[test]
    fn test_slack_message_deserialize_unicode_text() {
        let json = r#"{"ts": "1.0", "text": "Hello \u4e16\u754c"}"#;
        let msg: SlackMessage = serde_json::from_str(json).unwrap();
        assert!(msg.text.contains('\u{4e16}')); // Chinese character
    }

    #[test]
    fn test_slack_message_deserialize_empty_text() {
        let json = r#"{"ts": "1.0", "text": ""}"#;
        let msg: SlackMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.text, "");
    }

    #[test]
    fn test_extract_title_exactly_97_chars_no_truncation() {
        let s = "a".repeat(97);
        let title = SlackSource::extract_title(&s);
        assert_eq!(title.len(), 97);
        assert!(!title.ends_with("..."));
    }

    #[test]
    fn test_extract_title_multiline_long_first_line() {
        let first_line = "x".repeat(150);
        let input = format!("{}\nSecond line", first_line);
        let title = SlackSource::extract_title(&input);
        assert_eq!(title.len(), 100);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn test_extract_title_only_newlines() {
        let title = SlackSource::extract_title("\n\n\n");
        assert_eq!(title, "");
    }

    #[test]
    fn test_extract_title_special_characters() {
        let input = "Bug: @user mentioned #channel <link|text>";
        let title = SlackSource::extract_title(input);
        assert_eq!(title, input);
    }

    #[test]
    fn test_is_bot_message_empty_bot_id_string() {
        // bot_id = Some("") with no subtype is still a bot message
        let msg = SlackMessage {
            ts: "1.0".to_string(),
            text: "test".to_string(),
            user: Some("U123".to_string()),
            bot_id: Some("".to_string()),
            subtype: None,
            channel: None,
        };
        assert!(SlackSource::is_bot_message(&msg));
    }

    #[test]
    fn test_listen_channel_id_empty_string_listen() {
        let mut config = make_config();
        config.listen_channel_id = Some("".to_string());
        let source = SlackSource::new(config);
        // Empty string is still Some, so listen_channel_id returns it
        assert_eq!(source.listen_channel_id(), Some(""));
    }

    #[test]
    fn test_listen_channel_id_only_channel_id_set() {
        let mut config = make_config();
        config.channel_id = Some("C_ONLY".to_string());
        config.listen_channel_id = None;
        let source = SlackSource::new(config);
        assert_eq!(source.listen_channel_id(), Some("C_ONLY"));
    }

    #[test]
    fn test_message_url_empty_timestamp() {
        let source = SlackSource::new(make_config());
        let url = source.message_url("C111", "");
        assert_eq!(url, "https://myworkspace.slack.com/archives/C111/p");
    }

    #[test]
    fn test_message_url_long_timestamp() {
        let source = SlackSource::new(make_config());
        let url = source.message_url("C111", "1234567890.123456");
        assert_eq!(
            url,
            "https://myworkspace.slack.com/archives/C111/p1234567890123456"
        );
    }

    #[test]
    fn test_message_to_issue_preserves_full_text_as_description() {
        let source = SlackSource::new(make_config());
        let long_text = "Line 1\nLine 2\nLine 3\nLine 4";
        let msg = make_message("1234.5678", long_text, false);
        let issue = source.message_to_issue(&msg, "C12345");

        assert_eq!(issue.description.as_deref(), Some(long_text));
        assert_eq!(issue.title, "Line 1");
    }

    #[test]
    fn test_message_to_issue_url_uses_channel_parameter() {
        let source = SlackSource::new(make_config());
        let msg = make_message("1.0", "test", false);
        let issue = source.message_to_issue(&msg, "CSPECIAL");

        assert!(issue.url.contains("CSPECIAL"));
        assert_eq!(
            issue.get_metadata::<String>("channel_id"),
            Some("CSPECIAL".to_string())
        );
    }

    #[test]
    fn test_message_to_issue_short_id_truncation() {
        let source = SlackSource::new(make_config());
        // Timestamp shorter than 8 chars
        let msg = make_message("ab", "test", false);
        let issue = source.message_to_issue(&msg, "C1");
        assert_eq!(issue.short_id, "SLACK-ab");

        // Timestamp exactly 8 chars
        let msg2 = make_message("12345678", "test", false);
        let issue2 = source.message_to_issue(&msg2, "C1");
        assert_eq!(issue2.short_id, "SLACK-12345678");

        // Timestamp longer than 8 chars
        let msg3 = make_message("123456789", "test", false);
        let issue3 = source.message_to_issue(&msg3, "C1");
        assert_eq!(issue3.short_id, "SLACK-12345678");
    }

    #[test]
    fn test_new_creates_source_with_defaults() {
        let source = SlackSource::new(make_config());
        assert!(source.last_seen_ts.read().unwrap().is_none());
        assert_eq!(
            source.config.bot_token.as_ref().map(|s| s.expose()),
            Some("xoxb-test-token")
        );
    }

    #[test]
    fn test_new_with_minimal_config() {
        let config = SlackConfig {
            bot_token: None,
            channel_id: None,
            source_enabled: false,
            listen_channel_id: None,
            workspace: None,
            ..Default::default()
        };
        let source = SlackSource::new(config);
        assert!(source.listen_channel_id().is_none());
    }

    #[test]
    fn test_name_is_slack() {
        let source = SlackSource::new(make_config());
        assert_eq!(IssueSource::name(&source), "slack");
    }

    #[test]
    fn test_display_name_is_slack_messages() {
        let source = SlackSource::new(make_config());
        assert_eq!(IssueSource::display_name(&source), "Slack Messages");
    }

    #[tokio::test]
    async fn test_build_issue_context_all_fields() {
        let source = SlackSource::new(make_config());
        let mut issue = Issue::new(
            "1709.123",
            "SLACK-1709",
            "Important bug",
            "https://myworkspace.slack.com/archives/C12345/p1709123",
            "slack",
        );
        issue.description = Some("Please fix the login page".to_string());
        issue.set_metadata("author_id", "U99999");

        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("Slack Message Issue: Important bug"));
        assert!(context.contains("Message:\nPlease fix the login page"));
        assert!(context.contains("Author ID: U99999"));
        assert!(context.contains("URL: https://myworkspace.slack.com/archives/C12345/p1709123"));
    }

    #[test]
    fn test_history_response_realistic_payload() {
        let json = r#"{
            "ok": true,
            "messages": [
                {
                    "ts": "1709123456.789012",
                    "text": "Hey team, the deployment pipeline is broken. Can someone look at the CI config?",
                    "user": "U08EXAMPLE1",
                    "channel": "C08PROJCHAN"
                },
                {
                    "ts": "1709123400.000001",
                    "text": "Automated build report: all checks passed",
                    "bot_id": "B08CIBOT001"
                }
            ],
            "has_more": false,
            "pin_count": 0,
            "response_metadata": {
                "next_cursor": ""
            }
        }"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.messages.len(), 2);

        let human_msg = &resp.messages[0];
        assert_eq!(human_msg.user.as_deref(), Some("U08EXAMPLE1"));
        assert!(!SlackSource::is_bot_message(human_msg));
        assert!(human_msg.text.contains("deployment pipeline"));

        let bot_msg = &resp.messages[1];
        assert_eq!(bot_msg.bot_id.as_deref(), Some("B08CIBOT001"));
        assert!(SlackSource::is_bot_message(bot_msg));
    }

    #[test]
    fn test_history_response_rate_limited_error() {
        let json = r#"{"ok": false, "error": "ratelimited"}"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("ratelimited"));
    }

    #[test]
    fn test_history_response_invalid_auth_error() {
        let json = r#"{"ok": false, "error": "invalid_auth"}"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("invalid_auth"));
    }

    #[test]
    fn test_matches_criteria_with_different_sources() {
        let source = SlackSource::new(make_config());
        for src in ["slack", "linear", "sentry", "jira", "unknown"] {
            let issue = Issue::new("1", "S-1", "Test", "http://t.com", src);
            assert!(
                source.matches_criteria(&issue).matches,
                "Should match for source '{}'",
                src
            );
        }
    }

    #[test]
    fn test_slack_config_default() {
        let config = SlackConfig::default();
        assert!(config.bot_token.is_none());
        assert!(config.channel_id.is_none());
        assert!(!config.source_enabled);
        assert!(config.listen_channel_id.is_none());
        assert!(config.workspace.is_none());
        assert!(config.poll_interval_ms.is_none());
    }

    #[test]
    fn test_slack_config_field_access() {
        let config = SlackConfig {
            bot_token: Some("xoxb-abc-123".into()),
            channel_id: Some("C_MAIN".to_string()),
            source_enabled: true,
            listen_channel_id: Some("C_LISTEN".to_string()),
            workspace: Some("myteam".to_string()),
            poll_interval_ms: Some(5000),
            ..Default::default()
        };
        assert_eq!(
            config.bot_token.as_ref().map(|s| s.expose()),
            Some("xoxb-abc-123")
        );
        assert_eq!(config.channel_id.as_deref(), Some("C_MAIN"));
        assert!(config.source_enabled);
        assert_eq!(config.listen_channel_id.as_deref(), Some("C_LISTEN"));
        assert_eq!(config.workspace.as_deref(), Some("myteam"));
        assert_eq!(config.poll_interval_ms, Some(5000));
    }

    #[test]
    fn test_slack_history_response_ok_empty_messages() {
        let json = r#"{"ok": true, "messages": []}"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.messages.is_empty());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_slack_history_response_ok_false_with_unknown_error() {
        let json = r#"{"ok": false, "error": "unknown_error_code"}"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("unknown_error_code"));
    }

    #[test]
    fn test_slack_message_deserialize_with_all_none_optional_fields() {
        let json =
            r#"{"ts": "999.0", "text": "msg", "user": null, "bot_id": null, "channel": null}"#;
        let msg: SlackMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.ts, "999.0");
        assert_eq!(msg.text, "msg");
        assert!(msg.user.is_none());
        assert!(msg.bot_id.is_none());
        assert!(msg.channel.is_none());
    }

    #[test]
    fn test_slack_message_deserialize_long_text() {
        let long_text = "x".repeat(5000);
        let json = format!(r#"{{"ts": "1.0", "text": "{}"}}"#, long_text);
        let msg: SlackMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.text.len(), 5000);
    }

    #[test]
    fn test_extract_title_tab_separated_first_line() {
        let input = "Title\there\nNext line";
        let title = SlackSource::extract_title(input);
        assert_eq!(title, "Title\there");
    }

    #[test]
    fn test_extract_title_with_leading_whitespace() {
        let input = "  Indented title\nBody";
        let title = SlackSource::extract_title(input);
        assert_eq!(title, "  Indented title");
    }

    #[test]
    fn test_message_to_issue_metadata_completeness() {
        let source = SlackSource::new(make_config());
        let msg = make_message("1709999999.123456", "Check this bug", false);
        let issue = source.message_to_issue(&msg, "CTEST");

        // Verify all expected metadata keys
        assert_eq!(
            issue.get_metadata::<String>("author_id"),
            Some("U12345".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("channel_id"),
            Some("CTEST".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("message_ts"),
            Some("1709999999.123456".to_string())
        );
    }

    #[test]
    fn test_message_to_issue_with_multiline_text_title_is_first_line() {
        let source = SlackSource::new(make_config());
        let msg = make_message("1.0", "First\nSecond\nThird\nFourth", false);
        let issue = source.message_to_issue(&msg, "C1");
        assert_eq!(issue.title, "First");
        assert_eq!(
            issue.description.as_deref(),
            Some("First\nSecond\nThird\nFourth")
        );
    }

    #[test]
    fn test_message_url_empty_channel() {
        let source = SlackSource::new(make_config());
        let url = source.message_url("", "1.0");
        assert_eq!(url, "https://myworkspace.slack.com/archives//p10");
    }

    #[test]
    fn test_is_bot_message_with_user_only() {
        let msg = SlackMessage {
            ts: "1.0".to_string(),
            text: "human".to_string(),
            user: Some("U999".to_string()),
            bot_id: None,
            subtype: None,
            channel: None,
        };
        assert!(!SlackSource::is_bot_message(&msg));
    }

    #[test]
    fn test_message_to_issue_long_text_truncates_title() {
        let source = SlackSource::new(make_config());
        let long_first_line = "a".repeat(200);
        let msg = make_message("1.0", &long_first_line, false);
        let issue = source.message_to_issue(&msg, "C1");

        assert_eq!(issue.title.len(), 100);
        assert!(issue.title.ends_with("..."));
        // Full text preserved in description
        assert_eq!(issue.description.as_deref(), Some(long_first_line.as_str()));
    }

    #[tokio::test]
    async fn test_build_issue_context_empty_description() {
        let source = SlackSource::new(make_config());
        let mut issue = Issue::new("1", "S-1", "Title", "http://example.com", "slack");
        issue.description = Some("".to_string());
        let context = source.build_issue_context(&issue).await.unwrap();
        // Empty description is still Some(""), so "Message:" section appears
        assert!(context.contains("Message:"));
    }

    #[test]
    fn test_slack_history_response_deserialize_with_response_metadata() {
        // Slack sometimes returns response_metadata with cursor info
        let json = r#"{
            "ok": true,
            "messages": [
                {"ts": "100.0", "text": "hello", "user": "U1"}
            ],
            "has_more": true,
            "response_metadata": {
                "next_cursor": "dXNlcjpXMDdMQ1RZTDE="
            }
        }"#;
        let resp: SlackHistoryResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.messages.len(), 1);
        assert_eq!(resp.messages[0].text, "hello");
    }

    #[test]
    fn test_slack_message_with_subtype_field() {
        // Slack messages can have extra fields like subtype which we don't model
        let json = r#"{
            "ts": "1.0",
            "text": "joined the channel",
            "user": "U1",
            "subtype": "channel_join"
        }"#;
        let msg: SlackMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.text, "joined the channel");
        assert_eq!(msg.user.as_deref(), Some("U1"));
    }

    #[test]
    fn test_message_to_issue_with_special_chars_in_text() {
        let source = SlackSource::new(make_config());
        let msg = make_message(
            "1.0",
            "Bug: <@U123> the `code` is *broken* & \"bad\"",
            false,
        );
        let issue = source.message_to_issue(&msg, "C1");
        assert_eq!(issue.title, "Bug: <@U123> the `code` is *broken* & \"bad\"");
        assert_eq!(
            issue.description.as_deref(),
            Some("Bug: <@U123> the `code` is *broken* & \"bad\"")
        );
    }

    #[test]
    fn test_matches_criteria_reason_is_slack_message() {
        let source = SlackSource::new(make_config());
        let issue = Issue::new("1", "S-1", "Test", "http://t.com", "slack");
        let result = source.matches_criteria(&issue);
        assert_eq!(result.reason, "slack_message");
    }

    #[test]
    fn test_new_source_config_preserved() {
        let config = SlackConfig {
            bot_token: Some("xoxb-test-123".into()),
            channel_id: Some("C_TEST".to_string()),
            source_enabled: true,
            listen_channel_id: Some("C_LISTEN".to_string()),
            workspace: Some("testws".to_string()),
            poll_interval_ms: Some(10000),
            ..Default::default()
        };
        let source = SlackSource::new(config);
        assert_eq!(
            source.config.bot_token.as_ref().map(|s| s.expose()),
            Some("xoxb-test-123")
        );
        assert_eq!(source.config.workspace.as_deref(), Some("testws"));
        assert_eq!(source.config.poll_interval_ms, Some(10000));
    }
}
