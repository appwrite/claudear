//! Slack notifier with Block Kit formatting and Q&A support.

use super::get_source_emoji;
use super::Notifier;
use crate::reports::Report;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use claudear_config::config::SlackConfig;
use claudear_config::users::UserRegistry;
use claudear_core::error::{Error, Result};
use claudear_core::http::HttpResponse;
use claudear_core::types::{AskDelivery, AskReply, AskRequest, Issue};
use serde::{Deserialize, Serialize};

/// Trait for HTTP client used by Slack notifier.
#[async_trait]
pub trait SlackHttpClient: Send + Sync {
    /// POST JSON to `url`. If `auth_token` is `Some`, include `Authorization: Bearer {token}`.
    async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
        auth_token: Option<&str>,
    ) -> Result<HttpResponse>;

    /// GET JSON from `url`. If `auth_token` is `Some`, include `Authorization: Bearer {token}`.
    async fn get_json(&self, url: &str, auth_token: Option<&str>) -> Result<HttpResponse>;
}

/// Real HTTP client using reqwest.
pub struct ReqwestSlackHttpClient {
    client: reqwest::Client,
}

impl ReqwestSlackHttpClient {
    pub fn new() -> Self {
        Self {
            client: claudear_core::tls::client_builder()
                .timeout(std::time::Duration::from_secs(30))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| claudear_core::tls::client()),
        }
    }
}

impl Default for ReqwestSlackHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SlackHttpClient for ReqwestSlackHttpClient {
    async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
        auth_token: Option<&str>,
    ) -> Result<HttpResponse> {
        let mut req = self.client.post(url).json(body);
        if let Some(token) = auth_token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
        let response = req.send().await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }

    async fn get_json(&self, url: &str, auth_token: Option<&str>) -> Result<HttpResponse> {
        let mut req = self.client.get(url);
        if let Some(token) = auth_token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
        let response = req.send().await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }
}

/// A Slack Block Kit message payload.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SlackMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) channel: Option<String>,
    pub(crate) text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blocks: Option<Vec<SlackBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thread_ts: Option<String>,
}

/// A single block in Block Kit.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub(crate) enum SlackBlock {
    #[serde(rename = "header")]
    Header { text: SlackText },
    #[serde(rename = "section")]
    Section {
        text: SlackText,
        #[serde(skip_serializing_if = "Option::is_none")]
        fields: Option<Vec<SlackText>>,
    },
    #[serde(rename = "context")]
    Context { elements: Vec<SlackText> },
    #[allow(dead_code)]
    #[serde(rename = "divider")]
    Divider,
}

/// A Block Kit text object.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SlackText {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) text: String,
}

impl SlackText {
    pub(crate) fn mrkdwn(text: impl Into<String>) -> Self {
        Self {
            kind: "mrkdwn".to_string(),
            text: text.into(),
        }
    }

    pub(crate) fn plain_text(text: impl Into<String>) -> Self {
        Self {
            kind: "plain_text".to_string(),
            text: text.into(),
        }
    }
}

/// Response from Slack Web API methods.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SlackApiResponse {
    pub(crate) ok: bool,
    #[serde(default)]
    pub(crate) error: Option<String>,
    /// Message timestamp (returned by chat.postMessage).
    #[serde(default)]
    pub(crate) ts: Option<String>,
    /// Messages returned by conversations.history / conversations.replies.
    #[serde(default)]
    pub(crate) messages: Option<Vec<SlackApiMessage>>,
}

/// A message object from Slack API.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SlackApiMessage {
    pub(crate) ts: String,
    #[serde(default)]
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) bot_id: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub(crate) thread_ts: Option<String>,
}

const SLACK_API_BASE: &str = "https://slack.com/api/";

/// Maximum lengths for user-controlled fields to prevent unbounded memory allocation.
const MAX_SHORT_ID_LENGTH: usize = 64;
const MAX_SOURCE_LENGTH: usize = 32;
const MAX_URL_LENGTH: usize = 2000;
const MAX_DESCRIPTION_LENGTH: usize = 2048;

/// Truncate a string to the specified maximum length, adding "..." if truncated.
fn truncate_string(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len > 3 {
        format!("{}...", &s[..s.floor_char_boundary(max_len - 3)])
    } else {
        s[..s.floor_char_boundary(max_len)].to_string()
    }
}

/// Convert a Slack message timestamp (e.g. `"1709123456.789012"`) to `DateTime<Utc>`.
pub(crate) fn slack_ts_to_datetime(ts: &str) -> Option<DateTime<Utc>> {
    let secs_f64: f64 = ts.parse().ok()?;
    let secs = secs_f64.trunc() as i64;
    let nanos = (secs_f64.fract().abs() * 1_000_000_000.0).min(999_999_999.0) as u32;
    DateTime::from_timestamp(secs, nanos)
}

/// Slack notifier supporting both Incoming Webhooks and Bot Token API.
pub struct SlackNotifier<H: SlackHttpClient = ReqwestSlackHttpClient> {
    config: SlackConfig,
    http: H,
    user_registry: UserRegistry,
}

impl SlackNotifier<ReqwestSlackHttpClient> {
    pub fn new(config: SlackConfig, user_registry: UserRegistry) -> Self {
        Self {
            config,
            http: ReqwestSlackHttpClient::new(),
            user_registry,
        }
    }
}

impl<H: SlackHttpClient> SlackNotifier<H> {
    /// Create a new Slack notifier with a custom HTTP client.
    pub fn with_http_client(config: SlackConfig, http: H) -> Self {
        Self {
            config,
            http,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
        }
    }

    /// Create a new Slack notifier with a custom HTTP client and user registry.
    pub fn with_http_client_and_registry(
        config: SlackConfig,
        http: H,
        user_registry: UserRegistry,
    ) -> Self {
        Self {
            config,
            http,
            user_registry,
        }
    }

    /// Send a notification message.
    ///
    /// Tries the webhook URL first (no channel required). Falls back to
    /// `chat.postMessage` with bot_token + channel_id.
    async fn send(&self, message: SlackMessage) -> Result<()> {
        // Webhook path
        if let Some(ref webhook_url) = self.config.webhook_url {
            let body = serde_json::json!({
                "text": message.text,
                "blocks": message.blocks,
            });
            let response = self
                .http
                .post_json(webhook_url.expose(), &body, None)
                .await?;

            if response.status < 200 || response.status >= 300 {
                return Err(Error::notifier(
                    "slack",
                    format!("Webhook error: {}", response.body),
                ));
            }
            return Ok(());
        }

        // Bot API path
        if let (Some(ref token), Some(ref channel_id)) =
            (&self.config.bot_token, &self.config.channel_id)
        {
            let token_str = token.expose();
            if !token_str.is_empty() && !channel_id.is_empty() {
                self.post_chat_message(token_str, channel_id, &message)
                    .await?;
                return Ok(());
            }
        }

        tracing::warn!("Slack send() called but no webhook_url or bot_token+channel_id configured, message dropped");
        Ok(())
    }

    /// Send a message via `chat.postMessage` and return the message `ts`.
    ///
    /// Used by `ask_question` and `send_to_channel` where we need the
    /// timestamp for threading.
    async fn send_to_channel(&self, message: SlackMessage) -> Result<Option<String>> {
        let token = match self.config.bot_token.as_ref() {
            Some(t) => {
                let exposed = t.expose();
                if !exposed.is_empty() {
                    exposed
                } else {
                    return Ok(None);
                }
            }
            _ => return Ok(None),
        };
        let channel_id = match self.config.channel_id.as_deref() {
            Some(c) if !c.is_empty() => c,
            _ => return Ok(None),
        };

        let ts = self.post_chat_message(token, channel_id, &message).await?;
        Ok(Some(ts))
    }

    /// POST to `chat.postMessage` and return the resulting `ts`.
    async fn post_chat_message(
        &self,
        token: &str,
        channel_id: &str,
        message: &SlackMessage,
    ) -> Result<String> {
        let url = format!("{}chat.postMessage", SLACK_API_BASE);
        let body = serde_json::json!({
            "channel": channel_id,
            "text": message.text,
            "blocks": message.blocks,
            "thread_ts": message.thread_ts,
        });
        let response = self.http.post_json(&url, &body, Some(token)).await?;

        if response.status < 200 || response.status >= 300 {
            return Err(Error::notifier(
                "slack",
                format!("chat.postMessage HTTP error: {}", response.body),
            ));
        }

        let api_resp: SlackApiResponse = serde_json::from_str(&response.body).map_err(|e| {
            Error::notifier("slack", format!("Failed to parse API response: {}", e))
        })?;

        if !api_resp.ok {
            return Err(Error::notifier(
                "slack",
                format!(
                    "chat.postMessage error: {}",
                    api_resp.error.unwrap_or_else(|| "unknown".to_string())
                ),
            ));
        }

        Ok(api_resp.ts.unwrap_or_default())
    }

    fn has_bot_channel(&self) -> bool {
        self.config
            .bot_token
            .as_ref()
            .map(|v| !v.expose().is_empty())
            .unwrap_or(false)
            && self
                .config
                .channel_id
                .as_ref()
                .map(|v| !v.is_empty())
                .unwrap_or(false)
    }

    fn get_user_mention(&self) -> Option<String> {
        self.config.user_id.as_ref().map(|id| format!("<@{}>", id))
    }

    fn get_user_mention_for_issue(&self, issue: &Issue) -> Option<String> {
        // Check for resolved user first
        if let Some(slug) = issue.get_metadata::<String>("resolved_user") {
            if let Some(user) = self.user_registry.get_by_slug(&slug) {
                if let Some(ref slack_id) = user.slack_id {
                    return Some(format!("<@{}>", slack_id));
                }
            }
        }
        // Fall back to global config
        self.config.user_id.as_ref().map(|id| format!("<@{}>", id))
    }

    fn get_target_slack_id_for_issue(&self, issue: &Issue) -> Option<String> {
        if let Some(slug) = issue.get_metadata::<String>("resolved_user") {
            if let Some(user) = self.user_registry.get_by_slug(&slug) {
                if let Some(ref slack_id) = user.slack_id {
                    return Some(slack_id.clone());
                }
            }
        }
        self.config.user_id.clone()
    }

    fn expected_reply_user_id(&self, request: &AskRequest) -> Option<String> {
        request
            .target_slack_id
            .clone()
            .or_else(|| self.config.user_id.clone())
    }

    fn extract_reply_text(content: &str) -> Option<String> {
        let answer = content.trim();
        if answer.is_empty() {
            None
        } else {
            Some(answer.to_string())
        }
    }
}

/// Return the current UTC timestamp in RFC 3339 format.
pub(crate) fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Build the Slack message for a "processing started" notification.
pub(crate) fn build_start_message(issue: &Issue, mention: Option<String>) -> SlackMessage {
    let emoji = get_source_emoji(&issue.source);
    let short_id = truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH);
    let title = truncate_string(&issue.title, MAX_DESCRIPTION_LENGTH);
    let url = truncate_string(&issue.url, MAX_URL_LENGTH);
    let source = truncate_string(&issue.source, MAX_SOURCE_LENGTH);

    let mut fallback = format!("{} Processing: {} - {}", emoji, short_id, title);
    if let Some(ref m) = mention {
        fallback = format!("{} {}", m, fallback);
    }

    let mut blocks = vec![
        SlackBlock::Header {
            text: SlackText::plain_text(format!("{} Processing: {}", emoji, short_id)),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("*<{}|{}>*", url, title)),
            fields: Some(vec![
                SlackText::mrkdwn(format!("*Source:* {}", source)),
                SlackText::mrkdwn(format!("*Priority:* {}", issue.priority)),
                SlackText::mrkdwn(format!("*Status:* {}", issue.status)),
            ]),
        },
    ];
    if let Some(reason) = issue.get_metadata::<String>("trigger_reason") {
        blocks.push(SlackBlock::Context {
            elements: vec![SlackText::mrkdwn(format!("*Trigger:* {}", reason))],
        });
    }
    if let Some(ref m) = mention {
        blocks.push(SlackBlock::Context {
            elements: vec![SlackText::mrkdwn(m.clone())],
        });
    }
    blocks.push(SlackBlock::Context {
        elements: vec![SlackText::mrkdwn(format!("Claudear | {}", timestamp()))],
    });

    SlackMessage {
        channel: None,
        text: fallback,
        blocks: Some(blocks),
        thread_ts: None,
    }
}

/// Build the Slack message for a "PR created" notification.
pub(crate) fn build_success_message(
    issue: &Issue,
    pr_url: &str,
    mention: Option<String>,
) -> SlackMessage {
    let emoji = get_source_emoji(&issue.source);
    let short_id = truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH);
    let title = truncate_string(&issue.title, MAX_DESCRIPTION_LENGTH);
    let issue_url = truncate_string(&issue.url, MAX_URL_LENGTH);
    let pr_url_truncated = truncate_string(pr_url, MAX_URL_LENGTH);
    let source = truncate_string(&issue.source, MAX_SOURCE_LENGTH);

    let is_update = issue.get_metadata::<bool>("is_pr_update").unwrap_or(false);
    let header_label = if is_update {
        "PR Updated"
    } else {
        "PR Created"
    };

    let mut fallback = format!(
        "\u{2705} {}: {} - {}",
        header_label, short_id, pr_url_truncated
    );
    if let Some(ref m) = mention {
        fallback = format!("{} {}", m, fallback);
    }

    let mut blocks = vec![
        SlackBlock::Header {
            text: SlackText::plain_text(format!("\u{2705} {}: {}", header_label, short_id)),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("*<{}|{}>*", pr_url_truncated, title)),
            fields: Some(vec![
                SlackText::mrkdwn(format!("*Source:* {} {}", emoji, source)),
                SlackText::mrkdwn(format!("*Issue:* <{}|{}>", issue_url, short_id)),
            ]),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("*PR Link:* <{}|View PR>", pr_url_truncated)),
            fields: None,
        },
    ];
    if let Some(changelog) = issue.get_metadata::<String>("changelog") {
        blocks.push(SlackBlock::Section {
            text: SlackText::mrkdwn(format!("*Changes:*\n{}", truncate_string(&changelog, 1000))),
            fields: None,
        });
    }
    if let Some(reason) = issue.get_metadata::<String>("trigger_reason") {
        blocks.push(SlackBlock::Context {
            elements: vec![SlackText::mrkdwn(format!("*Trigger:* {}", reason))],
        });
    }
    if let Some(confidence) = issue.get_metadata::<u8>("confidence") {
        let mut text = format!("*Fix Confidence:* {}/100", confidence);
        if let Some(reasoning) = issue.get_metadata::<String>("confidence_reasoning") {
            text.push_str(&format!(" — {}", truncate_string(&reasoning, 800)));
        }
        blocks.push(SlackBlock::Context {
            elements: vec![SlackText::mrkdwn(text)],
        });
    }
    if let Some(ref m) = mention {
        blocks.push(SlackBlock::Context {
            elements: vec![SlackText::mrkdwn(m.clone())],
        });
    }
    blocks.push(SlackBlock::Context {
        elements: vec![SlackText::mrkdwn(format!("Claudear | {}", timestamp()))],
    });

    SlackMessage {
        channel: None,
        text: fallback,
        blocks: Some(blocks),
        thread_ts: None,
    }
}

/// Build the Slack message for a "completed without PR" notification.
pub(crate) fn build_completed_message(issue: &Issue, mention: Option<String>) -> SlackMessage {
    let emoji = get_source_emoji(&issue.source);
    let short_id = truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH);
    let title = truncate_string(&issue.title, MAX_DESCRIPTION_LENGTH);
    let url = truncate_string(&issue.url, MAX_URL_LENGTH);
    let source = truncate_string(&issue.source, MAX_SOURCE_LENGTH);

    let mut fallback = format!("\u{2714}\u{FE0F} Completed: {} - {}", short_id, title);
    if let Some(ref m) = mention {
        fallback = format!("{} {}", m, fallback);
    }

    let reason = issue
        .get_metadata::<String>("completion_reason")
        .unwrap_or_else(|| "Claude completed but no PR URL was captured".to_string());
    let reason_display = truncate_string(&reason, 500);

    let mut blocks = vec![
        SlackBlock::Header {
            text: SlackText::plain_text(format!("\u{2714}\u{FE0F} Completed: {}", short_id)),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("*<{}|{}>*", url, title)),
            fields: Some(vec![
                SlackText::mrkdwn(format!("*Source:* {} {}", emoji, source)),
                SlackText::mrkdwn(format!("*Reason:* {}", reason_display)),
            ]),
        },
    ];
    if let Some(ref m) = mention {
        blocks.push(SlackBlock::Context {
            elements: vec![SlackText::mrkdwn(m.clone())],
        });
    }
    blocks.push(SlackBlock::Context {
        elements: vec![SlackText::mrkdwn(format!("Claudear | {}", timestamp()))],
    });

    SlackMessage {
        channel: None,
        text: fallback,
        blocks: Some(blocks),
        thread_ts: None,
    }
}

