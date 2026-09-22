//! E2eAsk trait + Discord/Slack implementations for test orchestration.
//!
//! This covers test-specific question flow: posting issue messages as a user,
//! detecting bot ask questions by embed format, and replying.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::time::Duration;

/// Trait for E2E question flow orchestration.
#[async_trait]
pub trait E2eAsk: Send + Sync {
    /// Downcast helper for backend-specific operations.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Post a message in the ask channel (simulates a user posting an issue).
    async fn post_issue_message(&self, content: &str) -> Result<String>;

    /// Poll for a question from the bot after the given message ID.
    /// Returns the question message ID.
    async fn poll_for_question(&self, after_id: &str, timeout: Duration) -> Result<String>;

    /// Reply to a question message.
    async fn reply_to_question(&self, question_id: &str, answer: &str) -> Result<()>;
}

pub struct DiscordAsk {
    client: claudear::DiscordClient,
    channel_id: String,
    webhook_url: Option<String>,
    http: reqwest::Client,
}

impl DiscordAsk {
    pub fn new(bot_token: String, channel_id: String) -> Result<Self> {
        let client = claudear::DiscordClient::new(&bot_token).context("create Discord client")?;
        let webhook_url = std::env::var("CLAUDEAR_E2E_DISCORD_WEBHOOK_URL")
            .ok()
            .filter(|s| !s.is_empty());
        Ok(Self {
            client,
            channel_id,
            webhook_url,
            http: claudear_core::tls::client(),
        })
    }
}

#[async_trait]
impl E2eAsk for DiscordAsk {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn post_issue_message(&self, content: &str) -> Result<String> {
        // Prefer webhook: messages via webhook have webhook_id set, so the daemon's
        // Discord source won't filter them as bot messages.
        if let Some(ref webhook_url) = self.webhook_url {
            let body = serde_json::json!({ "content": content });
            let resp = self
                .http
                .post(format!("{}?wait=true", webhook_url))
                .json(&body)
                .send()
                .await
                .context("Discord webhook post")?;

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                bail!("Discord webhook error: {}", body);
            }

            let json: serde_json::Value = resp.json().await.context("parse webhook response")?;
            return json
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .context("no id in Discord webhook response");
        }

        // Fallback: bot token (message will have author.bot=true — will be filtered)
        let params = claudear::discord::CreateMessageParams::text(content);
        let msg = self
            .client
            .send_message(&self.channel_id, params)
            .await
            .context("send Discord message")?;
        Ok(msg.id)
    }

    async fn poll_for_question(&self, after_id: &str, timeout: Duration) -> Result<String> {
        let start = std::time::Instant::now();
        let interval = Duration::from_secs(3);

        loop {
            if start.elapsed() > timeout {
                bail!("Timed out waiting for Discord question after {}", after_id);
            }

            let messages = self
                .client
                .list_channel_messages_after(&self.channel_id, after_id, 10)
                .await
                .unwrap_or_default();

            // Look for an ask question embed (title contains "Input needed")
            for msg in &messages {
                for embed in &msg.embeds {
                    if let Some(ref title) = embed.title {
                        if title.contains("Input needed") {
                            tracing::info!(
                                msg_id = %msg.id,
                                "Found ask question embed"
                            );
                            return Ok(msg.id.clone());
                        }
                    }
                }
            }

            tokio::time::sleep(interval).await;
        }
    }

    async fn reply_to_question(&self, question_id: &str, answer: &str) -> Result<()> {
        let params = claudear::discord::CreateMessageParams::text(answer).replying_to(question_id);
        self.client
            .send_message(&self.channel_id, params)
            .await
            .context("send Discord reply")?;
        Ok(())
    }
}

pub struct SlackAsk {
    bot_token: String,
    channel_id: String,
    webhook_url: Option<String>,
    client: reqwest::Client,
}

