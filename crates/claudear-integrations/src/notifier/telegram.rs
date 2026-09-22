//! Telegram notifier via Telegram Bot API.

use super::Notifier;
use crate::ask_reply_inbox;
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use claudear_config::config::TelegramConfig;
use claudear_config::users::UserRegistry;
use claudear_core::error::{Error, Result};
use claudear_core::http::HttpResponse;
use claudear_core::types::{AskDelivery, AskReply, AskRequest, Issue};
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::RwLock;

/// Trait for HTTP client used by Telegram notifier.
#[async_trait]
pub trait TelegramHttpClient: Send + Sync {
    async fn post_json(&self, url: &str, body: &serde_json::Value) -> Result<HttpResponse>;
    async fn get_json(&self, url: &str) -> Result<HttpResponse>;
}

/// Real HTTP client using reqwest.
pub struct ReqwestTelegramClient {
    client: reqwest::Client,
}

impl ReqwestTelegramClient {
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

impl Default for ReqwestTelegramClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TelegramHttpClient for ReqwestTelegramClient {
    async fn post_json(&self, url: &str, body: &serde_json::Value) -> Result<HttpResponse> {
        let response = self.client.post(url).json(body).send().await?;

        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();

        Ok(HttpResponse { status, body })
    }

    async fn get_json(&self, url: &str) -> Result<HttpResponse> {
        let response = self.client.get(url).send().await?;

        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();

        Ok(HttpResponse { status, body })
    }
}

#[derive(Debug, Deserialize)]
struct TelegramSendMessageApiResponse {
    ok: bool,
    #[serde(default)]
    result: Option<TelegramSendMessageResult>,
}

#[derive(Debug, Deserialize)]
struct TelegramSendMessageResult {
    message_id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramGetUpdatesResponse {
    ok: bool,
    #[serde(default)]
    result: Vec<TelegramUpdateItem>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdateItem {
    update_id: i64,
    #[serde(default)]
    message: Option<TelegramInboundApiMessage>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramInboundApiMessage {
    message_id: i64,
    chat: TelegramInboundApiChat,
    #[serde(default)]
    from: Option<TelegramInboundApiUser>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    date: Option<i64>,
    #[serde(default)]
    reply_to_message: Option<TelegramInboundApiReplyMessage>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramInboundApiReplyMessage {
    message_id: i64,
    #[serde(default)]
    from: Option<TelegramInboundApiUser>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramInboundApiUser {
    id: i64,
    is_bot: bool,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramInboundApiChat {
    id: i64,
}

/// Telegram notifier that sends notifications via Telegram Bot API.
pub struct TelegramNotifier<H: TelegramHttpClient = ReqwestTelegramClient> {
    config: TelegramConfig,
    http: H,
    user_registry: UserRegistry,
    reply_last_update_id: RwLock<Option<i64>>,
}

impl TelegramNotifier<ReqwestTelegramClient> {
    /// Create a new Telegram notifier.
    pub fn new(config: TelegramConfig, user_registry: UserRegistry) -> Self {
        Self {
            config,
            http: ReqwestTelegramClient::new(),
            user_registry,
            reply_last_update_id: RwLock::new(None),
        }
    }
}

impl<H: TelegramHttpClient> TelegramNotifier<H> {
    /// Create a new Telegram notifier with custom HTTP client.
    pub fn with_http_client(config: TelegramConfig, http: H) -> Self {
        Self {
            config,
            http,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            reply_last_update_id: RwLock::new(None),
        }
    }

    /// Create a new Telegram notifier with custom HTTP client and user registry.
    pub fn with_http_client_and_registry(
        config: TelegramConfig,
        http: H,
        user_registry: UserRegistry,
    ) -> Self {
        Self {
            config,
            http,
            user_registry,
            reply_last_update_id: RwLock::new(None),
        }
    }

    fn resolve_recipients(&self, issue: Option<&Issue>) -> Vec<String> {
        if let Some(issue) = issue {
            if let Some(slug) = issue.get_metadata::<String>("resolved_user") {
                if let Some(user) = self.user_registry.get_by_slug(&slug) {
                    if let Some(ref chat_id) = user.telegram_chat_id {
                        return vec![chat_id.clone()];
                    }
                }
            }
        }

        // Fall back: collect chat_id + to_chat_ids from config
        let mut recipients = Vec::new();
        if let Some(ref chat_id) = self.config.chat_id {
            recipients.push(chat_id.clone());
        }
        for chat_id in &self.config.to_chat_ids {
            recipients.push(chat_id.clone());
        }
        recipients
    }

    fn extract_reply_text(content: &str) -> Option<String> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn poll_chat_ids(&self) -> HashSet<i64> {
        let mut ids = HashSet::new();
        for raw in self
            .config
            .listen_chat_id
            .iter()
            .chain(self.config.chat_id.iter())
            .chain(self.config.to_chat_ids.iter())
        {
            if let Ok(id) = raw.parse::<i64>() {
                ids.insert(id);
            }
        }
        ids
    }

    fn reply_matches_request(
        &self,
        request: &AskRequest,
        msg: &ask_reply_inbox::TelegramInboundMessage,
    ) -> bool {
        let poll_chat_ids = self.poll_chat_ids();
        if !poll_chat_ids.is_empty() && !poll_chat_ids.contains(&msg.chat_id) {
            return false;
        }

        let ask_ids: HashSet<i64> =
            ask_reply_inbox::ask_delivery_ids("telegram", &request.correlation_id)
                .into_iter()
                .filter_map(|id| id.parse::<i64>().ok())
                .collect();

        let is_reply_to_known_ask = msg
            .reply_to_message_id
            .map(|id| ask_ids.contains(&id))
            .unwrap_or(false);

        let ask_prefix = format!("Human input needed for {}", request.short_id);
        let is_reply_to_matching_ask_text = msg
            .reply_to_text
            .as_deref()
            .map(|text| text.contains(&ask_prefix))
            .unwrap_or(false)
            && msg.reply_to_is_bot.unwrap_or(true);

        is_reply_to_known_ask || is_reply_to_matching_ask_text
    }

    fn collect_replies_from_inbox(
        &self,
        request: &AskRequest,
        since: DateTime<Utc>,
    ) -> Vec<AskReply> {
        let mut replies = Vec::new();

        for msg in ask_reply_inbox::telegram_messages_since(since) {
            if !self.reply_matches_request(request, &msg) {
                continue;
            }

            let answer = match Self::extract_reply_text(&msg.text) {
                Some(v) => v,
                None => continue,
            };

            replies.push(AskReply {
                correlation_id: request.correlation_id.clone(),
                channel: "telegram".to_string(),
                responder: msg.responder_id.or(msg.responder_username),
                answer,
                replied_at: msg.replied_at,
            });
        }

        replies.sort_by_key(|r| r.replied_at);
        replies
    }

    fn record_inbound_message_for_replies(msg: &TelegramInboundApiMessage) {
        let from = match msg.from.as_ref() {
            Some(user) if !user.is_bot => user,
            _ => return,
        };
        let text = match msg.text.as_ref() {
            Some(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => return,
        };

        let replied_at = msg
            .date
            .and_then(|secs| Utc.timestamp_opt(secs, 0).single())
            .unwrap_or_else(Utc::now);

        ask_reply_inbox::record_telegram_message(ask_reply_inbox::TelegramInboundMessage {
            message_id: msg.message_id,
            chat_id: msg.chat.id,
            responder_id: Some(from.id.to_string()),
            responder_username: from.username.clone(),
            text,
            replied_at,
            reply_to_message_id: msg.reply_to_message.as_ref().map(|m| m.message_id),
            reply_to_text: msg.reply_to_message.as_ref().and_then(|m| m.text.clone()),
            reply_to_is_bot: msg
                .reply_to_message
                .as_ref()
                .and_then(|m| m.from.as_ref().map(|u| u.is_bot)),
        });
    }

    async fn ingest_updates_into_reply_inbox(&self) -> Result<()> {
        let bot_token = match &self.config.bot_token {
            Some(token) => token.expose(),
            None => return Ok(()),
        };

        let mut url = format!(
            "https://api.telegram.org/bot{}/getUpdates?timeout=0",
            bot_token
        );
        if let Some(last_id) = *self
            .reply_last_update_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            url.push_str(&format!("&offset={}", last_id + 1));
        }

        let response = self.http.get_json(&url).await?;
        if response.status < 200 || response.status >= 300 {
            return Err(Error::notifier(
                "telegram",
                format!("Telegram getUpdates error: {}", response.body),
            ));
        }

        let parsed: TelegramGetUpdatesResponse =
            serde_json::from_str(&response.body).map_err(|e| {
                Error::notifier(
                    "telegram",
                    format!("Failed to parse getUpdates response: {}", e),
                )
            })?;

        if !parsed.ok {
            return Err(Error::notifier(
                "telegram",
                "Telegram getUpdates returned ok=false",
            ));
        }

        if let Some(last) = parsed.result.last() {
            let mut lock = self
                .reply_last_update_id
                .write()
                .unwrap_or_else(|e| e.into_inner());
            *lock = Some(last.update_id);
        }

        for update in parsed.result {
            if let Some(msg) = update.message.as_ref() {
                Self::record_inbound_message_for_replies(msg);
            }
        }

        Ok(())
    }

    async fn send_message_with_ids(
        &self,
        text: &str,
        issue: Option<&Issue>,
    ) -> Result<Vec<String>> {
        let bot_token = match &self.config.bot_token {
            Some(token) => token.expose(),
            None => return Ok(Vec::new()),
        };

        let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);

        // Truncate message to Telegram limit (4096 chars)
        let truncated_text = if text.len() > 4096 {
            format!("{}...", &text[..text.floor_char_boundary(4093)])
        } else {
            text.to_string()
        };

        let recipients = self.resolve_recipients(issue);

        let mut sent_ids = Vec::new();

        for chat_id in &recipients {
            let body = serde_json::json!({
                "chat_id": chat_id,
                "text": truncated_text,
                "parse_mode": "HTML"
            });

            let response = self.http.post_json(&url, &body).await?;

            if response.status < 200 || response.status >= 300 {
                return Err(Error::notifier(
                    "telegram",
                    format!("Telegram API error: {}", response.body),
                ));
            }

            if let Ok(parsed) =
                serde_json::from_str::<TelegramSendMessageApiResponse>(&response.body)
            {
                if parsed.ok {
                    if let Some(result) = parsed.result {
                        sent_ids.push(result.message_id.to_string());
                    }
                }
            }
        }

        Ok(sent_ids)
    }

    async fn send_message(&self, text: &str, issue: Option<&Issue>) -> Result<()> {
        let _ = self.send_message_with_ids(text, issue).await?;
        Ok(())
    }
}

#[async_trait]
impl<H: TelegramHttpClient + 'static> Notifier for TelegramNotifier<H> {
    fn name(&self) -> &str {
        "telegram"
    }

    fn is_enabled(&self) -> bool {
        self.config.bot_token.is_some()
            && (self.config.chat_id.is_some() || !self.config.to_chat_ids.is_empty())
    }

    async fn notify_start(&self, issue: &Issue) -> Result<()> {
        let mut body = format!(
            "[Claudear] Processing {} from {} - {}",
            issue.short_id, issue.source, issue.title
        );
        if let Some(reason) = issue.get_metadata::<String>("trigger_reason") {
            body.push_str(&format!("\nTrigger: {}", reason));
        }
        self.send_message(&body, Some(issue)).await
    }

    async fn notify_success(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        let mut body = if issue
            .get_metadata::<String>("cascade_downstream_repo")
            .is_some()
        {
            let downstream = issue
                .get_metadata::<String>("cascade_downstream_repo")
                .unwrap_or_default();
            format!(
                "[Claudear] Cascade PR for {} ({}): {}",
                issue.short_id, downstream, pr_url
            )
        } else if issue.get_metadata::<bool>("is_pr_update").unwrap_or(false) {
            format!("[Claudear] PR Updated for {}: {}", issue.short_id, pr_url)
        } else {
            format!("[Claudear] PR Created for {}: {}", issue.short_id, pr_url)
        };
        if let Some(reason) = issue.get_metadata::<String>("trigger_reason") {
            body.push_str(&format!("\nTrigger: {}", reason));
        }
        if let Some(confidence) = issue.get_metadata::<u8>("confidence") {
            body.push_str(&format!("\nFix Confidence: {}/100", confidence));
        }
        self.send_message(&body, Some(issue)).await
    }

    async fn notify_completed(&self, issue: &Issue) -> Result<()> {
        let body = if issue
            .get_metadata::<bool>("regression_resolved")
            .unwrap_or(false)
        {
            format!(
                "[Claudear] Regression Resolved: {} (no regression after monitoring)",
                issue.short_id
            )
        } else {
            let reason = issue
                .get_metadata::<String>("completion_reason")
                .unwrap_or_else(|| "no PR URL".to_string());
            format!("[Claudear] Completed {}: {}", issue.short_id, reason)
        };
        self.send_message(&body, Some(issue)).await
    }

    async fn notify_failed(&self, issue: &Issue, error: &str) -> Result<()> {
        let short_error = if error.len() > 100 {
            format!("{}...", &error[..error.floor_char_boundary(97)])
        } else {
            error.to_string()
        };

        let mut body = if issue
            .get_metadata::<bool>("regression_detected")
            .unwrap_or(false)
        {
            format!("[Claudear] REGRESSION {}: {}", issue.short_id, short_error)
        } else if issue
            .get_metadata::<String>("cascade_downstream_repo")
            .is_some()
        {
            let downstream = issue
                .get_metadata::<String>("cascade_downstream_repo")
                .unwrap_or_default();
            format!(
                "[Claudear] CASCADE FAILED {} ({}): {}",
                issue.short_id, downstream, short_error
            )
        } else {
            format!("[Claudear] FAILED {}: {}", issue.short_id, short_error)
        };
        if let Some(reason) = issue.get_metadata::<String>("trigger_reason") {
            body.push_str(&format!("\nTrigger: {}", reason));
        }
        self.send_message(&body, Some(issue)).await
    }

    async fn notify_merged(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        let body = format!("[Claudear] PR Merged for {}: {}", issue.short_id, pr_url);
        self.send_message(&body, Some(issue)).await
    }

    async fn notify_closed(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        let body = format!("[Claudear] PR Closed for {}: {}", issue.short_id, pr_url);
        self.send_message(&body, Some(issue)).await
    }

    async fn notify_status(&self, message: &str) -> Result<()> {
        let body = format!("[Claudear] {}", message);
        self.send_message(&body, None).await
    }

    async fn notify_urgent_issues(&self, issues: &[Issue]) -> Result<()> {
        if issues.is_empty() {
            return Ok(());
        }

        let body = format!(
            "[Claudear] {} urgent issue(s): {}",
            issues.len(),
            issues
                .iter()
                .take(3)
                .map(|i| i.short_id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.send_message(&body, None).await
    }

    async fn ask_question(
        &self,
        issue: &Issue,
        request: &AskRequest,
    ) -> Result<Option<AskDelivery>> {
        let body = format!(
            "[Claudear] Human input needed for {}: {}",
            issue.short_id, request.question.question
        );
        let message_ids = self.send_message_with_ids(&body, Some(issue)).await?;
        for message_id in &message_ids {
            ask_reply_inbox::remember_ask_delivery_id(
                "telegram",
                &request.correlation_id,
                message_id.clone(),
            );
        }
        Ok(Some(AskDelivery {
            channel: "telegram".to_string(),
            target: None,
            message_id: message_ids.first().cloned(),
        }))
    }

    async fn poll_question_replies(
        &self,
        request: &AskRequest,
        since: DateTime<Utc>,
    ) -> Result<Vec<AskReply>> {
        if !self.config.source_enabled {
            self.ingest_updates_into_reply_inbox().await?;
        }
        Ok(self.collect_replies_from_inbox(request, since))
    }

    fn supports_replies(&self) -> bool {
        self.is_enabled()
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn empty_registry() -> UserRegistry {
        UserRegistry::new(std::collections::HashMap::new())
    }

    /// Mock Telegram HTTP client for testing.
    struct MockTelegramClient {
        post_response_status: u16,
        post_response_body: String,
        get_response_status: u16,
        get_response_body: String,
        call_count: AtomicUsize,
        last_calls: Mutex<Vec<(String, serde_json::Value)>>,
        get_calls: Mutex<Vec<String>>,
    }

    impl MockTelegramClient {
        fn new(status: u16, body: &str) -> Self {
            Self {
                post_response_status: status,
                post_response_body: body.to_string(),
                get_response_status: status,
                get_response_body: body.to_string(),
                call_count: AtomicUsize::new(0),
                last_calls: Mutex::new(Vec::new()),
                get_calls: Mutex::new(Vec::new()),
            }
        }

        fn with_get_response(mut self, status: u16, body: &str) -> Self {
            self.get_response_status = status;
            self.get_response_body = body.to_string();
            self
        }

        fn success() -> Self {
            Self::new(200, r#"{"ok": true, "result": {"message_id": 42}}"#)
        }

        fn error(status: u16, body: &str) -> Self {
            Self::new(status, body)
        }

        fn get_call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }

        fn get_last_calls(&self) -> Vec<(String, serde_json::Value)> {
            self.last_calls.lock().unwrap().clone()
        }

        fn get_get_calls(&self) -> Vec<String> {
            self.get_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TelegramHttpClient for MockTelegramClient {
        async fn post_json(&self, url: &str, body: &serde_json::Value) -> Result<HttpResponse> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.last_calls
                .lock()
                .unwrap()
                .push((url.to_string(), body.clone()));

            Ok(HttpResponse {
                status: self.post_response_status,
                body: self.post_response_body.clone(),
            })
        }

        async fn get_json(&self, url: &str) -> Result<HttpResponse> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.get_calls.lock().unwrap().push(url.to_string());
            Ok(HttpResponse {
                status: self.get_response_status,
                body: self.get_response_body.clone(),
            })
        }
    }

    fn disabled_config() -> TelegramConfig {
        TelegramConfig {
            bot_token: None,
            chat_id: None,
            to_chat_ids: vec![],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        }
    }

    fn enabled_config() -> TelegramConfig {
        TelegramConfig {
            bot_token: Some("123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11".into()),
            chat_id: Some("987654321".to_string()),
            to_chat_ids: vec![],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        }
    }

    fn multi_recipient_config() -> TelegramConfig {
        TelegramConfig {
            bot_token: Some("123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11".into()),
            chat_id: Some("111111111".to_string()),
            to_chat_ids: vec!["222222222".to_string(), "333333333".to_string()],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        }
    }

    fn partial_config_no_token() -> TelegramConfig {
        TelegramConfig {
            bot_token: None,
            chat_id: Some("987654321".to_string()),
            to_chat_ids: vec![],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        }
    }

    fn partial_config_no_chat_id() -> TelegramConfig {
        TelegramConfig {
            bot_token: Some("token".into()),
            chat_id: None,
            to_chat_ids: vec![],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        }
    }

    fn config_with_to_chat_ids_only() -> TelegramConfig {
        TelegramConfig {
            bot_token: Some("token".into()),
            chat_id: None,
            to_chat_ids: vec!["444444444".to_string()],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        }
    }

    // --- Basic trait tests ---

    #[test]
    fn test_name() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        assert_eq!(notifier.name(), "telegram");
    }

    #[test]
    fn test_is_enabled() {
        let notifier = TelegramNotifier::new(enabled_config(), empty_registry());
        assert!(notifier.is_enabled());

        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        assert!(!notifier.is_enabled());
    }

    #[test]
    fn test_is_enabled_partial_configs() {
        assert!(!TelegramNotifier::new(partial_config_no_token(), empty_registry()).is_enabled());
        assert!(!TelegramNotifier::new(partial_config_no_chat_id(), empty_registry()).is_enabled());
    }

    #[test]
    fn test_is_enabled_with_to_chat_ids_only() {
        assert!(
            TelegramNotifier::new(config_with_to_chat_ids_only(), empty_registry()).is_enabled()
        );
    }

    // --- Disabled config tests (no HTTP calls) ---

    #[tokio::test]
    async fn test_notify_start_disabled() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier.notify_start(&issue).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_success_disabled() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier
            .notify_success(&issue, "https://github.com/org/repo/pull/1")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_completed_disabled() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier.notify_completed(&issue).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_failed_disabled() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier.notify_failed(&issue, "Error message").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_failed_long_error() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let long_error = "x".repeat(200);
        let result = notifier.notify_failed(&issue, &long_error).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_status_disabled() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());

        let result = notifier.notify_status("Status update").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_empty() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());

        let result = notifier.notify_urgent_issues(&[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_disabled() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        let issues = vec![
            Issue::new("1", "PROJ-1", "Issue 1", "https://example.com", "linear"),
            Issue::new("2", "PROJ-2", "Issue 2", "https://example.com", "linear"),
        ];

        let result = notifier.notify_urgent_issues(&issues).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_truncated_to_three() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        let issues: Vec<Issue> = (0..10)
            .map(|i| {
                Issue::new(
                    format!("{}", i),
                    format!("PROJ-{}", i),
                    format!("Issue {}", i),
                    "https://example.com",
                    "linear",
                )
            })
            .collect();

        let result = notifier.notify_urgent_issues(&issues).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_new_multiple_recipients() {
        let notifier = TelegramNotifier::new(multi_recipient_config(), empty_registry());
        assert!(notifier.is_enabled());
    }

    // --- Mock-based tests for HTTP-dependent functionality ---

    #[tokio::test]
    async fn test_send_message_success() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        let result = notifier.notify_start(&issue).await;

        assert!(result.is_ok());
        assert_eq!(notifier.http.get_call_count(), 1);
    }

    #[tokio::test]
    async fn test_send_message_verifies_url_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0.contains("api.telegram.org"));
        assert!(calls[0].0.contains("/bot"));
        assert!(calls[0].0.contains("/sendMessage"));
        // Token should be in the URL
        assert!(calls[0]
            .0
            .contains("123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11"));
    }

    #[tokio::test]
    async fn test_send_message_no_auth_header() {
        // Telegram uses token-in-URL, not an auth header.
        // The mock only receives url + body (no auth params), verifying no auth header is used.
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        assert_eq!(calls.len(), 1);
        // The body should contain chat_id, text, and parse_mode
        let body = &calls[0].1;
        assert!(body.get("chat_id").is_some());
        assert!(body.get("text").is_some());
        assert_eq!(body["parse_mode"], "HTML");
    }

    #[tokio::test]
    async fn test_send_message_sends_correct_body() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].1;
        assert_eq!(body["chat_id"], "987654321");
        assert!(body["text"].as_str().unwrap().contains("Processing"));
        assert_eq!(body["parse_mode"], "HTML");
    }

    #[tokio::test]
    async fn test_send_message_multiple_recipients() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(multi_recipient_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        let result = notifier.notify_start(&issue).await;

        assert!(result.is_ok());
        // chat_id + 2 to_chat_ids = 3 calls
        assert_eq!(notifier.http.get_call_count(), 3);
    }

    #[tokio::test]
    async fn test_send_message_error_response() {
        let mock = MockTelegramClient::error(400, r#"{"ok":false,"description":"Bad Request"}"#);
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        let result = notifier.notify_start(&issue).await;

        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("Telegram API error"));
        assert!(err_str.contains("Bad Request"));
    }

    #[tokio::test]
    async fn test_send_message_server_error() {
        let mock = MockTelegramClient::error(500, "Internal server error");
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        let result = notifier.notify_start(&issue).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_message_truncates_long_message() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);

        // Create a message longer than 4096 chars
        let long_message = "x".repeat(5000);
        notifier.notify_status(&long_message).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        // Body should be truncated to 4096 chars + "..."
        assert!(text.len() <= 4200); // "[Claudear] " + truncated body
        assert!(text.ends_with("..."));
    }