/// Build the Slack message for a "failed" notification.
pub(crate) fn build_failed_message(
    issue: &Issue,
    error: &str,
    mention: Option<String>,
) -> SlackMessage {
    let emoji = get_source_emoji(&issue.source);
    let short_id = truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH);
    let title = truncate_string(&issue.title, MAX_DESCRIPTION_LENGTH);
    let url = truncate_string(&issue.url, MAX_URL_LENGTH);
    let source = truncate_string(&issue.source, MAX_SOURCE_LENGTH);
    let error_display = truncate_string(error, 1000);

    let mut fallback = format!("\u{274C} Failed: {} - {}", short_id, error_display);
    if let Some(ref m) = mention {
        fallback = format!("{} {}", m, fallback);
    }

    let mut blocks = vec![
        SlackBlock::Header {
            text: SlackText::plain_text(format!("\u{274C} Failed: {}", short_id)),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("*<{}|{}>*", url, title)),
            fields: Some(vec![SlackText::mrkdwn(format!(
                "*Source:* {} {}",
                emoji, source
            ))]),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("*Error:*\n```{}```", error_display)),
            fields: None,
        },
    ];
    if let Some(reason) = issue.get_metadata::<String>("trigger_reason") {
        blocks.push(SlackBlock::Context {
            elements: vec![SlackText::mrkdwn(format!("*Trigger:* {}", reason))],
        });
    }
    if let Some(ref m) = mention {
        blocks.push(SlackBlock::Context {
            elements: vec![SlackText::mrkdwn(m.clone())],
        });
    }
    blocks.push(SlackBlock::Context {
        elements: vec![SlackText::mrkdwn(format!("Claudear | {}", timestamp()))],
    });

    SlackMessage {
        channel: None,
        text: fallback,
        blocks: Some(blocks),
        thread_ts: None,
    }
}

/// Build the Slack message for a status notification.
pub(crate) fn build_status_message(message: &str) -> SlackMessage {
    let message_truncated = truncate_string(message, MAX_DESCRIPTION_LENGTH);

    SlackMessage {
        channel: None,
        text: message_truncated.clone(),
        blocks: Some(vec![
            SlackBlock::Section {
                text: SlackText::mrkdwn(message_truncated),
                fields: None,
            },
            SlackBlock::Context {
                elements: vec![SlackText::mrkdwn(format!("Claudear | {}", timestamp()))],
            },
        ]),
        thread_ts: None,
    }
}

/// Build the Slack message for an "urgent issues" notification.
///
/// Returns `None` when the issue list is empty (nothing to send).
pub(crate) fn build_urgent_issues_message(
    issues: &[Issue],
    mention: Option<String>,
) -> Option<SlackMessage> {
    if issues.is_empty() {
        return None;
    }

    let fields: Vec<SlackText> = issues
        .iter()
        .take(10)
        .map(|issue| {
            let emoji = get_source_emoji(&issue.source);
            let short_id = truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH);
            let title = truncate_string(&issue.title, 50);
            let url = truncate_string(&issue.url, MAX_URL_LENGTH);
            SlackText::mrkdwn(format!("{} <{}|{} - {}>", emoji, url, short_id, title))
        })
        .collect();

    let header_text = format!(
        "\u{1F6A8} {} Urgent Issue{} Detected",
        issues.len(),
        if issues.len() > 1 { "s" } else { "" }
    );

    let mut fallback = header_text.clone();
    if let Some(ref m) = mention {
        fallback = format!("{} {}", m, fallback);
    }

    let mut blocks = vec![
        SlackBlock::Header {
            text: SlackText::plain_text(header_text),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn("The following issues require attention:".to_string()),
            fields: None,
        },
    ];

    // Add each issue as its own section for readability
    for field in &fields {
        blocks.push(SlackBlock::Section {
            text: field.clone(),
            fields: None,
        });
    }

    if let Some(ref m) = mention {
        blocks.push(SlackBlock::Context {
            elements: vec![SlackText::mrkdwn(m.clone())],
        });
    }
    blocks.push(SlackBlock::Context {
        elements: vec![SlackText::mrkdwn(format!("Claudear | {}", timestamp()))],
    });

    Some(SlackMessage {
        channel: None,
        text: fallback,
        blocks: Some(blocks),
        thread_ts: None,
    })
}

/// Build the Slack message for a "merged" notification.
pub(crate) fn build_merged_message(
    issue: &Issue,
    pr_url: &str,
    mention: Option<String>,
) -> SlackMessage {
    let emoji = get_source_emoji(&issue.source);
    let short_id = truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH);
    let pr_url_truncated = truncate_string(pr_url, MAX_URL_LENGTH);
    let source = truncate_string(&issue.source, MAX_SOURCE_LENGTH);

    let mut fallback = format!("\u{1F389} PR Merged: {} - {}", short_id, pr_url_truncated);
    if let Some(ref m) = mention {
        fallback = format!("{} {}", m, fallback);
    }

    let mut blocks = vec![
        SlackBlock::Header {
            text: SlackText::plain_text(format!(
                "\u{1F389} PR Merged & Issue Resolved: {}",
                short_id
            )),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!(
                "*Source:* {} {} | *PR:* <{}|View PR>",
                emoji, source, pr_url_truncated
            )),
            fields: None,
        },
    ];
    if let Some(ref m) = mention {
        blocks.push(SlackBlock::Context {
            elements: vec![SlackText::mrkdwn(m.clone())],
        });
    }
    blocks.push(SlackBlock::Context {
        elements: vec![SlackText::mrkdwn(format!("Claudear | {}", timestamp()))],
    });

    SlackMessage {
        channel: None,
        text: fallback,
        blocks: Some(blocks),
        thread_ts: None,
    }
}

/// Build the Slack message for a "PR closed" notification.
pub(crate) fn build_closed_message(
    issue: &Issue,
    pr_url: &str,
    mention: Option<String>,
) -> SlackMessage {
    let emoji = get_source_emoji(&issue.source);
    let short_id = truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH);
    let pr_url_truncated = truncate_string(pr_url, MAX_URL_LENGTH);

    let header = format!("{} PR Closed: {}", emoji, short_id);
    let mut blocks = vec![
        SlackBlock::Header {
            text: SlackText::plain_text(&header),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!(
                "*{}*\nPR was closed without merging",
                truncate_string(&issue.title, MAX_DESCRIPTION_LENGTH)
            )),
            fields: None,
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("<{}|View PR>", pr_url_truncated)),
            fields: None,
        },
    ];
    if let Some(ref m) = mention {
        blocks.insert(
            0,
            SlackBlock::Section {
                text: SlackText::mrkdwn(m.clone()),
                fields: None,
            },
        );
    }

    SlackMessage {
        channel: None,
        text: header,
        blocks: Some(blocks),
        thread_ts: None,
    }
}

/// Build the Slack message for a cascade PR success notification.
pub(crate) fn build_cascade_success_message(
    issue: &Issue,
    pr_url: &str,
    mention: Option<String>,
) -> SlackMessage {
    let upstream = issue
        .get_metadata::<String>("cascade_upstream_repo")
        .unwrap_or_default();
    let downstream = issue
        .get_metadata::<String>("cascade_downstream_repo")
        .unwrap_or_default();
    let pr_url_truncated = truncate_string(pr_url, MAX_URL_LENGTH);

    let header = format!(
        "\u{1F517} Cascade PR: {}",
        truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH)
    );
    let mut blocks = vec![
        SlackBlock::Header {
            text: SlackText::plain_text(&header),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("Downstream adaptation for *{}*", downstream)),
            fields: Some(vec![
                SlackText::mrkdwn(format!("*Upstream*\n{}", upstream)),
                SlackText::mrkdwn(format!("*Downstream*\n{}", downstream)),
            ]),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("<{}|View PR>", pr_url_truncated)),
            fields: None,
        },
    ];
    if let Some(ref m) = mention {
        blocks.insert(
            0,
            SlackBlock::Section {
                text: SlackText::mrkdwn(m.clone()),
                fields: None,
            },
        );
    }

    SlackMessage {
        channel: None,
        text: header,
        blocks: Some(blocks),
        thread_ts: None,
    }
}

/// Build the Slack message for a cascade PR failure notification.
pub(crate) fn build_cascade_failed_message(
    issue: &Issue,
    error: &str,
    mention: Option<String>,
) -> SlackMessage {
    let upstream = issue
        .get_metadata::<String>("cascade_upstream_repo")
        .unwrap_or_default();
    let downstream = issue
        .get_metadata::<String>("cascade_downstream_repo")
        .unwrap_or_default();

    let header = format!(
        "\u{26A0}\u{FE0F} Cascade Failed: {}",
        truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH)
    );
    let error_truncated = if error.len() > 500 {
        format!("{}...", &error[..error.floor_char_boundary(497)])
    } else {
        error.to_string()
    };
    let mut blocks = vec![
        SlackBlock::Header {
            text: SlackText::plain_text(&header),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("Failed to adapt *{}*", downstream)),
            fields: Some(vec![
                SlackText::mrkdwn(format!("*Upstream*\n{}", upstream)),
                SlackText::mrkdwn(format!("*Downstream*\n{}", downstream)),
            ]),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("*Error*\n```{}```", error_truncated)),
            fields: None,
        },
    ];
    if let Some(ref m) = mention {
        blocks.insert(
            0,
            SlackBlock::Section {
                text: SlackText::mrkdwn(m.clone()),
                fields: None,
            },
        );
    }

    SlackMessage {
        channel: None,
        text: header,
        blocks: Some(blocks),
        thread_ts: None,
    }
}

/// Build the Slack message for a regression detected notification.
pub(crate) fn build_regression_detected_message(
    issue: &Issue,
    error: &str,
    mention: Option<String>,
) -> SlackMessage {
    let emoji = get_source_emoji(&issue.source);
    let header = format!(
        "{} Regression Detected: {}",
        emoji,
        truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH)
    );
    let error_truncated = if error.len() > 500 {
        format!("{}...", &error[..error.floor_char_boundary(497)])
    } else {
        error.to_string()
    };
    let mut blocks = vec![
        SlackBlock::Header {
            text: SlackText::plain_text(&header),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn("A previously fixed issue has regressed".to_string()),
            fields: None,
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn(format!("*Details*\n{}", error_truncated)),
            fields: None,
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn("_Retry has been scheduled_".to_string()),
            fields: None,
        },
    ];
    if let Some(ref m) = mention {
        blocks.insert(
            0,
            SlackBlock::Section {
                text: SlackText::mrkdwn(m.clone()),
                fields: None,
            },
        );
    }

    SlackMessage {
        channel: None,
        text: header,
        blocks: Some(blocks),
        thread_ts: None,
    }
}

/// Build the Slack message for a regression resolved notification.
pub(crate) fn build_regression_resolved_message(
    issue: &Issue,
    mention: Option<String>,
) -> SlackMessage {
    let emoji = get_source_emoji(&issue.source);
    let header = format!(
        "{} Regression Resolved: {}",
        emoji,
        truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH)
    );
    let mut blocks = vec![
        SlackBlock::Header {
            text: SlackText::plain_text(&header),
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn("No regression detected after monitoring period".to_string()),
            fields: None,
        },
        SlackBlock::Section {
            text: SlackText::mrkdwn("_Issue resolved after final check_".to_string()),
            fields: None,
        },
    ];
    if let Some(ref m) = mention {
        blocks.insert(
            0,
            SlackBlock::Section {
                text: SlackText::mrkdwn(m.clone()),
                fields: None,
            },
        );
    }

    SlackMessage {
        channel: None,
        text: header,
        blocks: Some(blocks),
        thread_ts: None,
    }
}

/// Build the Slack message for a scheduled report.
pub(crate) fn build_report_message(report: &Report) -> SlackMessage {
    let text = report.format_text();
    let truncated = truncate_string(&text, MAX_DESCRIPTION_LENGTH);

    SlackMessage {
        channel: None,
        text: truncated.clone(),
        blocks: Some(vec![
            SlackBlock::Header {
                text: SlackText::plain_text(format!(
                    "\u{1F4CA} Claudear Report: {}",
                    report.period
                )),
            },
            SlackBlock::Section {
                text: SlackText::mrkdwn(format!(
                    "*Issues:* {} attempted, {} succeeded, {} failed\n*PRs:* {} created, {} merged\n*Success Rate:* {:.1}%",
                    report.issues_attempted,
                    report.issues_succeeded,
                    report.issues_failed,
                    report.prs_created,
                    report.prs_merged,
                    report.success_rate,
                )),
                fields: None,
            },
            SlackBlock::Context {
                elements: vec![SlackText::mrkdwn(format!("Claudear | {}", timestamp()))],
            },
        ]),
        thread_ts: None,
    }
}

/// Build the Slack message for a human-in-the-loop question.
pub(crate) fn build_ask_question_message(
    issue: &Issue,
    request: &AskRequest,
    mention: Option<String>,
) -> SlackMessage {
    let short_id = truncate_string(&issue.short_id, MAX_SHORT_ID_LENGTH);

    let mut text = String::new();
    if let Some(ref m) = mention {
        text.push_str(m);
        text.push(' ');
    }
    text.push_str(&format!(
        "Human input needed for {}:\n{}",
        short_id, request.question.question
    ));
    if let Some(ref why) = request.question.why {
        text.push_str(&format!("\nWhy: {}", why));
    }
    if let Some(ref ctx) = request.question.context {
        text.push_str(&format!("\nContext: {}", truncate_string(ctx, 400)));
    }
    if !request.question.options.is_empty() {
        text.push_str(&format!(
            "\nOptions: {}",
            request.question.options.join(" | ")
        ));
    }
    text.push_str("\nReply in this thread with your answer.");

    SlackMessage {
        channel: None,
        text: text.clone(),
        blocks: None,
        thread_ts: None,
    }
}