impl SlackAsk {
    pub fn new(bot_token: String, channel_id: String) -> Self {
        let webhook_url = std::env::var("CLAUDEAR_E2E_SLACK_WEBHOOK_URL")
            .ok()
            .filter(|s| !s.is_empty());
        Self {
            bot_token,
            channel_id,
            webhook_url,
            client: claudear_core::tls::client(),
        }
    }

    /// Resolve the bot user ID via `auth.test`. This is needed so the daemon's
    /// Slack notifier accepts thread replies from the bot (which we use to
    /// simulate user replies in E2E tests).
    pub async fn resolve_bot_user_id(&self) -> Result<String> {
        let url = "https://slack.com/api/auth.test";
        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .send()
            .await
            .context("Slack auth.test")?;
        let json: serde_json::Value = response.json().await.context("parse auth.test")?;
        json.get("user_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .context("no user_id in auth.test response")
    }

    async fn slack_post(
        &self,
        endpoint: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!("https://slack.com/api/{}", endpoint);
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .json(&body)
            .send()
            .await
            .context("Slack API request")?;

        let json: serde_json::Value = response.json().await.context("parse Slack response")?;

        if !json.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let error = json
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            bail!("Slack API error: {}", error);
        }

        Ok(json)
    }
}

#[async_trait]
impl E2eAsk for SlackAsk {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn post_issue_message(&self, content: &str) -> Result<String> {
        // Prefer webhook: messages via webhook don't have bot_id, so the daemon's
        // Slack source won't filter them as bot messages.
        if let Some(ref webhook_url) = self.webhook_url {
            let resp = self
                .client
                .post(webhook_url)
                .json(&serde_json::json!({ "text": content }))
                .send()
                .await
                .context("Slack webhook post")?;

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                bail!("Slack webhook error: {}", body);
            }

            // Fetch the latest message to get its ts
            tokio::time::sleep(Duration::from_millis(500)).await;
            let history = self
                .slack_post(
                    "conversations.history",
                    serde_json::json!({
                        "channel": self.channel_id,
                        "limit": 1,
                    }),
                )
                .await?;

            return history
                .pointer("/messages/0/ts")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .context("no ts in Slack history after webhook post");
        }

        // Fallback: bot token (message will have bot_id — may be filtered)
        let response = self
            .slack_post(
                "chat.postMessage",
                serde_json::json!({
                    "channel": self.channel_id,
                    "text": content,
                }),
            )
            .await?;

        response
            .get("ts")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .context("no ts in Slack response")
    }

    async fn poll_for_question(&self, after_id: &str, timeout: Duration) -> Result<String> {
        let start = std::time::Instant::now();
        let interval = Duration::from_secs(3);

        loop {
            if start.elapsed() > timeout {
                bail!("Timed out waiting for Slack question after {}", after_id);
            }

            let response = self
                .slack_post(
                    "conversations.history",
                    serde_json::json!({
                        "channel": self.channel_id,
                        "oldest": after_id,
                        "limit": 10,
                    }),
                )
                .await;

            if let Ok(json) = response {
                if let Some(messages) = json.get("messages").and_then(|v| v.as_array()) {
                    for msg in messages {
                        // Slack ask questions are posted as plain text containing
                        // "Human input needed" (no blocks). Match on text content
                        // to avoid matching notification messages that have blocks.
                        let text = msg.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if text.contains("Human input needed") {
                            if let Some(ts) = msg.get("ts").and_then(|v| v.as_str()) {
                                tracing::info!(ts, text, "Found ask question in Slack");
                                return Ok(ts.to_string());
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(interval).await;
        }
    }

    async fn reply_to_question(&self, question_id: &str, answer: &str) -> Result<()> {
        self.slack_post(
            "chat.postMessage",
            serde_json::json!({
                "channel": self.channel_id,
                "text": answer,
                "thread_ts": question_id,
            }),
        )
        .await?;
        Ok(())
    }
}