    #[tokio::test]
    async fn test_notify_success_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier
            .notify_success(&issue, "https://github.com/org/repo/pull/42")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("[Claudear]"));
        assert!(text.contains("PR Created"));
        assert!(text.contains("PROJ-123"));
        assert!(text.contains("https://github.com/org/repo/pull/42"));
    }

    #[tokio::test]
    async fn test_notify_completed_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier.notify_completed(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("Completed"));
        assert!(text.contains("no PR URL"));
    }

    #[tokio::test]
    async fn test_notify_failed_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier
            .notify_failed(&issue, "Build failed with exit code 1")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("FAILED"));
        assert!(text.contains("PROJ-123"));
        assert!(text.contains("Build failed"));
    }

    #[tokio::test]
    async fn test_notify_failed_truncates_long_error() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        let long_error = "x".repeat(200);
        notifier.notify_failed(&issue, &long_error).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        // Error should be truncated to 100 chars including "..."
        assert!(text.contains("..."));
    }

    #[tokio::test]
    async fn test_notify_status_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);

        notifier.notify_status("System is healthy").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert_eq!(text, "[Claudear] System is healthy");
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issues = vec![
            Issue::new("1", "PROJ-1", "Issue 1", "https://example.com", "linear"),
            Issue::new("2", "PROJ-2", "Issue 2", "https://example.com", "linear"),
        ];

        notifier.notify_urgent_issues(&issues).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("2 urgent issue(s)"));
        assert!(text.contains("PROJ-1"));
        assert!(text.contains("PROJ-2"));
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_truncates_to_three() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issues: Vec<Issue> = (1..=10)
            .map(|i| {
                Issue::new(
                    i.to_string(),
                    format!("PROJ-{}", i),
                    format!("Issue {}", i),
                    "https://example.com",
                    "linear",
                )
            })
            .collect();

        notifier.notify_urgent_issues(&issues).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("10 urgent issue(s)"));
        // Only first 3 are listed
        assert!(text.contains("PROJ-1"));
        assert!(text.contains("PROJ-2"));
        assert!(text.contains("PROJ-3"));
        assert!(!text.contains("PROJ-4"));
    }

    #[tokio::test]
    async fn test_send_message_stops_on_first_error() {
        let mock = MockTelegramClient::error(400, "Bad request");
        let notifier = TelegramNotifier::with_http_client(multi_recipient_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        let result = notifier.notify_start(&issue).await;

        assert!(result.is_err());
        // Should stop after first failure, not try all 3 recipients
        assert_eq!(notifier.http.get_call_count(), 1);
    }

    #[test]
    fn test_with_http_client() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);

        assert!(notifier.is_enabled());
        assert_eq!(notifier.name(), "telegram");
    }

    #[test]
    fn test_reqwest_telegram_client_default() {
        let client = ReqwestTelegramClient::default();
        // Just verify it can be constructed
        assert!(std::mem::size_of_val(&client) > 0);
    }

    #[test]
    fn test_resolve_recipients_returns_config_chat_ids_when_no_issue() {
        let notifier = TelegramNotifier::with_http_client(
            multi_recipient_config(),
            MockTelegramClient::success(),
        );
        let recipients = notifier.resolve_recipients(None);
        assert_eq!(
            recipients,
            vec![
                "111111111".to_string(),
                "222222222".to_string(),
                "333333333".to_string()
            ]
        );
    }

    #[test]
    fn test_resolve_recipients_returns_config_chat_ids_when_no_resolved_user() {
        let notifier =
            TelegramNotifier::with_http_client(enabled_config(), MockTelegramClient::success());
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let recipients = notifier.resolve_recipients(Some(&issue));
        assert_eq!(recipients, vec!["987654321".to_string()]);
    }

    #[test]
    fn test_resolve_recipients_uses_resolved_user_telegram_chat_id() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                telegram_chat_id: Some("555555555".to_string()),
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier = TelegramNotifier::with_http_client_and_registry(
            enabled_config(),
            MockTelegramClient::success(),
            registry,
        );
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "jake");
        let recipients = notifier.resolve_recipients(Some(&issue));
        assert_eq!(recipients, vec!["555555555".to_string()]);
    }

    #[test]
    fn test_resolve_recipients_falls_back_when_user_has_no_telegram() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                telegram_chat_id: None,
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier = TelegramNotifier::with_http_client_and_registry(
            enabled_config(),
            MockTelegramClient::success(),
            registry,
        );
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "jake");
        let recipients = notifier.resolve_recipients(Some(&issue));
        // Falls back to config chat_id
        assert_eq!(recipients, vec!["987654321".to_string()]);
    }

    #[tokio::test]
    async fn test_ask_question_message_contains_question() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test Issue", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-tg-1".to_string(),
            source: "linear".to_string(),
            repo: None,
            issue_id: "1".to_string(),
            short_id: "LIN-1".to_string(),
            question: claudear_core::types::BlockingQuestion {
                question: "Which branch?".to_string(),
                context: None,
                options: vec![],
                why: None,
            },
            asked_at: chrono::Utc::now(),
            target_discord_id: None,
            target_email: None,
            target_slack_id: None,
        };
        notifier.ask_question(&issue, &request).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(!text.contains("[CLAUDEAR-Q:"));
        assert!(text.contains("Human input needed for LIN-1"));
        assert!(text.contains("Which branch?"));
    }

    #[tokio::test]
    async fn test_ask_question_delivery_channel_is_telegram() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-tg-2".to_string(),
            source: "linear".to_string(),
            repo: None,
            issue_id: "1".to_string(),
            short_id: "LIN-1".to_string(),
            question: claudear_core::types::BlockingQuestion {
                question: "Q?".to_string(),
                context: None,
                options: vec![],
                why: None,
            },
            asked_at: chrono::Utc::now(),
            target_discord_id: None,
            target_email: None,
            target_slack_id: None,
        };
        let delivery = notifier
            .ask_question(&issue, &request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.channel, "telegram");
        assert!(delivery.target.is_none());
        assert_eq!(delivery.message_id.as_deref(), Some("42"));
    }

    #[test]
    fn test_supports_replies_enabled_when_notifier_enabled() {
        let notifier =
            TelegramNotifier::with_http_client(enabled_config(), MockTelegramClient::success());
        assert!(notifier.supports_replies());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // single-threaded tokio test; mutex serializes global inbox access
    async fn test_poll_question_replies_via_get_updates_matches_reply_to_ask() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();

        let updates = r#"{
            "ok": true,
            "result": [
                {
                    "update_id": 1001,
                    "message": {
                        "message_id": 77,
                        "date": 1700000001,
                        "chat": { "id": 987654321 },
                        "from": { "id": 999, "is_bot": false, "username": "alice" },
                        "text": "Use feature/telegram-replies",
                        "reply_to_message": {
                            "message_id": 42,
                            "from": { "id": 111, "is_bot": true, "username": "claudear_bot" },
                            "text": "[Claudear] Human input needed for LIN-1: Which branch?"
                        }
                    }
                }
            ]
        }"#;
        let mock = MockTelegramClient::success().with_get_response(200, updates);
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-tg-reply".to_string(),
            source: "linear".to_string(),
            repo: None,
            issue_id: "1".to_string(),
            short_id: "LIN-1".to_string(),
            question: claudear_core::types::BlockingQuestion {
                question: "Which branch?".to_string(),
                context: None,
                options: vec![],
                why: None,
            },
            asked_at: chrono::Utc::now(),
            target_discord_id: None,
            target_email: None,
            target_slack_id: None,
        };

        notifier.ask_question(&issue, &request).await.unwrap();

        let replies = notifier
            .poll_question_replies(
                &request,
                chrono::Utc
                    .timestamp_opt(1_700_000_000, 0)
                    .single()
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].channel, "telegram");
        assert_eq!(replies[0].responder.as_deref(), Some("999"));
        assert_eq!(replies[0].answer, "Use feature/telegram-replies");

        let get_calls = notifier.http.get_get_calls();
        assert_eq!(get_calls.len(), 1);
        assert!(get_calls[0].contains("/getUpdates"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // single-threaded tokio test; mutex serializes global inbox access
    async fn test_poll_question_replies_uses_shared_inbox_when_source_enabled() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();

        let mut cfg = enabled_config();
        cfg.source_enabled = true;
        let mock =
            MockTelegramClient::success().with_get_response(200, r#"{"ok": true, "result": []}"#);
        let notifier = TelegramNotifier::with_http_client(cfg, mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-tg-inbox".to_string(),
            source: "linear".to_string(),
            repo: None,
            issue_id: "1".to_string(),
            short_id: "LIN-1".to_string(),
            question: claudear_core::types::BlockingQuestion {
                question: "Which branch?".to_string(),
                context: None,
                options: vec![],
                why: None,
            },
            asked_at: chrono::Utc::now(),
            target_discord_id: None,
            target_email: None,
            target_slack_id: None,
        };

        notifier.ask_question(&issue, &request).await.unwrap();

        let now = chrono::Utc::now();
        crate::ask_reply_inbox::record_telegram_message(
            crate::ask_reply_inbox::TelegramInboundMessage {
                message_id: 88,
                chat_id: 987654321,
                responder_id: Some("321".to_string()),
                responder_username: Some("jake".to_string()),
                text: "Use main".to_string(),
                replied_at: now,
                reply_to_message_id: Some(42),
                reply_to_text: Some(
                    "[Claudear] Human input needed for LIN-1: Which branch?".to_string(),
                ),
                reply_to_is_bot: Some(true),
            },
        );

        let replies = notifier
            .poll_question_replies(&request, now - chrono::Duration::seconds(1))
            .await
            .unwrap();

        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].answer, "Use main");
        assert!(notifier.http.get_get_calls().is_empty());
    }

    #[tokio::test]
    async fn test_notify_start_message_includes_source_and_title() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "1",
            "SEN-42",
            "Memory leak in worker",
            "https://sentry.io/42",
            "sentry",
        );
        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("SEN-42"));
        assert!(text.contains("sentry"));
        assert!(text.contains("Memory leak in worker"));
    }

    #[tokio::test]
    async fn test_notify_failed_short_error_not_truncated() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");

        notifier.notify_failed(&issue, "Short error").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("Short error"));
        assert!(!text.contains("..."));
    }

    #[tokio::test]
    async fn test_notify_failed_exact_100_char_error_not_truncated() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");

        let error = "x".repeat(100);
        notifier.notify_failed(&issue, &error).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains(&error));
        assert!(!text.ends_with("..."));
    }

    #[tokio::test]
    async fn test_send_message_within_limit_not_truncated() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);

        let message = "x".repeat(100);
        notifier.notify_status(&message).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(!text.ends_with("..."));
    }

    #[tokio::test]
    async fn test_notify_routes_to_resolved_user_telegram_chat_id() {
        let mock = MockTelegramClient::success();
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                telegram_chat_id: Some("999999999".to_string()),
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier =
            TelegramNotifier::with_http_client_and_registry(enabled_config(), mock, registry);
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "jake");

        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        assert_eq!(calls[0].1["chat_id"], "999999999");
    }

    // --- Tests for cascade success message ---

    #[tokio::test]
    async fn test_notify_success_cascade_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "downstream/repo");

        notifier
            .notify_success(&issue, "https://github.com/downstream/repo/pull/5")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("Cascade PR"));
        assert!(text.contains("LIN-1"));
        assert!(text.contains("downstream/repo"));
        assert!(text.contains("https://github.com/downstream/repo/pull/5"));
    }

    // --- Tests for PR update success message ---

    #[tokio::test]
    async fn test_notify_success_pr_update_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("is_pr_update", true);

        notifier
            .notify_success(&issue, "https://github.com/org/repo/pull/77")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("PR Updated"));
        assert!(text.contains("LIN-1"));
        assert!(text.contains("https://github.com/org/repo/pull/77"));
    }

    // --- Tests for regression resolved completed message ---

    #[tokio::test]
    async fn test_notify_completed_regression_resolved_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "SEN-1", "Error", "https://sentry.io/1", "sentry");
        issue.set_metadata("regression_resolved", true);

        notifier.notify_completed(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("Regression Resolved"));
        assert!(text.contains("SEN-1"));
        assert!(text.contains("no regression"));
    }

    // --- Tests for regression detected failed message ---

    #[tokio::test]
    async fn test_notify_failed_regression_detected_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "SEN-1", "Error", "https://sentry.io/1", "sentry");
        issue.set_metadata("regression_detected", true);

        notifier
            .notify_failed(&issue, "Tests failing again")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("REGRESSION"));
        assert!(text.contains("SEN-1"));
        assert!(text.contains("Tests failing again"));
    }

    // --- Tests for cascade failed message ---

    #[tokio::test]
    async fn test_notify_failed_cascade_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "downstream/repo");

        notifier.notify_failed(&issue, "Build error").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("CASCADE FAILED"));
        assert!(text.contains("LIN-1"));
        assert!(text.contains("downstream/repo"));
        assert!(text.contains("Build error"));
    }

    // --- Tests for notify_merged and notify_closed ---

    #[tokio::test]
    async fn test_notify_merged_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Fix", "https://example.com", "linear");

        notifier
            .notify_merged(&issue, "https://github.com/org/repo/pull/42")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("PR Merged"));
        assert!(text.contains("PROJ-1"));
        assert!(text.contains("https://github.com/org/repo/pull/42"));
    }

    #[tokio::test]
    async fn test_notify_closed_message_format() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Fix", "https://example.com", "linear");

        notifier
            .notify_closed(&issue, "https://github.com/org/repo/pull/43")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("PR Closed"));
        assert!(text.contains("PROJ-1"));
        assert!(text.contains("https://github.com/org/repo/pull/43"));
    }

    // --- Test failed cascade with long error truncation ---

    #[tokio::test]
    async fn test_notify_failed_cascade_truncates_long_error() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "downstream/repo");

        let long_error = "e".repeat(200);
        notifier.notify_failed(&issue, &long_error).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("CASCADE FAILED"));
        assert!(text.contains("..."));
    }

    // --- Test regression with long error truncation ---

    #[tokio::test]
    async fn test_notify_failed_regression_truncates_long_error() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "SEN-1", "Error", "https://sentry.io/1", "sentry");
        issue.set_metadata("regression_detected", true);

        let long_error = "r".repeat(200);
        notifier.notify_failed(&issue, &long_error).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("REGRESSION"));
        assert!(text.contains("..."));
    }

    // --- Test parse_mode is always HTML ---

    #[tokio::test]
    async fn test_all_messages_use_html_parse_mode() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");

        notifier.notify_start(&issue).await.unwrap();
        notifier
            .notify_success(&issue, "https://example.com/pr")
            .await
            .unwrap();
        notifier.notify_completed(&issue).await.unwrap();
        notifier.notify_failed(&issue, "error").await.unwrap();
        notifier.notify_status("status").await.unwrap();

        let calls = notifier.http.get_last_calls();
        for (i, call) in calls.iter().enumerate() {
            assert_eq!(
                call.1["parse_mode"], "HTML",
                "Call {} should use HTML parse mode",
                i
            );
        }
    }

    // --- Test multiple recipients get the same text ---

    #[tokio::test]
    async fn test_multiple_recipients_receive_same_text() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(multi_recipient_config(), mock);

        notifier.notify_status("Broadcast message").await.unwrap();

        let calls = notifier.http.get_last_calls();
        assert_eq!(calls.len(), 3);
        let expected_text = "[Claudear] Broadcast message";
        for call in &calls {
            assert_eq!(call.1["text"].as_str().unwrap(), expected_text);
        }
        // Verify different chat_ids
        assert_eq!(calls[0].1["chat_id"], "111111111");
        assert_eq!(calls[1].1["chat_id"], "222222222");
        assert_eq!(calls[2].1["chat_id"], "333333333");
    }

    // --- Test config with only to_chat_ids (no primary chat_id) ---

    #[tokio::test]
    async fn test_config_with_only_to_chat_ids() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(config_with_to_chat_ids_only(), mock);

        notifier.notify_status("test").await.unwrap();

        let calls = notifier.http.get_last_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["chat_id"], "444444444");
    }

    // --- Test http_response_fields ---

    #[test]
    fn test_http_response_fields() {
        let response = HttpResponse {
            status: 201,
            body: "Created".to_string(),
        };
        assert_eq!(response.status, 201);
        assert_eq!(response.body, "Created");
    }

    // --- Additional coverage tests ---

    #[tokio::test]
    async fn test_notify_merged_disabled() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");
        let result = notifier
            .notify_merged(&issue, "https://github.com/pr/1")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_closed_disabled() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");
        let result = notifier
            .notify_closed(&issue, "https://github.com/pr/1")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_ask_question_disabled() {
        let notifier = TelegramNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-tg-disabled".to_string(),
            source: "linear".to_string(),
            repo: None,
            issue_id: "1".to_string(),
            short_id: "LIN-1".to_string(),
            question: claudear_core::types::BlockingQuestion {
                question: "Q?".to_string(),
                context: None,
                options: vec![],
                why: None,
            },
            asked_at: chrono::Utc::now(),
            target_discord_id: None,
            target_email: None,
            target_slack_id: None,
        };
        let result = notifier.ask_question(&issue, &request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_ask_question_api_error_propagates() {
        let mock = MockTelegramClient::error(400, "Bad request");
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-tg-err".to_string(),
            source: "linear".to_string(),
            repo: None,
            issue_id: "1".to_string(),
            short_id: "LIN-1".to_string(),
            question: claudear_core::types::BlockingQuestion {
                question: "Which branch?".to_string(),
                context: None,
                options: vec![],
                why: None,
            },
            asked_at: chrono::Utc::now(),
            target_discord_id: None,
            target_email: None,
            target_slack_id: None,
        };
        let result = notifier.ask_question(&issue, &request).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_recipients_unknown_user_slug_falls_back() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "alice".to_string(),
            claudear_config::config::UserConfig {
                telegram_chat_id: Some("111".to_string()),
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier = TelegramNotifier::with_http_client_and_registry(
            enabled_config(),
            MockTelegramClient::success(),
            registry,
        );
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "unknown_user");
        let recipients = notifier.resolve_recipients(Some(&issue));
        assert_eq!(recipients, vec!["987654321".to_string()]);
    }

    #[tokio::test]
    async fn test_send_message_exactly_4096_not_truncated() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);

        let msg = "x".repeat(4085);
        notifier.notify_status(&msg).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert_eq!(text.len(), 4096);
        assert!(!text.ends_with("..."));
    }

    #[tokio::test]
    async fn test_notify_success_cascade_takes_precedence_over_pr_update() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "downstream/repo");
        issue.set_metadata("is_pr_update", true);

        notifier
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("Cascade PR"));
        assert!(!text.contains("PR Updated"));
    }

    #[tokio::test]
    async fn test_send_message_empty_recipients_no_api_calls() {
        let mock = MockTelegramClient::success();
        let config = TelegramConfig {
            bot_token: Some("token".into()),
            chat_id: None,
            to_chat_ids: vec![],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        };
        let notifier = TelegramNotifier::with_http_client(config, mock);

        let result = notifier.notify_status("hello").await;
        assert!(result.is_ok());
        assert_eq!(notifier.http.get_call_count(), 0);
    }

    #[tokio::test]
    async fn test_notify_status_unicode_passthrough() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);

        notifier
            .notify_status("Status: OK \u{2705} \u{1F680}")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("\u{2705}"));
        assert!(text.contains("\u{1F680}"));
    }

    #[tokio::test]
    async fn test_notify_failed_error_101_chars_gets_truncated() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");

        let error_101 = "x".repeat(101);
        notifier.notify_failed(&issue, &error_101).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("..."));
        assert!(!text.contains(&error_101));
    }

    #[tokio::test]
    async fn test_notify_success_normal_pr_created() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");

        notifier
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("PR Created"));
        assert!(!text.contains("Cascade"));
        assert!(!text.contains("Updated"));
    }

    #[tokio::test]
    async fn test_notify_completed_normal_no_regression() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");

        notifier.notify_completed(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("Completed"));
        assert!(text.contains("no PR URL"));
        assert!(!text.contains("Regression"));
    }

    #[tokio::test]
    async fn test_notify_failed_normal_no_regression_no_cascade() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");

        notifier
            .notify_failed(&issue, "compile error")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("FAILED"));
        assert!(text.contains("compile error"));
        assert!(!text.contains("REGRESSION"));
        assert!(!text.contains("CASCADE"));
    }

    #[tokio::test]
    async fn test_send_message_error_response_299() {
        let mock = MockTelegramClient::new(299, "OK");
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.notify_status("test").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_message_error_response_300() {
        let mock = MockTelegramClient::new(300, "Redirect");
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.notify_status("test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_url_contains_bot_prefix() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);

        notifier.notify_status("test").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let url = &calls[0].0;
        assert!(url.starts_with("https://api.telegram.org/bot"));
        assert!(url.ends_with("/sendMessage"));
    }

    #[tokio::test]
    async fn test_duplicate_chat_id_in_to_chat_ids() {
        let mock = MockTelegramClient::success();
        let config = TelegramConfig {
            bot_token: Some("token".into()),
            chat_id: Some("111".to_string()),
            to_chat_ids: vec!["111".to_string()],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        };
        let notifier = TelegramNotifier::with_http_client(config, mock);

        notifier.notify_status("test").await.unwrap();

        assert_eq!(notifier.http.get_call_count(), 2);
    }

    #[tokio::test]
    async fn test_notify_start_with_trigger_reason() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("trigger_reason", "Retry attempt 2: timeout");
        notifier.notify_start(&issue).await.unwrap();
        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("Trigger: Retry attempt 2: timeout"));
    }

    #[tokio::test]
    async fn test_notify_start_without_trigger_reason() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        notifier.notify_start(&issue).await.unwrap();
        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(!text.contains("Trigger:"));
    }

    #[tokio::test]
    async fn test_notify_failed_with_trigger_reason() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("trigger_reason", "Manual trigger");
        notifier.notify_failed(&issue, "some error").await.unwrap();
        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("Trigger: Manual trigger"));
    }

    #[test]
    fn test_extract_reply_text_normal() {
        assert_eq!(
            TelegramNotifier::<MockTelegramClient>::extract_reply_text("hello"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn test_extract_reply_text_empty() {
        assert_eq!(
            TelegramNotifier::<MockTelegramClient>::extract_reply_text(""),
            None
        );
    }

    #[test]
    fn test_extract_reply_text_whitespace_only() {
        assert_eq!(
            TelegramNotifier::<MockTelegramClient>::extract_reply_text("   \n\t  "),
            None
        );
    }

    #[test]
    fn test_extract_reply_text_trims_whitespace() {
        assert_eq!(
            TelegramNotifier::<MockTelegramClient>::extract_reply_text("  answer  "),
            Some("answer".to_string())
        );
    }

    #[test]
    fn test_poll_chat_ids_empty_config() {
        let config = TelegramConfig {
            bot_token: Some("tok".into()),
            chat_id: None,
            to_chat_ids: vec![],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        };
        let notifier = TelegramNotifier::with_http_client(config, MockTelegramClient::success());
        let ids = notifier.poll_chat_ids();
        assert!(ids.is_empty());
    }

    #[test]
    fn test_poll_chat_ids_collects_all_sources() {
        let config = TelegramConfig {
            bot_token: Some("tok".into()),
            chat_id: Some("111".to_string()),
            to_chat_ids: vec!["222".to_string(), "333".to_string()],
            source_enabled: false,
            listen_chat_id: Some("444".to_string()),
            poll_interval_ms: None,
        };
        let notifier = TelegramNotifier::with_http_client(config, MockTelegramClient::success());
        let ids = notifier.poll_chat_ids();
        assert!(ids.contains(&111));
        assert!(ids.contains(&222));
        assert!(ids.contains(&333));
        assert!(ids.contains(&444));
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn test_poll_chat_ids_skips_non_numeric() {
        let config = TelegramConfig {
            bot_token: Some("tok".into()),
            chat_id: Some("not_a_number".to_string()),
            to_chat_ids: vec!["222".to_string()],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        };
        let notifier = TelegramNotifier::with_http_client(config, MockTelegramClient::success());
        let ids = notifier.poll_chat_ids();
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(&222));
    }

    #[test]
    fn test_poll_chat_ids_deduplicates() {
        let config = TelegramConfig {
            bot_token: Some("tok".into()),
            chat_id: Some("111".to_string()),
            to_chat_ids: vec!["111".to_string()],
            source_enabled: false,
            listen_chat_id: Some("111".to_string()),
            poll_interval_ms: None,
        };
        let notifier = TelegramNotifier::with_http_client(config, MockTelegramClient::success());
        let ids = notifier.poll_chat_ids();
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(&111));
    }

    #[test]
    fn test_telegram_send_message_api_response_deserialize_success() {
        let json = r#"{"ok": true, "result": {"message_id": 42}}"#;
        let parsed: TelegramSendMessageApiResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.result.unwrap().message_id, 42);
    }

    #[test]
    fn test_telegram_send_message_api_response_deserialize_no_result() {
        let json = r#"{"ok": false}"#;
        let parsed: TelegramSendMessageApiResponse = serde_json::from_str(json).unwrap();
        assert!(!parsed.ok);
        assert!(parsed.result.is_none());
    }

    #[test]
    fn test_telegram_get_updates_response_deserialize_success() {
        let json = r#"{"ok": true, "result": [{"update_id": 100, "message": {"message_id": 1, "chat": {"id": 555}, "text": "hello", "date": 1700000000}}]}"#;
        let parsed: TelegramGetUpdatesResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.result.len(), 1);
        assert_eq!(parsed.result[0].update_id, 100);
        let msg = parsed.result[0].message.as_ref().unwrap();
        assert_eq!(msg.message_id, 1);
        assert_eq!(msg.chat.id, 555);
        assert_eq!(msg.text.as_deref(), Some("hello"));
    }

    #[test]
    fn test_telegram_get_updates_response_deserialize_empty() {
        let json = r#"{"ok": true, "result": []}"#;
        let parsed: TelegramGetUpdatesResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.ok);
        assert!(parsed.result.is_empty());
    }

    #[test]
    fn test_telegram_update_item_without_message() {
        let json = r#"{"ok": true, "result": [{"update_id": 200}]}"#;
        let parsed: TelegramGetUpdatesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.result.len(), 1);
        assert!(parsed.result[0].message.is_none());
    }

    #[test]
    fn test_telegram_inbound_message_with_reply() {
        let json = r#"{
            "message_id": 10,
            "chat": {"id": 999},
            "from": {"id": 123, "is_bot": false, "username": "testuser"},
            "text": "my reply",
            "date": 1700000000,
            "reply_to_message": {"message_id": 5, "from": {"id": 456, "is_bot": true}, "text": "original question"}
        }"#;
        let parsed: TelegramInboundApiMessage = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.message_id, 10);
        assert_eq!(parsed.chat.id, 999);
        assert_eq!(parsed.from.as_ref().unwrap().id, 123);
        assert!(!parsed.from.as_ref().unwrap().is_bot);
        assert_eq!(
            parsed.from.as_ref().unwrap().username.as_deref(),
            Some("testuser")
        );
        assert_eq!(parsed.text.as_deref(), Some("my reply"));
        let reply = parsed.reply_to_message.as_ref().unwrap();
        assert_eq!(reply.message_id, 5);
        assert!(reply.from.as_ref().unwrap().is_bot);
        assert_eq!(reply.text.as_deref(), Some("original question"));
    }

    #[test]
    fn test_telegram_inbound_message_minimal() {
        let json = r#"{"message_id": 1, "chat": {"id": 100}}"#;
        let parsed: TelegramInboundApiMessage = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.message_id, 1);
        assert!(parsed.from.is_none());
        assert!(parsed.text.is_none());
        assert!(parsed.date.is_none());
        assert!(parsed.reply_to_message.is_none());
    }

    #[tokio::test]
    async fn test_send_message_with_ids_ok_false_no_id_returned() {
        let mock = MockTelegramClient::new(200, r#"{"ok": false}"#);
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);

        let ids = notifier.send_message_with_ids("test", None).await.unwrap();
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn test_send_message_with_ids_no_bot_token_returns_empty() {
        let config = TelegramConfig {
            bot_token: None,
            chat_id: Some("111".to_string()),
            to_chat_ids: vec![],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        };
        let notifier = TelegramNotifier::with_http_client(config, MockTelegramClient::success());

        let ids = notifier.send_message_with_ids("test", None).await.unwrap();
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn test_ingest_updates_no_bot_token_noop() {
        let config = TelegramConfig {
            bot_token: None,
            chat_id: Some("111".to_string()),
            to_chat_ids: vec![],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        };
        let notifier = TelegramNotifier::with_http_client(config, MockTelegramClient::success());
        let result = notifier.ingest_updates_into_reply_inbox().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_ingest_updates_error_response() {
        let mock = MockTelegramClient::success().with_get_response(400, "Bad request");
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.ingest_updates_into_reply_inbox().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ingest_updates_ok_false_returns_error() {
        let mock =
            MockTelegramClient::success().with_get_response(200, r#"{"ok": false, "result": []}"#);
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.ingest_updates_into_reply_inbox().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ingest_updates_invalid_json_returns_error() {
        let mock = MockTelegramClient::success().with_get_response(200, "not json");
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.ingest_updates_into_reply_inbox().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ingest_updates_success_records_messages() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let updates_json = r#"{
            "ok": true,
            "result": [{
                "update_id": 500,
                "message": {
                    "message_id": 10,
                    "chat": {"id": 555},
                    "from": {"id": 123, "is_bot": false, "username": "alice"},
                    "text": "hello bot",
                    "date": 1700000000
                }
            }]
        }"#;
        let mock = MockTelegramClient::success().with_get_response(200, updates_json);
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.ingest_updates_into_reply_inbox().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_ingest_updates_tracks_last_update_id() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let updates_json = r#"{
            "ok": true,
            "result": [
                {"update_id": 100, "message": {"message_id": 1, "chat": {"id": 555}, "from": {"id": 1, "is_bot": false}, "text": "a", "date": 1700000000}},
                {"update_id": 200, "message": {"message_id": 2, "chat": {"id": 555}, "from": {"id": 1, "is_bot": false}, "text": "b", "date": 1700000001}}
            ]
        }"#;
        let mock = MockTelegramClient::success().with_get_response(200, updates_json);
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        notifier.ingest_updates_into_reply_inbox().await.unwrap();

        let last_id = notifier.reply_last_update_id.read().unwrap();
        assert_eq!(*last_id, Some(200));
    }

    #[tokio::test]
    async fn test_ingest_updates_skips_bot_messages() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let updates_json = r#"{
            "ok": true,
            "result": [{
                "update_id": 300,
                "message": {
                    "message_id": 10,
                    "chat": {"id": 555},
                    "from": {"id": 999, "is_bot": true},
                    "text": "bot message",
                    "date": 1700000000
                }
            }]
        }"#;
        let mock = MockTelegramClient::success().with_get_response(200, updates_json);
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.ingest_updates_into_reply_inbox().await;
        assert!(result.is_ok());
        // Bot messages should be skipped in record_inbound_message_for_replies
    }

    #[tokio::test]
    async fn test_ingest_updates_skips_empty_text() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let updates_json = r#"{
            "ok": true,
            "result": [{
                "update_id": 400,
                "message": {
                    "message_id": 10,
                    "chat": {"id": 555},
                    "from": {"id": 123, "is_bot": false},
                    "text": "   ",
                    "date": 1700000000
                }
            }]
        }"#;
        let mock = MockTelegramClient::success().with_get_response(200, updates_json);
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.ingest_updates_into_reply_inbox().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_ingest_updates_skips_no_text() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let updates_json = r#"{
            "ok": true,
            "result": [{
                "update_id": 410,
                "message": {
                    "message_id": 10,
                    "chat": {"id": 555},
                    "from": {"id": 123, "is_bot": false},
                    "date": 1700000000
                }
            }]
        }"#;
        let mock = MockTelegramClient::success().with_get_response(200, updates_json);
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.ingest_updates_into_reply_inbox().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_ingest_updates_skips_no_from() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let updates_json = r#"{
            "ok": true,
            "result": [{
                "update_id": 420,
                "message": {
                    "message_id": 10,
                    "chat": {"id": 555},
                    "text": "anonymous",
                    "date": 1700000000
                }
            }]
        }"#;
        let mock = MockTelegramClient::success().with_get_response(200, updates_json);
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.ingest_updates_into_reply_inbox().await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_record_inbound_message_with_date() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let msg = TelegramInboundApiMessage {
            message_id: 42,
            chat: TelegramInboundApiChat { id: 100 },
            from: Some(TelegramInboundApiUser {
                id: 5,
                is_bot: false,
                username: Some("user5".to_string()),
            }),
            text: Some("test message".to_string()),
            date: Some(1700000000),
            reply_to_message: None,
        };
        TelegramNotifier::<MockTelegramClient>::record_inbound_message_for_replies(&msg);
        // Should not panic, message recorded
    }

    #[test]
    fn test_record_inbound_message_without_date_uses_now() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let msg = TelegramInboundApiMessage {
            message_id: 43,
            chat: TelegramInboundApiChat { id: 100 },
            from: Some(TelegramInboundApiUser {
                id: 6,
                is_bot: false,
                username: None,
            }),
            text: Some("no date msg".to_string()),
            date: None,
            reply_to_message: None,
        };
        TelegramNotifier::<MockTelegramClient>::record_inbound_message_for_replies(&msg);
    }

    #[test]
    fn test_record_inbound_message_with_reply_to() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let msg = TelegramInboundApiMessage {
            message_id: 44,
            chat: TelegramInboundApiChat { id: 100 },
            from: Some(TelegramInboundApiUser {
                id: 7,
                is_bot: false,
                username: Some("bob".to_string()),
            }),
            text: Some("my reply".to_string()),
            date: Some(1700000000),
            reply_to_message: Some(TelegramInboundApiReplyMessage {
                message_id: 40,
                from: Some(TelegramInboundApiUser {
                    id: 999,
                    is_bot: true,
                    username: Some("mybot".to_string()),
                }),
                text: Some("original question text".to_string()),
            }),
        };
        TelegramNotifier::<MockTelegramClient>::record_inbound_message_for_replies(&msg);
    }

    #[test]
    fn test_supports_replies_enabled() {
        let notifier =
            TelegramNotifier::with_http_client(enabled_config(), MockTelegramClient::success());
        assert!(notifier.supports_replies());
    }

    #[test]
    fn test_supports_replies_disabled() {
        let config = TelegramConfig {
            bot_token: None,
            chat_id: None,
            to_chat_ids: vec![],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        };
        let notifier = TelegramNotifier::with_http_client(config, MockTelegramClient::success());
        assert!(!notifier.supports_replies());
    }

    #[tokio::test]
    async fn test_notify_success_with_trigger_reason() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("trigger_reason", "Manual retry");
        notifier
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .unwrap();
        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("PR Created"));
        assert!(text.contains("Trigger: Manual retry"));
    }

    #[tokio::test]
    async fn test_notify_completed_with_completion_reason() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("completion_reason", "Already resolved");
        notifier.notify_completed(&issue).await.unwrap();
        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("Completed"));
        assert!(text.contains("Already resolved"));
        assert!(!text.contains("no PR URL"));
    }

    #[tokio::test]
    async fn test_resolve_recipients_uses_user_registry() {
        let mock = MockTelegramClient::success();
        let mut users = std::collections::HashMap::new();
        users.insert(
            "alice".to_string(),
            claudear_config::config::UserConfig {
                telegram_chat_id: Some("99999".to_string()),
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier =
            TelegramNotifier::with_http_client_and_registry(enabled_config(), mock, registry);
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "alice");
        let recipients = notifier.resolve_recipients(Some(&issue));
        assert_eq!(recipients, vec!["99999".to_string()]);
    }

    #[tokio::test]
    async fn test_resolve_recipients_fallback_when_no_telegram_chat_id() {
        let mock = MockTelegramClient::success();
        let mut users = std::collections::HashMap::new();
        users.insert(
            "alice".to_string(),
            claudear_config::config::UserConfig {
                telegram_chat_id: None,
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier =
            TelegramNotifier::with_http_client_and_registry(enabled_config(), mock, registry);
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "alice");
        let recipients = notifier.resolve_recipients(Some(&issue));
        // Falls back to config chat_id
        assert!(recipients.contains(&"987654321".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_recipients_no_issue() {
        let config = TelegramConfig {
            bot_token: Some("tok".into()),
            chat_id: Some("aaa".to_string()),
            to_chat_ids: vec!["bbb".to_string()],
            source_enabled: false,
            listen_chat_id: None,
            poll_interval_ms: None,
        };
        let notifier = TelegramNotifier::with_http_client(config, MockTelegramClient::success());
        let recipients = notifier.resolve_recipients(None);
        assert_eq!(recipients, vec!["aaa".to_string(), "bbb".to_string()]);
    }

    #[tokio::test]
    async fn test_send_message_truncates_long_text() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);

        let long_msg = "x".repeat(5000);
        notifier.notify_status(&long_msg).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.len() <= 4200);
        assert!(text.ends_with("..."));
    }

    #[tokio::test]
    async fn test_notify_success_with_confidence() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("confidence", 85u8);
        notifier
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .unwrap();
        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.contains("Fix Confidence: 85/100"));
    }

    #[tokio::test]
    async fn test_notify_success_without_confidence() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        notifier
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .unwrap();
        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(!text.contains("Fix Confidence"));
    }

    #[tokio::test]
    async fn test_send_message_short_text_not_truncated() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);

        notifier.notify_status("short msg").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(!text.ends_with("..."));
    }

    #[tokio::test]
    async fn test_send_message_includes_parse_mode() {
        let mock = MockTelegramClient::success();
        let notifier = TelegramNotifier::with_http_client(enabled_config(), mock);

        notifier.notify_status("test").await.unwrap();

        let calls = notifier.http.get_last_calls();
        assert_eq!(calls[0].1["parse_mode"], "HTML");
    }
}