#[async_trait]
impl<H: SlackHttpClient + 'static> Notifier for SlackNotifier<H> {
    fn name(&self) -> &str {
        "slack"
    }

    fn is_enabled(&self) -> bool {
        self.config.webhook_url.is_some() || self.has_bot_channel()
    }

    async fn notify_start(&self, issue: &Issue) -> Result<()> {
        let mention = self.get_user_mention_for_issue(issue);
        self.send(build_start_message(issue, mention)).await
    }

    async fn notify_success(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        let mention = self.get_user_mention_for_issue(issue);
        if issue
            .get_metadata::<String>("cascade_downstream_repo")
            .is_some()
        {
            self.send(build_cascade_success_message(issue, pr_url, mention))
                .await
        } else {
            self.send(build_success_message(issue, pr_url, mention))
                .await
        }
    }

    async fn notify_completed(&self, issue: &Issue) -> Result<()> {
        let mention = self.get_user_mention_for_issue(issue);
        if issue
            .get_metadata::<bool>("regression_resolved")
            .unwrap_or(false)
        {
            self.send(build_regression_resolved_message(issue, mention))
                .await
        } else {
            self.send(build_completed_message(issue, mention)).await
        }
    }

    async fn notify_failed(&self, issue: &Issue, error: &str) -> Result<()> {
        let mention = self.get_user_mention_for_issue(issue);
        if issue
            .get_metadata::<bool>("regression_detected")
            .unwrap_or(false)
        {
            self.send(build_regression_detected_message(issue, error, mention))
                .await
        } else if issue
            .get_metadata::<String>("cascade_downstream_repo")
            .is_some()
        {
            self.send(build_cascade_failed_message(issue, error, mention))
                .await
        } else {
            self.send(build_failed_message(issue, error, mention)).await
        }
    }

    async fn notify_status(&self, message: &str) -> Result<()> {
        self.send(build_status_message(message)).await
    }

    async fn notify_urgent_issues(&self, issues: &[Issue]) -> Result<()> {
        let mention = self.get_user_mention();
        match build_urgent_issues_message(issues, mention) {
            Some(message) => self.send(message).await,
            None => Ok(()),
        }
    }

    async fn notify_merged(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        let mention = self.get_user_mention_for_issue(issue);
        self.send(build_merged_message(issue, pr_url, mention))
            .await
    }

    async fn notify_closed(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        let mention = self.get_user_mention_for_issue(issue);
        self.send(build_closed_message(issue, pr_url, mention))
            .await
    }

    async fn notify_report(&self, report: &Report) -> Result<()> {
        self.send(build_report_message(report)).await
    }

    async fn ask_question(
        &self,
        issue: &Issue,
        request: &AskRequest,
    ) -> Result<Option<AskDelivery>> {
        let mention = self.get_user_mention_for_issue(issue);
        let message = build_ask_question_message(issue, request, mention);

        // Always use chat.postMessage for Q&A so we get the ts for threading.
        let ts = self.send_to_channel(message).await?;

        Ok(Some(AskDelivery {
            channel: "slack".to_string(),
            target: self.get_target_slack_id_for_issue(issue),
            message_id: ts,
        }))
    }

    async fn poll_question_replies(
        &self,
        request: &AskRequest,
        since: DateTime<Utc>,
    ) -> Result<Vec<AskReply>> {
        let token_str = match self.config.bot_token.as_ref() {
            Some(t) => {
                let exposed = t.expose();
                if !exposed.is_empty() {
                    exposed
                } else {
                    return Ok(Vec::new());
                }
            }
            _ => return Ok(Vec::new()),
        };
        let channel_id = match self.config.channel_id.as_deref() {
            Some(c) if !c.is_empty() => c,
            _ => return Ok(Vec::new()),
        };

        let expected_user = self.expected_reply_user_id(request);

        // Identify question messages by "Human input needed for {short_id}" text + bot_id,
        // matching Discord's approach of using message content patterns instead of correlation tokens.
        let ask_prefix = format!("Human input needed for {}", request.short_id);

        // Step 1: Fetch recent channel history to find our question messages.
        let since_ts = format!("{}.000000", since.timestamp());
        let history_url = format!(
            "{}conversations.history?channel={}&oldest={}&limit=50",
            SLACK_API_BASE,
            urlencoding::encode(channel_id),
            urlencoding::encode(&since_ts)
        );
        let history_resp = self.http.get_json(&history_url, Some(token_str)).await?;
        let history: SlackApiResponse =
            serde_json::from_str(&history_resp.body).unwrap_or(SlackApiResponse {
                ok: false,
                error: None,
                ts: None,
                messages: None,
            });

        if !history.ok {
            tracing::warn!("Slack conversations.history failed: {:?}", history.error);
            return Ok(Vec::new());
        }

        let messages = history.messages.unwrap_or_default();

        // Find question messages by text pattern (bot messages containing our ask prefix).
        let question_messages: Vec<&SlackApiMessage> = messages
            .iter()
            .filter(|m| m.text.contains(&ask_prefix) && m.bot_id.is_some())
            .collect();

        let mut replies: Vec<AskReply> = Vec::new();

        // Step 2: For each question message, fetch its thread replies.
        for qm in &question_messages {
            let replies_url = format!(
                "{}conversations.replies?channel={}&ts={}&oldest={}",
                SLACK_API_BASE,
                urlencoding::encode(channel_id),
                urlencoding::encode(&qm.ts),
                urlencoding::encode(&since_ts)
            );
            let replies_resp = self.http.get_json(&replies_url, Some(token_str)).await?;
            let thread: SlackApiResponse =
                serde_json::from_str(&replies_resp.body).unwrap_or(SlackApiResponse {
                    ok: false,
                    error: None,
                    ts: None,
                    messages: None,
                });

            if !thread.ok {
                continue;
            }

            let thread_messages = thread.messages.unwrap_or_default();

            for tm in thread_messages {
                // Skip the parent message itself.
                if tm.ts == qm.ts {
                    continue;
                }

                let user_id = match tm.user {
                    Some(ref u) => u.clone(),
                    None => continue,
                };

                // Filter by expected user if configured; otherwise skip bot messages
                // to avoid processing our own notifications as replies.
                match expected_user {
                    Some(ref expected) => {
                        if &user_id != expected {
                            continue;
                        }
                    }
                    None => {
                        if tm.bot_id.is_some() {
                            continue;
                        }
                    }
                }

                let parsed_time = match slack_ts_to_datetime(&tm.ts) {
                    Some(dt) => dt,
                    None => continue,
                };

                if parsed_time < since {
                    continue;
                }

                let answer = match Self::extract_reply_text(&tm.text) {
                    Some(a) => a,
                    None => continue,
                };

                replies.push(AskReply {
                    correlation_id: request.correlation_id.clone(),
                    channel: "slack".to_string(),
                    responder: Some(user_id),
                    answer,
                    replied_at: parsed_time,
                });
            }
        }

        replies.sort_by_key(|r| r.replied_at);
        Ok(replies)
    }

    fn supports_replies(&self) -> bool {
        self.has_bot_channel()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn empty_registry() -> UserRegistry {
        UserRegistry::new(std::collections::HashMap::new())
    }

    /// Mock Slack HTTP client that records calls and returns configurable responses.
    struct MockSlackHttpClient {
        response_status: u16,
        response_body: String,
        call_count: AtomicUsize,
        last_post_calls: Mutex<Vec<(String, serde_json::Value, Option<String>)>>,
        last_get_calls: Mutex<Vec<(String, Option<String>)>>,
        /// Optional per-URL response overrides (url prefix -> response body).
        get_responses: Mutex<Vec<(String, String)>>,
    }

    impl MockSlackHttpClient {
        fn new(status: u16, body: &str) -> Self {
            Self {
                response_status: status,
                response_body: body.to_string(),
                call_count: AtomicUsize::new(0),
                last_post_calls: Mutex::new(Vec::new()),
                last_get_calls: Mutex::new(Vec::new()),
                get_responses: Mutex::new(Vec::new()),
            }
        }

        fn success() -> Self {
            // Slack webhook returns "ok" as plain text on success.
            Self::new(200, "ok")
        }

        fn success_api() -> Self {
            Self::new(200, r#"{"ok":true,"ts":"1709123456.789012"}"#)
        }

        fn error(status: u16, body: &str) -> Self {
            Self::new(status, body)
        }

        fn get_call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }

        fn get_last_post_call(&self) -> Option<(String, serde_json::Value, Option<String>)> {
            self.last_post_calls.lock().unwrap().last().cloned()
        }

        #[expect(dead_code)]
        fn get_last_get_call(&self) -> Option<(String, Option<String>)> {
            self.last_get_calls.lock().unwrap().last().cloned()
        }

        fn add_get_response(&self, url_prefix: &str, body: &str) {
            self.get_responses
                .lock()
                .unwrap()
                .push((url_prefix.to_string(), body.to_string()));
        }
    }

    #[async_trait]
    impl SlackHttpClient for MockSlackHttpClient {
        async fn post_json(
            &self,
            url: &str,
            body: &serde_json::Value,
            auth_token: Option<&str>,
        ) -> Result<HttpResponse> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.last_post_calls.lock().unwrap().push((
                url.to_string(),
                body.clone(),
                auth_token.map(|s| s.to_string()),
            ));

            Ok(HttpResponse {
                status: self.response_status,
                body: self.response_body.clone(),
            })
        }

        async fn get_json(&self, url: &str, auth_token: Option<&str>) -> Result<HttpResponse> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.last_get_calls
                .lock()
                .unwrap()
                .push((url.to_string(), auth_token.map(|s| s.to_string())));

            // Check for URL-specific responses.
            let responses = self.get_responses.lock().unwrap();
            for (prefix, body) in responses.iter() {
                if url.starts_with(prefix) || url.contains(prefix) {
                    return Ok(HttpResponse {
                        status: self.response_status,
                        body: body.clone(),
                    });
                }
            }

            Ok(HttpResponse {
                status: self.response_status,
                body: self.response_body.clone(),
            })
        }
    }

    fn webhook_config() -> SlackConfig {
        SlackConfig {
            webhook_url: Some("https://hooks.slack.com/services/T00/B00/xxx".into()),
            ..Default::default()
        }
    }

    fn webhook_config_with_user() -> SlackConfig {
        SlackConfig {
            webhook_url: Some("https://hooks.slack.com/services/T00/B00/xxx".into()),
            user_id: Some("U987654321".to_string()),
            ..Default::default()
        }
    }

    fn bot_config() -> SlackConfig {
        SlackConfig {
            bot_token: Some("xoxb-test-token".into()),
            channel_id: Some("C12345678".to_string()),
            ..Default::default()
        }
    }

    fn bot_config_with_user() -> SlackConfig {
        SlackConfig {
            bot_token: Some("xoxb-test-token".into()),
            channel_id: Some("C12345678".to_string()),
            user_id: Some("U987654321".to_string()),
            ..Default::default()
        }
    }

    fn test_issue() -> Issue {
        Issue::new(
            "123",
            "PROJ-123",
            "Test Issue Title",
            "https://example.com/issue/123",
            "linear",
        )
    }

    fn make_ask_request(
        correlation_id: &str,
        question: &str,
        context: Option<&str>,
        options: Vec<&str>,
        why: Option<&str>,
        target_slack_id: Option<&str>,
    ) -> AskRequest {
        AskRequest {
            correlation_id: correlation_id.to_string(),
            source: "linear".to_string(),
            repo: None,
            issue_id: "1".to_string(),
            short_id: "LIN-1".to_string(),
            question: claudear_core::types::BlockingQuestion {
                question: question.to_string(),
                context: context.map(|s| s.to_string()),
                options: options.into_iter().map(|s| s.to_string()).collect(),
                why: why.map(|s| s.to_string()),
            },
            asked_at: chrono::Utc::now(),
            target_discord_id: None,
            target_email: None,
            target_slack_id: target_slack_id.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_notifier_name() {
        let config = SlackConfig::default();
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        assert_eq!(notifier.name(), "slack");
    }

    #[test]
    fn test_is_enabled_with_webhook() {
        let notifier =
            SlackNotifier::with_http_client(webhook_config(), MockSlackHttpClient::success());
        assert!(notifier.is_enabled());
    }

    #[test]
    fn test_is_enabled_with_bot_channel() {
        let notifier =
            SlackNotifier::with_http_client(bot_config(), MockSlackHttpClient::success_api());
        assert!(notifier.is_enabled());
    }

    #[test]
    fn test_is_enabled_false_when_nothing_configured() {
        let config = SlackConfig::default();
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        assert!(!notifier.is_enabled());
    }

    #[test]
    fn test_is_enabled_false_with_only_bot_token() {
        let config = SlackConfig {
            bot_token: Some("xoxb-token".into()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        assert!(!notifier.is_enabled());
    }

    #[test]
    fn test_is_enabled_false_with_only_channel_id() {
        let config = SlackConfig {
            channel_id: Some("C12345678".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        assert!(!notifier.is_enabled());
    }

    #[test]
    fn test_is_enabled_false_with_empty_bot_token() {
        let config = SlackConfig {
            bot_token: Some("".into()),
            channel_id: Some("C12345678".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        assert!(!notifier.is_enabled());
    }

    #[test]
    fn test_is_enabled_false_with_empty_channel_id() {
        let config = SlackConfig {
            bot_token: Some("xoxb-token".into()),
            channel_id: Some("".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        assert!(!notifier.is_enabled());
    }

    #[tokio::test]
    async fn test_send_webhook_success() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let result = notifier.notify_start(&test_issue()).await;
        assert!(result.is_ok());
        assert_eq!(notifier.http.get_call_count(), 1);
    }

    #[tokio::test]
    async fn test_send_webhook_sends_to_correct_url() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        notifier.notify_start(&test_issue()).await.unwrap();
        let (url, _, auth) = notifier.http.get_last_post_call().unwrap();
        assert_eq!(url, "https://hooks.slack.com/services/T00/B00/xxx");
        assert!(
            auth.is_none(),
            "Webhook calls should not include auth token"
        );
    }

    #[tokio::test]
    async fn test_send_webhook_error_response() {
        let mock = MockSlackHttpClient::error(400, "invalid_payload");
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let result = notifier.notify_start(&test_issue()).await;
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("Webhook error"));
    }

    #[tokio::test]
    async fn test_send_webhook_server_error() {
        let mock = MockSlackHttpClient::error(500, "Internal Server Error");
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let result = notifier.notify_status("Test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_bot_api_success() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let result = notifier.notify_start(&test_issue()).await;
        assert!(result.is_ok());
        assert_eq!(notifier.http.get_call_count(), 1);
    }

    #[tokio::test]
    async fn test_send_bot_api_includes_auth_token() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        notifier.notify_start(&test_issue()).await.unwrap();
        let (url, body, auth) = notifier.http.get_last_post_call().unwrap();
        assert!(url.contains("chat.postMessage"));
        assert_eq!(auth.as_deref(), Some("xoxb-test-token"));
        assert_eq!(body["channel"], "C12345678");
    }

    #[tokio::test]
    async fn test_send_bot_api_error_response() {
        let mock = MockSlackHttpClient::new(200, r#"{"ok":false,"error":"channel_not_found"}"#);
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let result = notifier.notify_start(&test_issue()).await;
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("channel_not_found"));
    }

    #[tokio::test]
    async fn test_notify_start_sends_blocks() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        notifier.notify_start(&test_issue()).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        assert!(body["blocks"].is_array());
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("PROJ-123"));
        assert!(text.contains("Processing"));
    }

    #[tokio::test]
    async fn test_notify_start_with_user_mention() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config_with_user(), mock);
        notifier.notify_start(&test_issue()).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("<@U987654321>"));
    }

    #[tokio::test]
    async fn test_notify_success_sends_correct_content() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        notifier
            .notify_success(&test_issue(), "https://github.com/org/repo/pull/42")
            .await
            .unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("PR Created"));
        assert!(text.contains("PROJ-123"));
    }

    #[tokio::test]
    async fn test_notify_completed_sends_correct_content() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        notifier.notify_completed(&test_issue()).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Completed"));
    }

    #[tokio::test]
    async fn test_notify_failed_sends_correct_content() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        notifier
            .notify_failed(&test_issue(), "Build failed with exit code 1")
            .await
            .unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Failed"));
        assert!(text.contains("PROJ-123"));
    }

    #[tokio::test]
    async fn test_notify_failed_truncates_long_error() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let long_error = "x".repeat(2000);
        notifier
            .notify_failed(&test_issue(), &long_error)
            .await
            .unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        // The blocks should contain a truncated error
        let blocks = body["blocks"].as_array().unwrap();
        let error_block = blocks
            .iter()
            .find(|b| {
                b.get("text")
                    .and_then(|t| t.get("text"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.contains("Error"))
                    .unwrap_or(false)
            })
            .unwrap();
        let error_text = error_block["text"]["text"].as_str().unwrap();
        assert!(error_text.contains("..."));
        // Error display should be truncated to ~1000 chars (within the code block markers)
        assert!(error_text.len() <= 1100);
    }

    #[tokio::test]
    async fn test_notify_status_sends_content() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        notifier.notify_status("System is healthy").await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert_eq!(text, "System is healthy");
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_empty() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let result = notifier.notify_urgent_issues(&[]).await;
        assert!(result.is_ok());
        // No call should have been made
        assert_eq!(notifier.http.get_call_count(), 0);
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_sends_content() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let issues = vec![
            Issue::new("1", "PROJ-1", "Issue 1", "https://example.com/1", "linear"),
            Issue::new("2", "PROJ-2", "Issue 2", "https://example.com/2", "sentry"),
        ];
        notifier.notify_urgent_issues(&issues).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("2 Urgent Issues"));
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_single_item_grammar() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let issues = vec![Issue::new(
            "1",
            "PROJ-1",
            "Issue",
            "https://example.com",
            "linear",
        )];
        notifier.notify_urgent_issues(&issues).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("1 Urgent Issue Detected"));
        assert!(!text.contains("Issues"));
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_with_user_mention() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config_with_user(), mock);
        let issues = vec![Issue::new(
            "1",
            "PROJ-1",
            "Issue",
            "https://example.com",
            "linear",
        )];
        notifier.notify_urgent_issues(&issues).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("<@U987654321>"));
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_limits_to_ten() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let issues: Vec<Issue> = (1..=20)
            .map(|i| {
                Issue::new(
                    i.to_string(),
                    format!("PROJ-{}", i),
                    format!("Issue {}", i),
                    format!("https://example.com/{}", i),
                    "linear",
                )
            })
            .collect();
        notifier.notify_urgent_issues(&issues).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let blocks = body["blocks"].as_array().unwrap();
        // Count section blocks that are issue items (after header + description).
        let issue_blocks: Vec<_> = blocks
            .iter()
            .filter(|b| {
                b.get("type").and_then(|t| t.as_str()) == Some("section")
                    && b.get("text")
                        .and_then(|t| t.get("text"))
                        .and_then(|t| t.as_str())
                        .map(|s| s.contains("PROJ-"))
                        .unwrap_or(false)
            })
            .collect();
        assert!(issue_blocks.len() <= 10);
    }

    #[tokio::test]
    async fn test_notify_status_disabled() {
        let config = SlackConfig::default();
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        let result = notifier.notify_status("Test status").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_start_disabled() {
        let config = SlackConfig::default();
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        let result = notifier.notify_start(&test_issue()).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_user_mention() {
        let notifier = SlackNotifier::with_http_client(
            webhook_config_with_user(),
            MockSlackHttpClient::success(),
        );
        assert_eq!(
            notifier.get_user_mention(),
            Some("<@U987654321>".to_string())
        );
    }

    #[test]
    fn test_user_mention_none() {
        let notifier =
            SlackNotifier::with_http_client(webhook_config(), MockSlackHttpClient::success());
        assert_eq!(notifier.get_user_mention(), None);
    }

    #[tokio::test]
    async fn test_notify_start_with_resolved_user_mention() {
        let mock = MockSlackHttpClient::success();
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                slack_id: Some("U111222333".to_string()),
                ..Default::default()
            },
        );
        let registry = UserRegistry::new(users);
        let config = SlackConfig {
            webhook_url: Some("https://hooks.slack.com/services/T00/B00/xxx".into()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client_and_registry(config, mock, registry);
        let mut issue = test_issue();
        issue.set_metadata("resolved_user", "jake");
        notifier.notify_start(&issue).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("<@U111222333>"));
    }

    #[tokio::test]
    async fn test_resolved_user_overrides_global_user_id() {
        let mock = MockSlackHttpClient::success();
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                slack_id: Some("U111222333".to_string()),
                ..Default::default()
            },
        );
        let registry = UserRegistry::new(users);
        let config = SlackConfig {
            webhook_url: Some("https://hooks.slack.com/services/T00/B00/xxx".into()),
            user_id: Some("U999999999".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client_and_registry(config, mock, registry);
        let mut issue = test_issue();
        issue.set_metadata("resolved_user", "jake");
        notifier.notify_start(&issue).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("<@U111222333>"));
        assert!(!text.contains("<@U999999999>"));
    }

    #[tokio::test]
    async fn test_fallback_to_global_when_no_resolved_user() {
        let mock = MockSlackHttpClient::success();
        let registry = empty_registry();
        let config = SlackConfig {
            webhook_url: Some("https://hooks.slack.com/services/T00/B00/xxx".into()),
            user_id: Some("U999999999".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client_and_registry(config, mock, registry);
        notifier.notify_start(&test_issue()).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("<@U999999999>"));
    }

    #[test]
    fn test_get_user_mention_for_issue_no_resolved_user_no_global() {
        let config = SlackConfig {
            webhook_url: Some("https://hooks.slack.com/services/T00/B00/xxx".into()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        let issue = test_issue();
        assert_eq!(notifier.get_user_mention_for_issue(&issue), None);
    }

    #[test]
    fn test_get_user_mention_for_issue_resolved_user_no_slack_id() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                slack_id: None,
                ..Default::default()
            },
        );
        let registry = UserRegistry::new(users);
        let config = SlackConfig {
            webhook_url: Some("https://hooks.slack.com/services/T00/B00/xxx".into()),
            user_id: Some("Ufallback".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client_and_registry(
            config,
            MockSlackHttpClient::success(),
            registry,
        );
        let mut issue = test_issue();
        issue.set_metadata("resolved_user", "jake");
        assert_eq!(
            notifier.get_user_mention_for_issue(&issue),
            Some("<@Ufallback>".to_string())
        );
    }

    #[tokio::test]
    async fn test_ask_question_sends_via_chat_post_message() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config_with_user(), mock);
        let issue = test_issue();
        let request = make_ask_request("tok-1", "Choose target branch?", None, vec![], None, None);
        let delivery = notifier
            .ask_question(&issue, &request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.channel, "slack");
        assert_eq!(delivery.message_id.as_deref(), Some("1709123456.789012"));
    }

    #[tokio::test]
    async fn test_ask_question_includes_correlation_token() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let issue = test_issue();
        let request = make_ask_request(
            "tok-abc",
            "Pick a branch",
            None,
            vec!["main", "develop"],
            None,
            None,
        );
        notifier.ask_question(&issue, &request).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(!text.contains("[CLAUDEAR-Q:"));
        assert!(text.contains("Human input needed for"));
        assert!(text.contains("Pick a branch"));
        assert!(text.contains("main | develop"));
    }

    #[tokio::test]
    async fn test_ask_question_includes_options_and_context() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let issue = test_issue();
        let request = make_ask_request(
            "tok-opts",
            "Pick a branch",
            Some("We need a target for the PR"),
            vec!["main", "develop"],
            Some("Multiple branches available"),
            None,
        );
        notifier.ask_question(&issue, &request).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(!text.contains("[CLAUDEAR-Q:"));
        assert!(text.contains("Human input needed for"));
        assert!(text.contains("Pick a branch"));
        assert!(text.contains("Why: Multiple branches available"));
        assert!(text.contains("Context: We need a target for the PR"));
        assert!(text.contains("main | develop"));
    }

    #[tokio::test]
    async fn test_ask_question_uses_resolved_user_target() {
        let mock = MockSlackHttpClient::success_api();
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                slack_id: Some("U111222333".to_string()),
                ..Default::default()
            },
        );
        let registry = UserRegistry::new(users);
        let config = SlackConfig {
            bot_token: Some("xoxb-test-token".into()),
            channel_id: Some("C12345678".to_string()),
            user_id: Some("U999999999".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client_and_registry(config, mock, registry);
        let mut issue = test_issue();
        issue.set_metadata("resolved_user", "jake");
        let request = make_ask_request("tok-1", "Question?", None, vec![], None, None);
        let delivery = notifier
            .ask_question(&issue, &request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.target.as_deref(), Some("U111222333"));
    }

    #[tokio::test]
    async fn test_ask_question_falls_back_to_global_target() {
        let mock = MockSlackHttpClient::success_api();
        let config = SlackConfig {
            bot_token: Some("xoxb-test-token".into()),
            channel_id: Some("C12345678".to_string()),
            user_id: Some("U999999999".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client_and_registry(config, mock, empty_registry());
        let issue = test_issue();
        let request = make_ask_request("tok-2", "Question?", None, vec![], None, None);
        let delivery = notifier
            .ask_question(&issue, &request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.target.as_deref(), Some("U999999999"));
    }

    #[tokio::test]
    async fn test_poll_question_replies_basic() {
        let mock = MockSlackHttpClient::success_api();
        let since = Utc::now() - chrono::Duration::minutes(5);

        // History response: one question message from our bot.
        let history_body = serde_json::json!({
            "ok": true,
            "messages": [
                {
                    "ts": "1709123456.000000",
                    "text": "Human input needed for LIN-1:\nPick a branch",
                    "bot_id": "B12345",
                    "user": null
                }
            ]
        });
        // Replies response: one human reply in the thread.
        let reply_ts_f = (Utc::now().timestamp() as f64) + 10.0;
        let reply_ts = format!("{:.6}", reply_ts_f);
        let replies_body = serde_json::json!({
            "ok": true,
            "messages": [
                {
                    "ts": "1709123456.000000",
                    "text": "Human input needed for LIN-1",
                    "bot_id": "B12345"
                },
                {
                    "ts": reply_ts,
                    "text": "Use main branch",
                    "user": "U987654321"
                }
            ]
        });

        mock.add_get_response("conversations.history", &history_body.to_string());
        mock.add_get_response("conversations.replies", &replies_body.to_string());

        let notifier = SlackNotifier::with_http_client(bot_config_with_user(), mock);
        let request = make_ask_request("corr-1", "Pick a branch", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, since)
            .await
            .unwrap();

        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].answer, "Use main branch");
        assert_eq!(replies[0].channel, "slack");
        assert_eq!(replies[0].responder.as_deref(), Some("U987654321"));
        assert_eq!(replies[0].correlation_id, "corr-1");
    }

    #[tokio::test]
    async fn test_poll_question_replies_filters_by_user() {
        let mock = MockSlackHttpClient::success_api();
        let since = Utc::now() - chrono::Duration::minutes(5);

        let history_body = serde_json::json!({
            "ok": true,
            "messages": [
                {
                    "ts": "1709123456.000000",
                    "text": "Human input needed for LIN-1:\nQuestion",
                    "bot_id": "B12345"
                }
            ]
        });
        let reply_ts_f = (Utc::now().timestamp() as f64) + 10.0;
        let reply_ts = format!("{:.6}", reply_ts_f);
        let replies_body = serde_json::json!({
            "ok": true,
            "messages": [
                {
                    "ts": "1709123456.000000",
                    "text": "Human input needed for LIN-1:\nQuestion",
                    "bot_id": "B12345"
                },
                {
                    "ts": reply_ts,
                    "text": "Wrong user reply",
                    "user": "U_OTHER"
                }
            ]
        });

        mock.add_get_response("conversations.history", &history_body.to_string());
        mock.add_get_response("conversations.replies", &replies_body.to_string());

        let notifier = SlackNotifier::with_http_client(bot_config_with_user(), mock);
        let request = make_ask_request(
            "corr-2",
            "Question?",
            None,
            vec![],
            None,
            Some("U987654321"),
        );
        let replies = notifier
            .poll_question_replies(&request, since)
            .await
            .unwrap();

        // Should be empty because the reply is from a different user.
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn test_poll_returns_empty_without_bot_token() {
        let config = SlackConfig {
            webhook_url: Some("https://hooks.slack.com/services/T00/B00/xxx".into()),
            ..Default::default()
        };
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(config, mock);
        let request = make_ask_request("corr-3", "Q?", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, Utc::now())
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[test]
    fn test_supports_replies_true_when_both_set() {
        let notifier =
            SlackNotifier::with_http_client(bot_config(), MockSlackHttpClient::success());
        assert!(notifier.supports_replies());
    }

    #[test]
    fn test_supports_replies_false_when_no_bot_token() {
        let config = SlackConfig {
            webhook_url: Some("https://hooks.slack.com/services/T00/B00/xxx".into()),
            channel_id: Some("C12345678".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        assert!(!notifier.supports_replies());
    }

    #[test]
    fn test_supports_replies_false_when_no_channel_id() {
        let config = SlackConfig {
            bot_token: Some("xoxb-token".into()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        assert!(!notifier.supports_replies());
    }

    #[test]
    fn test_expected_reply_user_id_prefers_request_target() {
        let notifier =
            SlackNotifier::with_http_client(bot_config_with_user(), MockSlackHttpClient::success());
        let request = make_ask_request("tok-1", "Q?", None, vec![], None, Some("U_REQUEST_TARGET"));
        assert_eq!(
            notifier.expected_reply_user_id(&request),
            Some("U_REQUEST_TARGET".to_string())
        );
    }

    #[test]
    fn test_expected_reply_user_id_falls_back_to_config() {
        let notifier =
            SlackNotifier::with_http_client(bot_config_with_user(), MockSlackHttpClient::success());
        let request = make_ask_request("tok-2", "Q?", None, vec![], None, None);
        assert_eq!(
            notifier.expected_reply_user_id(&request),
            Some("U987654321".to_string())
        );
    }

    #[test]
    fn test_expected_reply_user_id_none_when_both_absent() {
        let notifier =
            SlackNotifier::with_http_client(bot_config(), MockSlackHttpClient::success());
        let request = make_ask_request("tok-3", "Q?", None, vec![], None, None);
        assert_eq!(notifier.expected_reply_user_id(&request), None);
    }

    #[test]
    fn test_extract_reply_text() {
        let parsed =
            SlackNotifier::<ReqwestSlackHttpClient>::extract_reply_text("Use main branch").unwrap();
        assert_eq!(parsed, "Use main branch");
    }

    #[test]
    fn test_extract_reply_text_empty_string() {
        let result = SlackNotifier::<ReqwestSlackHttpClient>::extract_reply_text("");
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_reply_text_whitespace_only() {
        let result = SlackNotifier::<ReqwestSlackHttpClient>::extract_reply_text("   \n\t  ");
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_reply_text_trims_whitespace() {
        let result =
            SlackNotifier::<ReqwestSlackHttpClient>::extract_reply_text("  yes  ").unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_truncate_string_short_unchanged() {
        assert_eq!(truncate_string("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_string_exact_length_unchanged() {
        assert_eq!(truncate_string("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_string_over_limit_adds_ellipsis() {
        let result = truncate_string("hello world", 8);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 8);
    }

    #[test]
    fn test_truncate_string_very_small_max_no_room_for_ellipsis() {
        let result = truncate_string("hello", 3);
        assert_eq!(result.len(), 3);
        assert!(!result.contains("..."));
    }

    #[test]
    fn test_truncate_string_max_len_zero() {
        let result = truncate_string("hello", 0);
        assert_eq!(result, "");
    }

    #[test]
    fn test_truncate_string_empty_input() {
        assert_eq!(truncate_string("", 10), "");
    }

    #[test]
    fn test_truncate_string_with_known_constants() {
        let long_id = "x".repeat(100);
        let result = truncate_string(&long_id, MAX_SHORT_ID_LENGTH);
        assert!(result.len() <= MAX_SHORT_ID_LENGTH);
        assert!(result.ends_with("..."));

        let long_source = "y".repeat(50);
        let result = truncate_string(&long_source, MAX_SOURCE_LENGTH);
        assert!(result.len() <= MAX_SOURCE_LENGTH);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_slack_ts_to_datetime_valid() {
        let dt = slack_ts_to_datetime("1709123456.789012").unwrap();
        assert_eq!(dt.timestamp(), 1709123456);
    }

    #[test]
    fn test_slack_ts_to_datetime_invalid() {
        assert!(slack_ts_to_datetime("not_a_timestamp").is_none());
    }

    #[test]
    fn test_slack_ts_to_datetime_empty() {
        assert!(slack_ts_to_datetime("").is_none());
    }

    #[test]
    fn test_slack_text_mrkdwn() {
        let t = SlackText::mrkdwn("*bold*");
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("mrkdwn"));
        assert!(json.contains("*bold*"));
    }

    #[test]
    fn test_slack_text_plain_text() {
        let t = SlackText::plain_text("Hello");
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("plain_text"));
        assert!(json.contains("Hello"));
    }

    #[test]
    fn test_slack_block_header_serialization() {
        let block = SlackBlock::Header {
            text: SlackText::plain_text("Title"),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"header\""));
        assert!(json.contains("Title"));
    }

    #[test]
    fn test_slack_block_divider_serialization() {
        let block = SlackBlock::Divider;
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"divider\""));
    }

    #[test]
    fn test_slack_message_serialization() {
        let msg = SlackMessage {
            channel: Some("C123".to_string()),
            text: "fallback".to_string(),
            blocks: None,
            thread_ts: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("C123"));
        assert!(json.contains("fallback"));
        // Optional None fields should be skipped.
        assert!(!json.contains("blocks"));
        assert!(!json.contains("thread_ts"));
    }

    #[test]
    fn test_slack_api_response_deserialization() {
        let json = r#"{"ok":true,"ts":"1709123456.789012"}"#;
        let resp: SlackApiResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.ts.as_deref(), Some("1709123456.789012"));
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_slack_api_response_error_deserialization() {
        let json = r#"{"ok":false,"error":"channel_not_found"}"#;
        let resp: SlackApiResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("channel_not_found"));
    }

    #[test]
    fn test_slack_api_message_deserialization() {
        let json = r#"{"ts":"1709123456.000000","text":"hello","user":"U123","bot_id":null,"thread_ts":"1709123400.000000"}"#;
        let msg: SlackApiMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.ts, "1709123456.000000");
        assert_eq!(msg.text, "hello");
        assert_eq!(msg.user.as_deref(), Some("U123"));
        assert!(msg.bot_id.is_none());
        assert_eq!(msg.thread_ts.as_deref(), Some("1709123400.000000"));
    }

    #[test]
    fn test_reqwest_slack_http_client_default() {
        let client = ReqwestSlackHttpClient::default();
        assert!(std::mem::size_of_val(&client) > 0);
    }

    #[test]
    fn test_build_start_message_without_mention() {
        let issue = test_issue();
        let msg = build_start_message(&issue, None);

        assert!(msg.text.contains("Processing"));
        assert!(msg.text.contains("PROJ-123"));
        assert!(!msg.text.contains("<@"));
        assert!(msg.blocks.is_some());
        assert!(msg.channel.is_none());
        assert!(msg.thread_ts.is_none());

        let blocks = msg.blocks.unwrap();
        // Header + Section + Context (timestamp)
        assert!(blocks.len() >= 2);
    }

    #[test]
    fn test_build_start_message_with_mention() {
        let issue = test_issue();
        let msg = build_start_message(&issue, Some("<@U123>".to_string()));

        assert!(msg.text.contains("<@U123>"));
        let blocks = msg.blocks.unwrap();
        // Should have mention context block in addition to timestamp context
        let context_blocks: Vec<_> = blocks
            .iter()
            .filter(|b| matches!(b, SlackBlock::Context { .. }))
            .collect();
        assert!(context_blocks.len() >= 2);
    }

    #[test]
    fn test_build_success_message_without_mention() {
        let issue = test_issue();
        let msg = build_success_message(&issue, "https://github.com/org/repo/pull/1", None);

        assert!(msg.text.contains("PR Created"));
        assert!(msg.text.contains("PROJ-123"));
        assert!(msg.text.contains("https://github.com/org/repo/pull/1"));

        let blocks = msg.blocks.unwrap();
        // Header + Section (title/fields) + Section (PR link) + Context (timestamp)
        assert!(blocks.len() >= 3);
    }

    #[test]
    fn test_build_success_message_with_mention() {
        let issue = test_issue();
        let msg = build_success_message(
            &issue,
            "https://github.com/org/repo/pull/1",
            Some("<@U123>".to_string()),
        );

        assert!(msg.text.starts_with("<@U123>"));
    }

    #[test]
    fn test_build_completed_message_without_mention() {
        let issue = test_issue();
        let msg = build_completed_message(&issue, None);

        assert!(msg.text.contains("Completed"));
        assert!(msg.text.contains("PROJ-123"));

        let blocks = msg.blocks.unwrap();
        let section_blocks: Vec<_> = blocks
            .iter()
            .filter(|b| matches!(b, SlackBlock::Section { .. }))
            .collect();
        assert!(!section_blocks.is_empty());
        // Verify the "no PR URL" note is in the blocks
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("no PR URL was captured"));
    }

    #[test]
    fn test_build_completed_message_with_mention() {
        let issue = test_issue();
        let msg = build_completed_message(&issue, Some("<@U456>".to_string()));

        assert!(msg.text.starts_with("<@U456>"));
        let blocks = msg.blocks.unwrap();
        let context_with_mention = blocks.iter().any(|b| match b {
            SlackBlock::Context { elements } => elements.iter().any(|e| e.text.contains("<@U456>")),
            _ => false,
        });
        assert!(context_with_mention);
    }

    #[test]
    fn test_build_failed_message_without_mention() {
        let issue = test_issue();
        let msg = build_failed_message(&issue, "compilation error", None);

        assert!(msg.text.contains("Failed"));
        assert!(msg.text.contains("PROJ-123"));

        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("compilation error"));
    }

    #[test]
    fn test_build_failed_message_with_mention() {
        let issue = test_issue();
        let msg = build_failed_message(&issue, "error occurred", Some("<@U789>".to_string()));

        assert!(msg.text.starts_with("<@U789>"));
    }

    #[test]
    fn test_build_failed_message_truncates_error() {
        let issue = test_issue();
        let long_error = "x".repeat(2000);
        let msg = build_failed_message(&issue, &long_error, None);

        let blocks = msg.blocks.unwrap();
        let error_block = blocks
            .iter()
            .find(|b| match b {
                SlackBlock::Section { text, fields: None } => text.text.contains("Error"),
                _ => false,
            })
            .expect("Error section block should exist");

        match error_block {
            SlackBlock::Section { text, .. } => {
                // Error is truncated to 1000 chars + formatting
                assert!(text.text.len() <= 1100);
                assert!(text.text.contains("..."));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_build_status_message_content() {
        let msg = build_status_message("Everything is running smoothly");
        assert_eq!(msg.text, "Everything is running smoothly");

        let blocks = msg.blocks.unwrap();
        assert_eq!(blocks.len(), 2); // Section + Context
        match &blocks[0] {
            SlackBlock::Section { text, fields } => {
                assert_eq!(text.text, "Everything is running smoothly");
                assert!(fields.is_none());
            }
            _ => panic!("Expected Section block"),
        }
    }

    #[test]
    fn test_build_status_message_truncates_long_message() {
        let long_message = "y".repeat(5000);
        let msg = build_status_message(&long_message);
        assert!(msg.text.len() <= MAX_DESCRIPTION_LENGTH);
    }

    #[test]
    fn test_build_urgent_issues_message_empty() {
        let result = build_urgent_issues_message(&[], None);
        assert!(result.is_none());
    }

    #[test]
    fn test_build_urgent_issues_message_single() {
        let issues = vec![Issue::new(
            "1",
            "PROJ-1",
            "Urgent bug",
            "https://example.com/1",
            "linear",
        )];
        let msg = build_urgent_issues_message(&issues, None).unwrap();

        assert!(msg.text.contains("1 Urgent Issue Detected"));
        assert!(!msg.text.contains("Issues")); // singular
    }

    #[test]
    fn test_build_urgent_issues_message_plural() {
        let issues = vec![
            Issue::new("1", "PROJ-1", "Bug 1", "https://example.com/1", "linear"),
            Issue::new("2", "PROJ-2", "Bug 2", "https://example.com/2", "sentry"),
        ];
        let msg = build_urgent_issues_message(&issues, None).unwrap();

        assert!(msg.text.contains("2 Urgent Issues Detected"));
    }

    #[test]
    fn test_build_urgent_issues_message_with_mention() {
        let issues = vec![Issue::new(
            "1",
            "PROJ-1",
            "Bug",
            "https://example.com",
            "linear",
        )];
        let msg = build_urgent_issues_message(&issues, Some("<@U999>".to_string())).unwrap();

        assert!(msg.text.starts_with("<@U999>"));
    }

    #[test]
    fn test_build_urgent_issues_message_caps_at_ten() {
        let issues: Vec<Issue> = (1..=15)
            .map(|i| {
                Issue::new(
                    i.to_string(),
                    format!("PROJ-{}", i),
                    format!("Issue {}", i),
                    format!("https://example.com/{}", i),
                    "linear",
                )
            })
            .collect();
        let msg = build_urgent_issues_message(&issues, None).unwrap();

        let blocks = msg.blocks.unwrap();
        // Count section blocks with PROJ- (issue items)
        let issue_section_count = blocks
            .iter()
            .filter(|b| match b {
                SlackBlock::Section { text, .. } => text.text.contains("PROJ-"),
                _ => false,
            })
            .count();
        assert_eq!(issue_section_count, 10);
    }

    #[test]
    fn test_build_merged_message_without_mention() {
        let issue = test_issue();
        let msg = build_merged_message(&issue, "https://github.com/org/repo/pull/42", None);

        assert!(msg.text.contains("PR Merged"));
        assert!(msg.text.contains("PROJ-123"));
        assert!(msg.text.contains("https://github.com/org/repo/pull/42"));
    }

    #[test]
    fn test_build_merged_message_with_mention() {
        let issue = test_issue();
        let msg = build_merged_message(
            &issue,
            "https://github.com/org/repo/pull/42",
            Some("<@U111>".to_string()),
        );

        assert!(msg.text.starts_with("<@U111>"));
        let blocks = msg.blocks.unwrap();
        let has_mention_context = blocks.iter().any(|b| match b {
            SlackBlock::Context { elements } => elements.iter().any(|e| e.text.contains("<@U111>")),
            _ => false,
        });
        assert!(has_mention_context);
    }

    #[test]
    fn test_build_report_message() {
        let report = crate::reports::Report {
            period: "Daily".to_string(),
            from: chrono::Utc::now() - chrono::Duration::days(1),
            to: chrono::Utc::now(),
            issues_attempted: 10,
            issues_succeeded: 7,
            issues_failed: 2,
            issues_cannot_fix: 1,
            success_rate: 70.0,
            failure_rate: 20.0,
            prs_created: 7,
            prs_merged: 5,
            prs_closed: 0,
            by_source: std::collections::HashMap::new(),
            pending_count: 3,
            retryable_count: 1,
        };
        let msg = build_report_message(&report);

        assert!(!msg.text.is_empty());
        let blocks = msg.blocks.unwrap();
        assert_eq!(blocks.len(), 3); // Header + Section + Context
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("Daily"));
        assert!(block_json.contains("10")); // issues_attempted
        assert!(block_json.contains("70.0")); // success_rate
    }

    #[test]
    fn test_build_ask_question_message_basic() {
        let issue = test_issue();
        let request = make_ask_request("corr-1", "Choose branch?", None, vec![], None, None);
        let msg = build_ask_question_message(&issue, &request, None);

        assert!(!msg.text.contains("[CLAUDEAR-Q:"));
        assert!(msg.text.contains("Human input needed for PROJ-123"));
        assert!(msg.text.contains("Choose branch?"));
        assert!(msg.text.contains("Reply in this thread"));
        assert!(msg.blocks.is_none());
    }

    #[test]
    fn test_build_ask_question_message_with_why() {
        let issue = test_issue();
        let request = make_ask_request(
            "corr-2",
            "Which branch?",
            None,
            vec![],
            Some("Multiple branches exist"),
            None,
        );
        let msg = build_ask_question_message(&issue, &request, None);

        assert!(msg.text.contains("Why: Multiple branches exist"));
    }

    #[test]
    fn test_build_ask_question_message_with_context() {
        let issue = test_issue();
        let request = make_ask_request(
            "corr-3",
            "Which branch?",
            Some("The repo has main and develop branches"),
            vec![],
            None,
            None,
        );
        let msg = build_ask_question_message(&issue, &request, None);

        assert!(msg
            .text
            .contains("Context: The repo has main and develop branches"));
    }

    #[test]
    fn test_build_ask_question_message_with_options() {
        let issue = test_issue();
        let request = make_ask_request(
            "corr-4",
            "Choose branch?",
            None,
            vec!["main", "develop", "staging"],
            None,
            None,
        );
        let msg = build_ask_question_message(&issue, &request, None);

        assert!(msg.text.contains("Options: main | develop | staging"));
    }

    #[test]
    fn test_build_ask_question_message_with_mention() {
        let issue = test_issue();
        let request = make_ask_request("corr-5", "Which?", None, vec![], None, None);
        let msg = build_ask_question_message(&issue, &request, Some("<@U999>".to_string()));

        assert!(msg.text.starts_with("<@U999>"));
    }

    #[test]
    fn test_build_ask_question_message_with_all_fields() {
        let issue = test_issue();
        let request = make_ask_request(
            "corr-all",
            "Pick branch?",
            Some("Context about branches"),
            vec!["main", "develop"],
            Some("Because there are multiple"),
            None,
        );
        let msg = build_ask_question_message(&issue, &request, Some("<@U_ALL>".to_string()));

        assert!(msg.text.contains("<@U_ALL>"));
        assert!(!msg.text.contains("[CLAUDEAR-Q:"));
        assert!(msg.text.contains("Human input needed for PROJ-123"));
        assert!(msg.text.contains("Pick branch?"));
        assert!(msg.text.contains("Why: Because there are multiple"));
        assert!(msg.text.contains("Context: Context about branches"));
        assert!(msg.text.contains("Options: main | develop"));
        assert!(msg.text.contains("Reply in this thread"));
    }

    #[test]
    fn test_build_ask_question_context_truncation() {
        let issue = test_issue();
        let long_context = "z".repeat(1000);
        let request = make_ask_request("corr-trunc", "Q?", Some(&long_context), vec![], None, None);
        let msg = build_ask_question_message(&issue, &request, None);

        // Context should be truncated to 400 chars
        let context_part = msg.text.split("Context: ").nth(1).unwrap();
        let context_line = context_part.lines().next().unwrap();
        assert!(context_line.len() <= 410); // 400 + "..." margin
    }

    #[test]
    fn test_slack_message_with_all_fields_serialization() {
        let msg = SlackMessage {
            channel: Some("C123".to_string()),
            text: "fallback text".to_string(),
            blocks: Some(vec![
                SlackBlock::Header {
                    text: SlackText::plain_text("Header"),
                },
                SlackBlock::Section {
                    text: SlackText::mrkdwn("*Bold*"),
                    fields: Some(vec![SlackText::mrkdwn("field1")]),
                },
                SlackBlock::Context {
                    elements: vec![SlackText::mrkdwn("context")],
                },
                SlackBlock::Divider,
            ]),
            thread_ts: Some("12345.6789".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("C123"));
        assert!(json.contains("fallback text"));
        assert!(json.contains("header"));
        assert!(json.contains("section"));
        assert!(json.contains("context"));
        assert!(json.contains("divider"));
        assert!(json.contains("12345.6789"));
    }

    #[test]
    fn test_slack_block_section_with_fields_serialization() {
        let block = SlackBlock::Section {
            text: SlackText::mrkdwn("Main text"),
            fields: Some(vec![
                SlackText::mrkdwn("Field 1"),
                SlackText::mrkdwn("Field 2"),
            ]),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"section\""));
        assert!(json.contains("Field 1"));
        assert!(json.contains("Field 2"));
        assert!(json.contains("fields"));
    }

    #[test]
    fn test_slack_block_section_without_fields_serialization() {
        let block = SlackBlock::Section {
            text: SlackText::mrkdwn("Just text"),
            fields: None,
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"section\""));
        assert!(!json.contains("fields"));
    }

    #[test]
    fn test_slack_block_context_serialization() {
        let block = SlackBlock::Context {
            elements: vec![
                SlackText::mrkdwn("element1"),
                SlackText::plain_text("element2"),
            ],
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"context\""));
        assert!(json.contains("elements"));
        assert!(json.contains("element1"));
        assert!(json.contains("element2"));
    }

    #[test]
    fn test_slack_api_response_with_messages_deserialization() {
        let json = r#"{
            "ok": true,
            "messages": [
                {"ts": "1709123456.000000", "text": "hello"},
                {"ts": "1709123457.000000", "text": "world", "user": "U123"}
            ]
        }"#;
        let resp: SlackApiResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.messages.as_ref().unwrap().len(), 2);
        assert!(resp.ts.is_none());
    }

    #[test]
    fn test_slack_api_message_minimal_deserialization() {
        let json = r#"{"ts": "1709123456.000000"}"#;
        let msg: SlackApiMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.ts, "1709123456.000000");
        assert_eq!(msg.text, ""); // default
        assert!(msg.user.is_none());
        assert!(msg.bot_id.is_none());
        assert!(msg.thread_ts.is_none());
    }

    #[test]
    fn test_slack_ts_to_datetime_integer_only() {
        let dt = slack_ts_to_datetime("1709123456").unwrap();
        assert_eq!(dt.timestamp(), 1709123456);
    }

    #[test]
    fn test_slack_ts_to_datetime_preserves_nanos() {
        let dt = slack_ts_to_datetime("1709123456.500000").unwrap();
        assert_eq!(dt.timestamp(), 1709123456);
        // Check subsecond portion
        assert!(dt.timestamp_subsec_nanos() > 0);
    }

    #[test]
    fn test_slack_ts_to_datetime_zero() {
        let dt = slack_ts_to_datetime("0.000000").unwrap();
        assert_eq!(dt.timestamp(), 0);
    }

    #[test]
    fn test_truncate_string_unicode_safety() {
        // Multi-byte characters should be safely handled
        let s = "Hello \u{1F600} World"; // emoji is multi-byte
        let result = truncate_string(s, 10);
        assert!(result.len() <= 10);
        // Should not panic on char boundary
    }

    #[test]
    fn test_truncate_string_max_len_1() {
        let result = truncate_string("hello", 1);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_truncate_string_max_len_2() {
        let result = truncate_string("hello", 2);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_truncate_string_max_len_4_adds_ellipsis() {
        let result = truncate_string("hello world", 4);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 4);
    }

    #[tokio::test]
    async fn test_send_no_config_drops_message() {
        let mock = MockSlackHttpClient::success();
        let config = SlackConfig::default(); // No webhook, no bot
        let notifier = SlackNotifier::with_http_client(config, mock);
        // Should succeed (message dropped, no error)
        let result = notifier.notify_start(&test_issue()).await;
        assert!(result.is_ok());
        assert_eq!(notifier.http.get_call_count(), 0);
    }

    #[tokio::test]
    async fn test_send_prefers_webhook_over_bot() {
        let mock = MockSlackHttpClient::success();
        let config = SlackConfig {
            webhook_url: Some("https://hooks.slack.com/services/T00/B00/xxx".into()),
            bot_token: Some("xoxb-test-token".into()),
            channel_id: Some("C12345678".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client(config, mock);
        notifier.notify_start(&test_issue()).await.unwrap();

        let (url, _, auth) = notifier.http.get_last_post_call().unwrap();
        // Should use webhook URL, not chat.postMessage
        assert!(url.contains("hooks.slack.com"));
        assert!(auth.is_none());
    }

    #[tokio::test]
    async fn test_send_to_channel_returns_none_without_bot_token() {
        let mock = MockSlackHttpClient::success();
        let config = SlackConfig::default();
        let notifier = SlackNotifier::with_http_client(config, mock);
        let issue = test_issue();
        let request = make_ask_request("tok-1", "Q?", None, vec![], None, None);
        let delivery = notifier.ask_question(&issue, &request).await.unwrap();
        // Without bot_token, send_to_channel returns None for ts
        assert!(delivery.is_some());
        let d = delivery.unwrap();
        assert!(d.message_id.is_none());
    }

    #[tokio::test]
    async fn test_post_chat_message_http_error() {
        let mock = MockSlackHttpClient::error(500, "server error");
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let result = notifier.notify_start(&test_issue()).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("HTTP error"));
    }

    #[tokio::test]
    async fn test_post_chat_message_invalid_json_response() {
        let mock = MockSlackHttpClient::new(200, "not json at all");
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let result = notifier.notify_start(&test_issue()).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("parse"));
    }

    #[tokio::test]
    async fn test_notify_merged_bot_api() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let result = notifier
            .notify_merged(&test_issue(), "https://github.com/pr/1")
            .await;
        assert!(result.is_ok());
        let (url, body, auth) = notifier.http.get_last_post_call().unwrap();
        assert!(url.contains("chat.postMessage"));
        assert!(auth.is_some());
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("PR Merged"));
    }

    #[tokio::test]
    async fn test_notify_report() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let report = crate::reports::Report {
            period: "Weekly".to_string(),
            from: chrono::Utc::now() - chrono::Duration::weeks(1),
            to: chrono::Utc::now(),
            issues_attempted: 5,
            issues_succeeded: 3,
            issues_failed: 1,
            issues_cannot_fix: 1,
            success_rate: 60.0,
            failure_rate: 20.0,
            prs_created: 3,
            prs_merged: 2,
            prs_closed: 0,
            by_source: std::collections::HashMap::new(),
            pending_count: 0,
            retryable_count: 0,
        };
        let result = notifier.notify_report(&report).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_poll_returns_empty_without_channel_id() {
        let config = SlackConfig {
            bot_token: Some("xoxb-token".into()),
            channel_id: None, // No channel
            ..Default::default()
        };
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(config, mock);
        let request = make_ask_request("corr-x", "Q?", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, Utc::now())
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn test_poll_returns_empty_with_empty_bot_token() {
        let config = SlackConfig {
            bot_token: Some("".into()),
            channel_id: Some("C123".to_string()),
            ..Default::default()
        };
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(config, mock);
        let request = make_ask_request("corr-y", "Q?", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, Utc::now())
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn test_poll_handles_failed_history_api() {
        let mock = MockSlackHttpClient::success_api();
        let since = Utc::now() - chrono::Duration::minutes(5);

        mock.add_get_response(
            "conversations.history",
            &serde_json::json!({"ok": false, "error": "not_authed"}).to_string(),
        );

        let notifier = SlackNotifier::with_http_client(bot_config_with_user(), mock);
        let request = make_ask_request("corr-fail", "Q?", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, since)
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn test_poll_ignores_bot_messages_in_thread() {
        let mock = MockSlackHttpClient::success_api();
        let since = Utc::now() - chrono::Duration::minutes(5);

        let history_body = serde_json::json!({
            "ok": true,
            "messages": [{
                "ts": "1709123456.000000",
                "text": "Human input needed for LIN-1:\nQuestion",
                "bot_id": "B12345"
            }]
        });

        let reply_ts = format!("{:.6}", (Utc::now().timestamp() as f64) + 10.0);
        let replies_body = serde_json::json!({
            "ok": true,
            "messages": [
                {
                    "ts": "1709123456.000000",
                    "text": "Human input needed for LIN-1:\nQuestion",
                    "bot_id": "B12345"
                },
                {
                    "ts": reply_ts,
                    "text": "Bot response",
                    "bot_id": "B99999"
                }
            ]
        });

        mock.add_get_response("conversations.history", &history_body.to_string());
        mock.add_get_response("conversations.replies", &replies_body.to_string());

        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let request = make_ask_request("corr-bot", "Q?", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, since)
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn test_poll_skips_messages_with_no_user() {
        let mock = MockSlackHttpClient::success_api();
        let since = Utc::now() - chrono::Duration::minutes(5);

        let history_body = serde_json::json!({
            "ok": true,
            "messages": [{
                "ts": "1709123456.000000",
                "text": "Human input needed for LIN-1:\nQuestion",
                "bot_id": "B12345"
            }]
        });

        let reply_ts = format!("{:.6}", (Utc::now().timestamp() as f64) + 10.0);
        let replies_body = serde_json::json!({
            "ok": true,
            "messages": [
                {
                    "ts": "1709123456.000000",
                    "text": "Human input needed for LIN-1:\nQuestion",
                    "bot_id": "B12345"
                },
                {
                    "ts": reply_ts,
                    "text": "message with no user field"
                }
            ]
        });

        mock.add_get_response("conversations.history", &history_body.to_string());
        mock.add_get_response("conversations.replies", &replies_body.to_string());

        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let request = make_ask_request("corr-nouser", "Q?", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, since)
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn test_poll_thread_reply_with_failed_thread_api() {
        let mock = MockSlackHttpClient::success_api();
        let since = Utc::now() - chrono::Duration::minutes(5);

        let history_body = serde_json::json!({
            "ok": true,
            "messages": [{
                "ts": "1709123456.000000",
                "text": "Human input needed for LIN-1:\nQuestion",
                "bot_id": "B12345"
            }]
        });

        let replies_body = serde_json::json!({
            "ok": false,
            "error": "thread_not_found"
        });

        mock.add_get_response("conversations.history", &history_body.to_string());
        mock.add_get_response("conversations.replies", &replies_body.to_string());

        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let request = make_ask_request("corr-tf", "Q?", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, since)
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn test_send_with_empty_bot_token_and_channel_drops_message() {
        let config = SlackConfig {
            bot_token: Some("".into()),
            channel_id: Some("".to_string()),
            ..Default::default()
        };
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(config, mock);
        let result = notifier.notify_start(&test_issue()).await;
        assert!(result.is_ok());
        assert_eq!(notifier.http.get_call_count(), 0);
    }

    #[test]
    fn test_has_bot_channel_false_for_empty_strings() {
        let config = SlackConfig {
            bot_token: Some("".into()),
            channel_id: Some("C123".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        assert!(!notifier.has_bot_channel());
    }

    #[test]
    fn test_timestamp_returns_rfc3339() {
        let ts = timestamp();
        // Should be parseable as RFC 3339
        assert!(chrono::DateTime::parse_from_rfc3339(&ts).is_ok());
    }

    #[test]
    fn test_get_target_slack_id_for_issue_no_resolved_user() {
        let config = SlackConfig {
            user_id: Some("U_GLOBAL".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        let issue = test_issue();
        assert_eq!(
            notifier.get_target_slack_id_for_issue(&issue),
            Some("U_GLOBAL".to_string())
        );
    }

    #[test]
    fn test_get_target_slack_id_for_issue_resolved_user_with_slack() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "alice".to_string(),
            claudear_config::config::UserConfig {
                slack_id: Some("U_ALICE".to_string()),
                ..Default::default()
            },
        );
        let registry = UserRegistry::new(users);
        let config = SlackConfig {
            user_id: Some("U_GLOBAL".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client_and_registry(
            config,
            MockSlackHttpClient::success(),
            registry,
        );
        let mut issue = test_issue();
        issue.set_metadata("resolved_user", "alice");
        assert_eq!(
            notifier.get_target_slack_id_for_issue(&issue),
            Some("U_ALICE".to_string())
        );
    }

    #[test]
    fn test_get_target_slack_id_for_issue_resolved_user_without_slack() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "bob".to_string(),
            claudear_config::config::UserConfig {
                slack_id: None,
                ..Default::default()
            },
        );
        let registry = UserRegistry::new(users);
        let config = SlackConfig {
            user_id: Some("U_GLOBAL".to_string()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client_and_registry(
            config,
            MockSlackHttpClient::success(),
            registry,
        );
        let mut issue = test_issue();
        issue.set_metadata("resolved_user", "bob");
        // Falls back to global user_id
        assert_eq!(
            notifier.get_target_slack_id_for_issue(&issue),
            Some("U_GLOBAL".to_string())
        );
    }

    #[test]
    fn test_get_target_slack_id_for_issue_no_config() {
        let config = SlackConfig::default();
        let notifier = SlackNotifier::with_http_client(config, MockSlackHttpClient::success());
        let issue = test_issue();
        assert_eq!(notifier.get_target_slack_id_for_issue(&issue), None);
    }

    #[test]
    fn test_build_closed_message_without_mention() {
        let issue = test_issue();
        let msg = build_closed_message(&issue, "https://github.com/org/repo/pull/10", None);

        assert!(msg.text.contains("PR Closed"));
        assert!(msg.text.contains("PROJ-123"));
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("closed without merging"));
        assert!(block_json.contains("View PR"));
    }

    #[test]
    fn test_build_closed_message_with_mention() {
        let issue = test_issue();
        let msg = build_closed_message(
            &issue,
            "https://github.com/org/repo/pull/10",
            Some("<@U555>".to_string()),
        );

        let blocks = msg.blocks.unwrap();
        // Mention should be inserted as first block
        let first_block_json = serde_json::to_string(&blocks[0]).unwrap();
        assert!(first_block_json.contains("<@U555>"));
    }

    #[test]
    fn test_build_cascade_success_message_without_mention() {
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/upstream");
        issue.set_metadata("cascade_downstream_repo", "org/downstream");

        let msg =
            build_cascade_success_message(&issue, "https://github.com/org/downstream/pull/5", None);

        assert!(msg.text.contains("Cascade PR"));
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("org/upstream"));
        assert!(block_json.contains("org/downstream"));
        assert!(block_json.contains("View PR"));
    }

    #[test]
    fn test_build_cascade_success_message_with_mention() {
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/upstream");
        issue.set_metadata("cascade_downstream_repo", "org/downstream");

        let msg = build_cascade_success_message(
            &issue,
            "https://github.com/org/downstream/pull/5",
            Some("<@U777>".to_string()),
        );

        let blocks = msg.blocks.unwrap();
        let first_block_json = serde_json::to_string(&blocks[0]).unwrap();
        assert!(first_block_json.contains("<@U777>"));
    }

    #[test]
    fn test_build_cascade_failed_message_without_mention() {
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/upstream");
        issue.set_metadata("cascade_downstream_repo", "org/downstream");

        let msg = build_cascade_failed_message(&issue, "Build failed", None);

        assert!(msg.text.contains("Cascade Failed"));
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("org/upstream"));
        assert!(block_json.contains("org/downstream"));
        assert!(block_json.contains("Build failed"));
    }

    #[test]
    fn test_build_cascade_failed_message_with_mention() {
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/upstream");
        issue.set_metadata("cascade_downstream_repo", "org/downstream");

        let msg =
            build_cascade_failed_message(&issue, "Error occurred", Some("<@U888>".to_string()));

        let blocks = msg.blocks.unwrap();
        let first_block_json = serde_json::to_string(&blocks[0]).unwrap();
        assert!(first_block_json.contains("<@U888>"));
    }

    #[test]
    fn test_build_cascade_failed_message_truncates_long_error() {
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/upstream");
        issue.set_metadata("cascade_downstream_repo", "org/downstream");

        let long_error = "e".repeat(1000);
        let msg = build_cascade_failed_message(&issue, &long_error, None);

        let blocks = msg.blocks.unwrap();
        let error_block = blocks.iter().find(|b| match b {
            SlackBlock::Section { text, .. } => text.text.contains("Error"),
            _ => false,
        });
        assert!(error_block.is_some());
        match error_block.unwrap() {
            SlackBlock::Section { text, .. } => {
                assert!(text.text.len() <= 600);
                assert!(text.text.contains("..."));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_build_regression_detected_message_without_mention() {
        let issue = test_issue();
        let msg = build_regression_detected_message(&issue, "Test failure detected", None);

        assert!(msg.text.contains("Regression Detected"));
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("previously fixed issue has regressed"));
        assert!(block_json.contains("Test failure detected"));
        assert!(block_json.contains("Retry has been scheduled"));
    }

    #[test]
    fn test_build_regression_detected_message_with_mention() {
        let issue = test_issue();
        let msg = build_regression_detected_message(
            &issue,
            "regression error",
            Some("<@U111>".to_string()),
        );

        let blocks = msg.blocks.unwrap();
        let first_block_json = serde_json::to_string(&blocks[0]).unwrap();
        assert!(first_block_json.contains("<@U111>"));
    }

    #[test]
    fn test_build_regression_detected_message_truncates_long_error() {
        let issue = test_issue();
        let long_error = "r".repeat(1000);
        let msg = build_regression_detected_message(&issue, &long_error, None);

        let blocks = msg.blocks.unwrap();
        let details_block = blocks.iter().find(|b| match b {
            SlackBlock::Section { text, .. } => text.text.contains("Details"),
            _ => false,
        });
        assert!(details_block.is_some());
        match details_block.unwrap() {
            SlackBlock::Section { text, .. } => {
                assert!(text.text.len() <= 600);
                assert!(text.text.contains("..."));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_build_regression_resolved_message_without_mention() {
        let issue = test_issue();
        let msg = build_regression_resolved_message(&issue, None);

        assert!(msg.text.contains("Regression Resolved"));
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("No regression detected after monitoring period"));
        assert!(block_json.contains("Issue resolved after final check"));
    }

    #[test]
    fn test_build_regression_resolved_message_with_mention() {
        let issue = test_issue();
        let msg = build_regression_resolved_message(&issue, Some("<@U222>".to_string()));

        let blocks = msg.blocks.unwrap();
        let first_block_json = serde_json::to_string(&blocks[0]).unwrap();
        assert!(first_block_json.contains("<@U222>"));
    }

    #[tokio::test]
    async fn test_notify_success_cascade_issue() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/upstream");
        issue.set_metadata("cascade_downstream_repo", "org/downstream");

        let result = notifier
            .notify_success(&issue, "https://github.com/org/downstream/pull/5")
            .await;
        assert!(result.is_ok());
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Cascade PR"));
    }

    #[tokio::test]
    async fn test_notify_success_pr_update() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let mut issue = test_issue();
        issue.set_metadata("is_pr_update", true);

        let result = notifier
            .notify_success(&issue, "https://github.com/org/repo/pull/42")
            .await;
        assert!(result.is_ok());
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("PR Updated"));
    }

    #[tokio::test]
    async fn test_notify_completed_regression_resolved() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let mut issue = test_issue();
        issue.set_metadata("regression_resolved", true);

        let result = notifier.notify_completed(&issue).await;
        assert!(result.is_ok());
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Regression Resolved"));
    }

    #[tokio::test]
    async fn test_notify_failed_regression_detected() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let mut issue = test_issue();
        issue.set_metadata("regression_detected", true);

        let result = notifier.notify_failed(&issue, "Regression error").await;
        assert!(result.is_ok());
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Regression Detected"));
    }

    #[tokio::test]
    async fn test_notify_failed_cascade() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/upstream");
        issue.set_metadata("cascade_downstream_repo", "org/downstream");

        let result = notifier.notify_failed(&issue, "Cascade error").await;
        assert!(result.is_ok());
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Cascade Failed"));
    }

    #[tokio::test]
    async fn test_notify_closed_via_webhook() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);

        let result = notifier
            .notify_closed(&test_issue(), "https://github.com/org/repo/pull/99")
            .await;
        assert!(result.is_ok());
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("PR Closed"));
    }

    #[tokio::test]
    async fn test_notify_closed_via_bot_api() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);

        let result = notifier
            .notify_closed(&test_issue(), "https://github.com/org/repo/pull/99")
            .await;
        assert!(result.is_ok());
        let (url, _, _) = notifier.http.get_last_post_call().unwrap();
        assert!(url.contains("chat.postMessage"));
    }

    #[test]
    fn test_build_success_message_pr_update() {
        let mut issue = test_issue();
        issue.set_metadata("is_pr_update", true);

        let msg = build_success_message(&issue, "https://github.com/org/repo/pull/42", None);
        assert!(msg.text.contains("PR Updated"));
        assert!(!msg.text.contains("PR Created"));
    }

    #[test]
    fn test_build_success_message_pr_update_with_mention() {
        let mut issue = test_issue();
        issue.set_metadata("is_pr_update", true);

        let msg = build_success_message(
            &issue,
            "https://github.com/org/repo/pull/42",
            Some("<@U_UPDATE>".to_string()),
        );
        assert!(msg.text.contains("PR Updated"));
        assert!(msg.text.starts_with("<@U_UPDATE>"));
    }

    #[tokio::test]
    async fn test_send_to_channel_returns_none_with_empty_channel_id() {
        let config = SlackConfig {
            bot_token: Some("xoxb-token".into()),
            channel_id: Some("".to_string()),
            ..Default::default()
        };
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(config, mock);
        let issue = test_issue();
        let request = make_ask_request("tok-ec", "Q?", None, vec![], None, None);
        let delivery = notifier.ask_question(&issue, &request).await.unwrap();
        assert!(delivery.is_some());
        assert!(delivery.unwrap().message_id.is_none());
    }

    #[tokio::test]
    async fn test_ask_question_returns_none_ts_when_no_bot() {
        let mock = MockSlackHttpClient::success();
        let config = SlackConfig {
            webhook_url: Some("https://hooks.slack.com/services/T00/B00/xxx".into()),
            ..Default::default()
        };
        let notifier = SlackNotifier::with_http_client(config, mock);
        let issue = test_issue();
        let request = make_ask_request("tok-nb", "Q?", None, vec![], None, None);
        let delivery = notifier.ask_question(&issue, &request).await.unwrap();
        assert!(delivery.is_some());
        let d = delivery.unwrap();
        assert!(d.message_id.is_none());
    }

    #[test]
    fn test_slack_notifier_new_production_constructor() {
        let config = webhook_config();
        let registry = empty_registry();
        let notifier = SlackNotifier::new(config, registry);
        assert_eq!(notifier.name(), "slack");
        assert!(notifier.is_enabled());
    }

    #[test]
    fn test_with_http_client_and_registry() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "tester".to_string(),
            claudear_config::config::UserConfig {
                slack_id: Some("U_TESTER".to_string()),
                ..Default::default()
            },
        );
        let registry = UserRegistry::new(users);
        let notifier = SlackNotifier::with_http_client_and_registry(
            bot_config(),
            MockSlackHttpClient::success_api(),
            registry,
        );
        assert_eq!(notifier.name(), "slack");
        assert!(notifier.is_enabled());
    }

    #[tokio::test]
    async fn test_poll_non_threaded_reply_skips_bot_question_messages() {
        let mock = MockSlackHttpClient::success_api();
        let since = Utc::now() - chrono::Duration::minutes(5);

        // History has a question message from bot and an unrelated bot message
        let history_body = serde_json::json!({
            "ok": true,
            "messages": [
                {
                    "ts": "1709123456.000000",
                    "text": "Human input needed for LIN-1:\nQuestion",
                    "bot_id": "B12345"
                },
                {
                    "ts": "1709123457.000000",
                    "text": "Some other bot message",
                    "bot_id": "B99999"
                }
            ]
        });

        let replies_body = serde_json::json!({
            "ok": true,
            "messages": [{
                "ts": "1709123456.000000",
                "text": "Human input needed for LIN-1:\nQuestion",
                "bot_id": "B12345"
            }]
        });

        mock.add_get_response("conversations.history", &history_body.to_string());
        mock.add_get_response("conversations.replies", &replies_body.to_string());

        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let request = make_ask_request("corr-skip", "Q?", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, since)
            .await
            .unwrap();

        // Both messages are from bots, so no replies
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn test_notify_merged_with_mention_webhook() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config_with_user(), mock);

        let result = notifier
            .notify_merged(&test_issue(), "https://github.com/org/repo/pull/99")
            .await;
        assert!(result.is_ok());
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("<@U987654321>"));
        assert!(text.contains("PR Merged"));
    }

    #[tokio::test]
    async fn test_notify_completed_via_bot_api() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);

        let result = notifier.notify_completed(&test_issue()).await;
        assert!(result.is_ok());
        let (url, body, auth) = notifier.http.get_last_post_call().unwrap();
        assert!(url.contains("chat.postMessage"));
        assert!(auth.is_some());
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Completed"));
    }

    #[tokio::test]
    async fn test_notify_failed_via_bot_api() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);

        let result = notifier
            .notify_failed(&test_issue(), "something went wrong")
            .await;
        assert!(result.is_ok());
        let (url, _, _) = notifier.http.get_last_post_call().unwrap();
        assert!(url.contains("chat.postMessage"));
    }

    #[tokio::test]
    async fn test_notify_success_via_bot_api() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);

        let result = notifier
            .notify_success(&test_issue(), "https://github.com/org/repo/pull/42")
            .await;
        assert!(result.is_ok());
        let (url, body, _) = notifier.http.get_last_post_call().unwrap();
        assert!(url.contains("chat.postMessage"));
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("PR Created"));
    }

    #[test]
    fn test_build_report_message_zero_values() {
        let report = crate::reports::Report {
            period: "Empty".to_string(),
            from: chrono::Utc::now() - chrono::Duration::hours(1),
            to: chrono::Utc::now(),
            issues_attempted: 0,
            issues_succeeded: 0,
            issues_failed: 0,
            issues_cannot_fix: 0,
            success_rate: 0.0,
            failure_rate: 0.0,
            prs_created: 0,
            prs_merged: 0,
            prs_closed: 0,
            by_source: std::collections::HashMap::new(),
            pending_count: 0,
            retryable_count: 0,
        };
        let msg = build_report_message(&report);

        assert!(!msg.text.is_empty());
        let blocks = msg.blocks.unwrap();
        assert_eq!(blocks.len(), 3);
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("Empty"));
    }

    #[tokio::test]
    async fn test_notify_status_via_bot_api() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);

        let result = notifier.notify_status("System healthy").await;
        assert!(result.is_ok());
        let (url, _, _) = notifier.http.get_last_post_call().unwrap();
        assert!(url.contains("chat.postMessage"));
    }

    #[tokio::test]
    async fn test_notify_report_via_bot_api() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let report = crate::reports::Report {
            period: "Daily".to_string(),
            from: chrono::Utc::now() - chrono::Duration::days(1),
            to: chrono::Utc::now(),
            issues_attempted: 3,
            issues_succeeded: 2,
            issues_failed: 1,
            issues_cannot_fix: 0,
            success_rate: 66.7,
            failure_rate: 33.3,
            prs_created: 2,
            prs_merged: 1,
            prs_closed: 0,
            by_source: std::collections::HashMap::new(),
            pending_count: 0,
            retryable_count: 0,
        };
        let result = notifier.notify_report(&report).await;
        assert!(result.is_ok());
        let (url, _, _) = notifier.http.get_last_post_call().unwrap();
        assert!(url.contains("chat.postMessage"));
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_via_bot_api() {
        let mock = MockSlackHttpClient::success_api();
        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let issues = vec![Issue::new(
            "1",
            "PROJ-1",
            "Urgent",
            "https://example.com",
            "linear",
        )];

        let result = notifier.notify_urgent_issues(&issues).await;
        assert!(result.is_ok());
        let (url, _, _) = notifier.http.get_last_post_call().unwrap();
        assert!(url.contains("chat.postMessage"));
    }

    #[tokio::test]
    async fn test_poll_returns_empty_with_empty_channel_id() {
        let config = SlackConfig {
            bot_token: Some("xoxb-token".into()),
            channel_id: Some("".to_string()),
            ..Default::default()
        };
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(config, mock);
        let request = make_ask_request("corr-empty-ch", "Q?", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, Utc::now())
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn test_poll_no_question_messages() {
        let mock = MockSlackHttpClient::success_api();
        let since = Utc::now() - chrono::Duration::minutes(5);

        let history_body = serde_json::json!({
            "ok": true,
            "messages": [
                {
                    "ts": "1709123456.000000",
                    "text": "Regular message without token",
                    "user": "U123"
                }
            ]
        });

        mock.add_get_response("conversations.history", &history_body.to_string());

        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let request = make_ask_request("corr-nq", "Q?", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, since)
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn test_poll_empty_history() {
        let mock = MockSlackHttpClient::success_api();
        let since = Utc::now() - chrono::Duration::minutes(5);

        let history_body = serde_json::json!({
            "ok": true,
            "messages": []
        });

        mock.add_get_response("conversations.history", &history_body.to_string());

        let notifier = SlackNotifier::with_http_client(bot_config(), mock);
        let request = make_ask_request("corr-empty-hist", "Q?", None, vec![], None, None);
        let replies = notifier
            .poll_question_replies(&request, since)
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[test]
    fn test_truncate_string_within_limit() {
        let s = "short";
        let result = truncate_string(s, 100);
        assert_eq!(result, "short");
    }

    #[test]
    fn test_truncate_string_at_limit() {
        let s = "exactly10!";
        assert_eq!(s.len(), 10);
        let result = truncate_string(s, 10);
        assert_eq!(result, "exactly10!");
    }

    #[test]
    fn test_truncate_string_over_limit() {
        let s = "this string is definitely over the limit";
        let result = truncate_string(s, 15);
        assert!(result.len() <= 15);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_string_very_small_limit() {
        // Limit of 3 or less: no room for "..."
        let r1 = truncate_string("abcdef", 3);
        assert_eq!(r1.len(), 3);
        assert!(!r1.contains("..."));

        let r2 = truncate_string("abcdef", 2);
        assert_eq!(r2.len(), 2);

        let r3 = truncate_string("abcdef", 1);
        assert_eq!(r3.len(), 1);
    }

    #[test]
    fn test_truncate_string_empty() {
        assert_eq!(truncate_string("", 0), "");
        assert_eq!(truncate_string("", 5), "");
        assert_eq!(truncate_string("", 100), "");
    }

    #[test]
    fn test_truncate_string_multibyte() {
        // Multi-byte UTF-8 chars near the boundary
        let s = "Hello \u{00E9}\u{00E9}\u{00E9}\u{00E9}\u{00E9}"; // e-acute is 2 bytes each
        let result = truncate_string(s, 10);
        assert!(result.len() <= 10);
        // Should not panic on char boundary issues

        // Emoji test (4-byte char)
        let emoji_str = "Hi \u{1F600}\u{1F600}\u{1F600}"; // each emoji is 4 bytes
        let result2 = truncate_string(emoji_str, 8);
        assert!(result2.len() <= 8);

        // CJK characters (3 bytes each)
        let cjk = "\u{4E16}\u{754C}\u{4F60}\u{597D}\u{5417}"; // 5 CJK chars = 15 bytes
        let result3 = truncate_string(cjk, 7);
        assert!(result3.len() <= 7);
    }

    #[test]
    fn test_slack_ts_to_datetime_valid_fractional() {
        let dt = slack_ts_to_datetime("1709123456.789012").unwrap();
        assert_eq!(dt.timestamp(), 1709123456);
        assert!(dt.timestamp_subsec_nanos() > 0);
    }

    #[test]
    fn test_slack_ts_to_datetime_invalid_strings() {
        assert!(slack_ts_to_datetime("not_a_timestamp").is_none());
        assert!(slack_ts_to_datetime("abc.def").is_none());
        assert!(slack_ts_to_datetime("").is_none());
        assert!(slack_ts_to_datetime("   ").is_none());
    }

    #[test]
    fn test_slack_ts_to_datetime_integer() {
        let dt = slack_ts_to_datetime("1709123456").unwrap();
        assert_eq!(dt.timestamp(), 1709123456);
        // No fractional part, so subsec nanos should be zero (or very close)
        assert_eq!(dt.timestamp_subsec_nanos(), 0);
    }

    #[test]
    fn test_extract_reply_text_normal() {
        let result = SlackNotifier::<ReqwestSlackHttpClient>::extract_reply_text("Use main branch");
        assert_eq!(result.unwrap(), "Use main branch");
    }

    #[test]
    fn test_extract_reply_text_empty() {
        assert!(SlackNotifier::<ReqwestSlackHttpClient>::extract_reply_text("").is_none());
        assert!(SlackNotifier::<ReqwestSlackHttpClient>::extract_reply_text("   \t\n  ").is_none());
    }

    #[test]
    fn test_build_start_message() {
        let issue = test_issue();
        let msg = build_start_message(&issue, None);

        // Fallback text includes Processing and issue ID
        assert!(msg.text.contains("Processing"));
        assert!(msg.text.contains("PROJ-123"));
        // Blocks are present
        assert!(msg.blocks.is_some());
        let blocks = msg.blocks.unwrap();
        assert!(blocks.len() >= 2);
        // No channel set on builder-produced messages
        assert!(msg.channel.is_none());
        assert!(msg.thread_ts.is_none());
    }

    #[test]
    fn test_build_success_message() {
        let issue = test_issue();
        let msg = build_success_message(&issue, "https://github.com/org/repo/pull/99", None);

        assert!(msg.text.contains("PR Created"));
        assert!(msg.text.contains("PROJ-123"));
        assert!(msg.text.contains("https://github.com/org/repo/pull/99"));
        let blocks = msg.blocks.unwrap();
        // Header + Section(title) + Section(PR link) + Context(timestamp)
        assert!(blocks.len() >= 3);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("View PR"));
    }

    #[test]
    fn test_build_failed_message() {
        let issue = test_issue();
        let msg = build_failed_message(&issue, "compilation error: missing semicolon", None);

        assert!(msg.text.contains("Failed"));
        assert!(msg.text.contains("PROJ-123"));
        let blocks = msg.blocks.unwrap();
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("compilation error"));
        assert!(json.contains("Error"));
    }

    #[test]
    fn test_get_source_emoji() {
        // Known sources get specific emoji
        assert_eq!(get_source_emoji("linear"), "\u{1F4CB}");
        assert_eq!(get_source_emoji("sentry"), "\u{1F534}");
        assert_eq!(get_source_emoji("github"), "\u{1F419}");
        assert_eq!(get_source_emoji("jira"), "\u{1F3AB}");
        assert_eq!(get_source_emoji("slack"), "\u{1F4AC}");
        // Case insensitive
        assert_eq!(get_source_emoji("Linear"), "\u{1F4CB}");
        assert_eq!(get_source_emoji("SENTRY"), "\u{1F534}");
        assert_eq!(get_source_emoji("GitHub"), "\u{1F419}");
        // Unknown sources get default pushpin emoji
        assert_eq!(get_source_emoji("unknown"), "\u{1F4CC}");
        assert_eq!(get_source_emoji(""), "\u{1F4CC}");
        assert_eq!(get_source_emoji("custom_source"), "\u{1F4CC}");
    }

    #[test]
    fn test_has_bot_channel() {
        // Both token and channel present -> true
        let notifier =
            SlackNotifier::with_http_client(bot_config(), MockSlackHttpClient::success());
        assert!(notifier.has_bot_channel());

        // Only bot_token, no channel -> false
        let config_no_channel = SlackConfig {
            bot_token: Some("xoxb-test-token".into()),
            channel_id: None,
            ..Default::default()
        };
        let notifier2 =
            SlackNotifier::with_http_client(config_no_channel, MockSlackHttpClient::success());
        assert!(!notifier2.has_bot_channel());

        // Only channel_id, no bot_token -> false
        let config_no_token = SlackConfig {
            bot_token: None,
            channel_id: Some("C12345678".to_string()),
            ..Default::default()
        };
        let notifier3 =
            SlackNotifier::with_http_client(config_no_token, MockSlackHttpClient::success());
        assert!(!notifier3.has_bot_channel());

        // Both empty strings -> false
        let config_empty = SlackConfig {
            bot_token: Some("".into()),
            channel_id: Some("".to_string()),
            ..Default::default()
        };
        let notifier4 =
            SlackNotifier::with_http_client(config_empty, MockSlackHttpClient::success());
        assert!(!notifier4.has_bot_channel());

        // Both None -> false
        let config_none = SlackConfig::default();
        let notifier5 =
            SlackNotifier::with_http_client(config_none, MockSlackHttpClient::success());
        assert!(!notifier5.has_bot_channel());

        // Token non-empty, channel empty -> false
        let config_mixed = SlackConfig {
            bot_token: Some("xoxb-token".into()),
            channel_id: Some("".to_string()),
            ..Default::default()
        };
        let notifier6 =
            SlackNotifier::with_http_client(config_mixed, MockSlackHttpClient::success());
        assert!(!notifier6.has_bot_channel());
    }

    #[test]
    fn test_build_start_message_with_trigger_reason() {
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://linear.app/1", "linear");
        issue.set_metadata("trigger_reason", "Retry attempt 2: timeout error");
        let msg = build_start_message(&issue, None);
        let blocks = msg.blocks.as_ref().unwrap();
        let trigger_block = blocks.iter().find(|b| {
            if let SlackBlock::Context { elements } = b {
                elements
                    .iter()
                    .any(|e| e.kind == "mrkdwn" && e.text.contains("*Trigger:*"))
            } else {
                false
            }
        });
        assert!(trigger_block.is_some());
    }

    #[test]
    fn test_build_start_message_without_trigger_reason() {
        let issue = Issue::new("1", "LIN-1", "Test", "https://linear.app/1", "linear");
        let msg = build_start_message(&issue, None);
        let blocks = msg.blocks.as_ref().unwrap();
        let trigger_block = blocks.iter().find(|b| {
            if let SlackBlock::Context { elements } = b {
                elements
                    .iter()
                    .any(|e| e.kind == "mrkdwn" && e.text.contains("*Trigger:*"))
            } else {
                false
            }
        });
        assert!(trigger_block.is_none());
    }

    #[test]
    fn test_build_success_message_with_trigger_reason() {
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://linear.app/1", "linear");
        issue.set_metadata("trigger_reason", "Review feedback received");
        let msg = build_success_message(&issue, "https://github.com/pr/1", None);
        let blocks = msg.blocks.as_ref().unwrap();
        let trigger_block = blocks.iter().find(|b| {
            if let SlackBlock::Context { elements } = b {
                elements
                    .iter()
                    .any(|e| e.kind == "mrkdwn" && e.text.contains("*Trigger:*"))
            } else {
                false
            }
        });
        assert!(trigger_block.is_some());
    }

    #[test]
    fn test_build_failed_message_with_trigger_reason() {
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://linear.app/1", "linear");
        issue.set_metadata("trigger_reason", "Manual trigger");
        let msg = build_failed_message(&issue, "some error", None);
        let blocks = msg.blocks.as_ref().unwrap();
        let trigger_block = blocks.iter().find(|b| {
            if let SlackBlock::Context { elements } = b {
                elements
                    .iter()
                    .any(|e| e.kind == "mrkdwn" && e.text.contains("*Trigger:*"))
            } else {
                false
            }
        });
        assert!(trigger_block.is_some());
    }

    // === Coverage tests for build_closed_message ===

    #[test]
    fn test_build_closed_message_without_mention_v2() {
        let issue = test_issue();
        let msg = build_closed_message(&issue, "https://github.com/org/repo/pull/10", None);

        assert!(msg.text.contains("PR Closed"));
        assert!(msg.text.contains("PROJ-123"));
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("closed without merging"));
        assert!(block_json.contains("View PR"));
    }

    #[test]
    fn test_build_closed_message_with_mention_v2() {
        let issue = test_issue();
        let msg = build_closed_message(
            &issue,
            "https://github.com/org/repo/pull/10",
            Some("<@U_CLOSED>".to_string()),
        );

        let blocks = msg.blocks.unwrap();
        // Mention block is inserted at position 0
        match &blocks[0] {
            SlackBlock::Section { text, .. } => {
                assert!(text.text.contains("<@U_CLOSED>"));
            }
            _ => panic!("Expected Section block with mention at position 0"),
        }
    }

    // === Coverage tests for build_cascade_success_message ===

    #[test]
    fn test_build_cascade_success_message_basic() {
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/upstream");
        issue.set_metadata("cascade_downstream_repo", "org/downstream");
        let msg = build_cascade_success_message(&issue, "https://github.com/pr/99", None);

        assert!(msg.text.contains("Cascade PR"));
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("org/upstream"));
        assert!(block_json.contains("org/downstream"));
        assert!(block_json.contains("View PR"));
    }

    #[test]
    fn test_build_cascade_success_message_with_mention_v2() {
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/up");
        issue.set_metadata("cascade_downstream_repo", "org/down");
        let msg = build_cascade_success_message(
            &issue,
            "https://github.com/pr/99",
            Some("<@U_CASCADE>".to_string()),
        );

        let blocks = msg.blocks.unwrap();
        match &blocks[0] {
            SlackBlock::Section { text, .. } => {
                assert!(text.text.contains("<@U_CASCADE>"));
            }
            _ => panic!("Expected mention Section at position 0"),
        }
    }

    // === Coverage tests for build_cascade_failed_message ===

    #[test]
    fn test_build_cascade_failed_message_basic() {
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/upstream");
        issue.set_metadata("cascade_downstream_repo", "org/downstream");
        let msg = build_cascade_failed_message(&issue, "merge conflict", None);

        assert!(msg.text.contains("Cascade Failed"));
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("org/upstream"));
        assert!(block_json.contains("org/downstream"));
        assert!(block_json.contains("merge conflict"));
    }

    #[test]
    fn test_build_cascade_failed_message_truncates_long_error_v2() {
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/up");
        issue.set_metadata("cascade_downstream_repo", "org/down");
        let long_error = "e".repeat(1000);
        let msg = build_cascade_failed_message(&issue, &long_error, None);

        let blocks = msg.blocks.unwrap();
        let error_block = blocks
            .iter()
            .find(|b| match b {
                SlackBlock::Section { text, .. } => text.text.contains("Error"),
                _ => false,
            })
            .expect("Error section block should exist");
        match error_block {
            SlackBlock::Section { text, .. } => {
                assert!(text.text.len() <= 600);
                assert!(text.text.contains("..."));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_build_cascade_failed_message_with_mention_v2() {
        let mut issue = test_issue();
        issue.set_metadata("cascade_upstream_repo", "org/up");
        issue.set_metadata("cascade_downstream_repo", "org/down");
        let msg = build_cascade_failed_message(&issue, "error", Some("<@U_CF>".to_string()));
        let blocks = msg.blocks.unwrap();
        match &blocks[0] {
            SlackBlock::Section { text, .. } => {
                assert!(text.text.contains("<@U_CF>"));
            }
            _ => panic!("Expected mention at position 0"),
        }
    }

    // === Coverage tests for build_regression_detected_message ===

    #[test]
    fn test_build_regression_detected_message_basic() {
        let issue = test_issue();
        let msg = build_regression_detected_message(&issue, "CI test #42 failed again", None);

        assert!(msg.text.contains("Regression Detected"));
        assert!(msg.text.contains("PROJ-123"));
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("previously fixed issue has regressed"));
        assert!(block_json.contains("CI test #42 failed again"));
        assert!(block_json.contains("Retry has been scheduled"));
    }

    #[test]
    fn test_build_regression_detected_message_with_mention_v2() {
        let issue = test_issue();
        let msg = build_regression_detected_message(
            &issue,
            "regression error",
            Some("<@U_REG>".to_string()),
        );
        let blocks = msg.blocks.unwrap();
        match &blocks[0] {
            SlackBlock::Section { text, .. } => {
                assert!(text.text.contains("<@U_REG>"));
            }
            _ => panic!("Expected mention at position 0"),
        }
    }

    #[test]
    fn test_build_regression_detected_message_truncates_long_error_v2() {
        let issue = test_issue();
        let long_error = "r".repeat(1000);
        let msg = build_regression_detected_message(&issue, &long_error, None);
        let blocks = msg.blocks.unwrap();
        let detail_block = blocks
            .iter()
            .find(|b| match b {
                SlackBlock::Section { text, .. } => text.text.contains("Details"),
                _ => false,
            })
            .unwrap();
        match detail_block {
            SlackBlock::Section { text, .. } => {
                assert!(text.text.len() <= 600);
            }
            _ => unreachable!(),
        }
    }

    // === Coverage tests for build_regression_resolved_message ===

    #[test]
    fn test_build_regression_resolved_message_basic() {
        let issue = test_issue();
        let msg = build_regression_resolved_message(&issue, None);

        assert!(msg.text.contains("Regression Resolved"));
        assert!(msg.text.contains("PROJ-123"));
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("No regression detected after monitoring period"));
        assert!(block_json.contains("Issue resolved after final check"));
    }

    #[test]
    fn test_build_regression_resolved_message_with_mention_v2() {
        let issue = test_issue();
        let msg = build_regression_resolved_message(&issue, Some("<@U_RESOLVED>".to_string()));
        let blocks = msg.blocks.unwrap();
        match &blocks[0] {
            SlackBlock::Section { text, .. } => {
                assert!(text.text.contains("<@U_RESOLVED>"));
            }
            _ => panic!("Expected mention at position 0"),
        }
    }

    // === Coverage tests for Notifier trait methods via mock HTTP ===

    #[tokio::test]
    async fn test_notify_merged_sends_correct_content() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        notifier
            .notify_merged(&test_issue(), "https://github.com/org/repo/pull/42")
            .await
            .unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("PR Merged"));
    }

    #[tokio::test]
    async fn test_notify_closed_sends_correct_content() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        notifier
            .notify_closed(&test_issue(), "https://github.com/org/repo/pull/42")
            .await
            .unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("PR Closed"));
    }

    #[tokio::test]
    async fn test_notify_report_sends_correct_content() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let report = crate::reports::Report {
            period: "Weekly".to_string(),
            from: chrono::Utc::now() - chrono::Duration::days(7),
            to: chrono::Utc::now(),
            issues_attempted: 20,
            issues_succeeded: 15,
            issues_failed: 3,
            issues_cannot_fix: 2,
            success_rate: 75.0,
            failure_rate: 15.0,
            prs_created: 15,
            prs_merged: 12,
            prs_closed: 1,
            by_source: std::collections::HashMap::new(),
            pending_count: 5,
            retryable_count: 2,
        };
        notifier.notify_report(&report).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let blocks = body["blocks"].as_array().unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("Weekly"));
        assert!(block_json.contains("75.0"));
    }

    #[tokio::test]
    async fn test_notify_success_cascade_path() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let mut issue = test_issue();
        issue.set_metadata("cascade_downstream_repo", "org/downstream");
        issue.set_metadata("cascade_upstream_repo", "org/upstream");
        notifier
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Cascade PR"));
    }

    #[tokio::test]
    async fn test_notify_completed_regression_resolved_path() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let mut issue = test_issue();
        issue.set_metadata("regression_resolved", true);
        notifier.notify_completed(&issue).await.unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Regression Resolved"));
    }

    #[tokio::test]
    async fn test_notify_failed_regression_detected_path() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let mut issue = test_issue();
        issue.set_metadata("regression_detected", true);
        notifier
            .notify_failed(&issue, "regression error")
            .await
            .unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Regression Detected"));
    }

    #[tokio::test]
    async fn test_notify_failed_cascade_failed_path() {
        let mock = MockSlackHttpClient::success();
        let notifier = SlackNotifier::with_http_client(webhook_config(), mock);
        let mut issue = test_issue();
        issue.set_metadata("cascade_downstream_repo", "org/downstream");
        issue.set_metadata("cascade_upstream_repo", "org/upstream");
        notifier
            .notify_failed(&issue, "cascade error")
            .await
            .unwrap();
        let (_, body, _) = notifier.http.get_last_post_call().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Cascade Failed"));
    }

    // === Coverage: build_success_message with is_pr_update and changelog ===

    #[test]
    fn test_build_success_message_pr_update_v2() {
        let mut issue = test_issue();
        issue.set_metadata("is_pr_update", true);
        let msg = build_success_message(&issue, "https://github.com/pr/1", None);
        assert!(msg.text.contains("PR Updated"));
    }

    #[test]
    fn test_build_success_message_with_changelog() {
        let mut issue = test_issue();
        issue.set_metadata("changelog", "Fixed authentication bug");
        let msg = build_success_message(&issue, "https://github.com/pr/1", None);
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("Changes"));
        assert!(block_json.contains("Fixed authentication bug"));
    }

    // === Coverage: build_completed_message with custom completion_reason ===

    #[test]
    fn test_build_completed_message_with_custom_reason() {
        let mut issue = test_issue();
        issue.set_metadata("completion_reason", "Already fixed in previous release");
        let msg = build_completed_message(&issue, None);
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("Already fixed in previous release"));
    }

    // === Coverage: confidence in success messages ===

    #[test]
    fn test_build_success_message_with_confidence() {
        let mut issue = test_issue();
        issue.set_metadata("confidence", 85u8);
        let msg = build_success_message(&issue, "https://github.com/pr/1", None);
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("Fix Confidence"));
        assert!(block_json.contains("85/100"));
    }

    #[test]
    fn test_build_success_message_with_confidence_and_reasoning() {
        let mut issue = test_issue();
        issue.set_metadata("confidence", 72u8);
        issue.set_metadata("confidence_reasoning", "Simple null check fix".to_string());
        let msg = build_success_message(&issue, "https://github.com/pr/1", None);
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(block_json.contains("72/100"));
        assert!(block_json.contains("Simple null check fix"));
    }

    #[test]
    fn test_build_success_message_without_confidence() {
        let issue = test_issue();
        let msg = build_success_message(&issue, "https://github.com/pr/1", None);
        let blocks = msg.blocks.unwrap();
        let block_json = serde_json::to_string(&blocks).unwrap();
        assert!(!block_json.contains("Fix Confidence"));
    }
}
