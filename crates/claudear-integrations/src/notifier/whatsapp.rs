//! WhatsApp notifier via WhatsApp Business Cloud API.

use super::Notifier;
use crate::ask_reply_inbox;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use claudear_config::config::WhatsAppConfig;
use claudear_config::users::UserRegistry;
use claudear_core::error::{Error, Result};
use claudear_core::http::HttpResponse;
use claudear_core::types::{AskDelivery, AskReply, AskRequest, Issue};
use serde::Deserialize;
use std::collections::HashSet;

/// Trait for HTTP client used by WhatsApp notifier.
#[async_trait]
pub trait WhatsAppHttpClient: Send + Sync {
    async fn post_json(
        &self,
        url: &str,
        bearer_token: &str,
        body: &serde_json::Value,
    ) -> Result<HttpResponse>;
}

/// Real HTTP client using reqwest.
pub struct ReqwestWhatsAppClient {
    client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct WhatsAppSendMessageResponse {
    #[serde(default)]
    messages: Vec<WhatsAppSendMessageResult>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppSendMessageResult {
    id: String,
}

impl ReqwestWhatsAppClient {
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

impl Default for ReqwestWhatsAppClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WhatsAppHttpClient for ReqwestWhatsAppClient {
    async fn post_json(
        &self,
        url: &str,
        bearer_token: &str,
        body: &serde_json::Value,
    ) -> Result<HttpResponse> {
        let response = self
            .client
            .post(url)
            .bearer_auth(bearer_token)
            .json(body)
            .send()
            .await?;

        let status = response.status().as_u16();
        let resp_body = response.text().await.unwrap_or_default();

        Ok(HttpResponse {
            status,
            body: resp_body,
        })
    }
}

/// WhatsApp notifier that sends notifications via WhatsApp Business Cloud API.
pub struct WhatsAppNotifier<H: WhatsAppHttpClient = ReqwestWhatsAppClient> {
    config: WhatsAppConfig,
    http: H,
    user_registry: UserRegistry,
}

impl WhatsAppNotifier<ReqwestWhatsAppClient> {
    /// Create a new WhatsApp notifier.
    pub fn new(config: WhatsAppConfig, user_registry: UserRegistry) -> Self {
        Self {
            config,
            http: ReqwestWhatsAppClient::new(),
            user_registry,
        }
    }
}

impl<H: WhatsAppHttpClient> WhatsAppNotifier<H> {
    /// Create a new WhatsApp notifier with custom HTTP client.
    pub fn with_http_client(config: WhatsAppConfig, http: H) -> Self {
        Self {
            config,
            http,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
        }
    }

    /// Create a new WhatsApp notifier with custom HTTP client and user registry.
    pub fn with_http_client_and_registry(
        config: WhatsAppConfig,
        http: H,
        user_registry: UserRegistry,
    ) -> Self {
        Self {
            config,
            http,
            user_registry,
        }
    }

    fn resolve_recipients(&self, issue: Option<&Issue>) -> Vec<String> {
        if let Some(issue) = issue {
            if let Some(slug) = issue.get_metadata::<String>("resolved_user") {
                if let Some(user) = self.user_registry.get_by_slug(&slug) {
                    if let Some(ref number) = user.whatsapp_number {
                        return vec![number.clone()];
                    }
                }
            }
        }
        self.config.to_numbers.clone()
    }

    fn extract_reply_text(content: &str) -> Option<String> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    async fn send_message_with_ids(
        &self,
        body: &str,
        issue: Option<&Issue>,
    ) -> Result<Vec<String>> {
        let (phone_number_id, access_token) =
            match (&self.config.phone_number_id, &self.config.access_token) {
                (Some(pid), Some(token)) => (pid, token.expose()),
                _ => return Ok(Vec::new()),
            };

        let url = format!(
            "https://graph.facebook.com/v21.0/{}/messages",
            phone_number_id
        );

        // Truncate message to WhatsApp limit (4096 chars)
        let truncated_body = if body.len() > 4096 {
            format!("{}...", &body[..body.floor_char_boundary(4093)])
        } else {
            body.to_string()
        };

        let recipients = self.resolve_recipients(issue);

        let mut sent_message_ids = Vec::new();

        for to_number in &recipients {
            let payload = serde_json::json!({
                "messaging_product": "whatsapp",
                "to": to_number,
                "type": "text",
                "text": {
                    "body": truncated_body
                }
            });

            let response = self.http.post_json(&url, access_token, &payload).await?;

            if response.status < 200 || response.status >= 300 {
                return Err(Error::notifier(
                    "whatsapp",
                    format!("WhatsApp API error: {}", response.body),
                ));
            }

            if let Ok(parsed) = serde_json::from_str::<WhatsAppSendMessageResponse>(&response.body)
            {
                if let Some(first) = parsed.messages.into_iter().next() {
                    if !first.id.trim().is_empty() {
                        sent_message_ids.push(first.id);
                    }
                }
            }
        }

        Ok(sent_message_ids)
    }

    async fn send_message(&self, body: &str, issue: Option<&Issue>) -> Result<()> {
        let _ = self.send_message_with_ids(body, issue).await?;
        Ok(())
    }
}

#[async_trait]
impl<H: WhatsAppHttpClient + 'static> Notifier for WhatsAppNotifier<H> {
    fn name(&self) -> &str {
        "whatsapp"
    }

    fn is_enabled(&self) -> bool {
        self.config.phone_number_id.is_some()
            && self.config.access_token.is_some()
            && !self.config.to_numbers.is_empty()
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
                "whatsapp",
                &request.correlation_id,
                message_id.clone(),
            );
        }
        Ok(Some(AskDelivery {
            channel: "whatsapp".to_string(),
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
            return Ok(Vec::new());
        }

        let ask_ids: HashSet<String> =
            ask_reply_inbox::ask_delivery_ids("whatsapp", &request.correlation_id)
                .into_iter()
                .collect();
        let configured_senders: HashSet<String> = self.config.to_numbers.iter().cloned().collect();
        let allow_single_sender_fallback = configured_senders.len() == 1;

        let mut replies = Vec::new();
        for msg in ask_reply_inbox::whatsapp_messages_since(since) {
            let is_reply_to_known_ask = msg
                .context_message_id
                .as_ref()
                .map(|id| ask_ids.contains(id))
                .unwrap_or(false);

            let is_single_sender_fallback = !is_reply_to_known_ask
                && allow_single_sender_fallback
                && msg.context_message_id.is_none()
                && configured_senders.contains(&msg.from);

            if !is_reply_to_known_ask && !is_single_sender_fallback {
                continue;
            }

            let answer = match Self::extract_reply_text(&msg.text) {
                Some(v) => v,
                None => continue,
            };

            replies.push(AskReply {
                correlation_id: request.correlation_id.clone(),
                channel: "whatsapp".to_string(),
                responder: Some(msg.from),
                answer,
                replied_at: msg.replied_at,
            });
        }

        replies.sort_by_key(|r| r.replied_at);
        Ok(replies)
    }

    fn supports_replies(&self) -> bool {
        self.is_enabled() && self.config.source_enabled
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

    /// Mock WhatsApp HTTP client for testing.
    struct MockWhatsAppClient {
        response_status: u16,
        response_body: String,
        call_count: AtomicUsize,
        last_calls: Mutex<Vec<(String, String, serde_json::Value)>>,
    }

    impl MockWhatsAppClient {
        fn new(status: u16, body: &str) -> Self {
            Self {
                response_status: status,
                response_body: body.to_string(),
                call_count: AtomicUsize::new(0),
                last_calls: Mutex::new(Vec::new()),
            }
        }

        fn success() -> Self {
            Self::new(
                200,
                r#"{"messaging_product":"whatsapp","contacts":[{"wa_id":"15559876543"}],"messages":[{"id":"wamid.xxx"}]}"#,
            )
        }

        fn error(status: u16, body: &str) -> Self {
            Self::new(status, body)
        }

        fn get_call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }

        fn get_last_calls(&self) -> Vec<(String, String, serde_json::Value)> {
            self.last_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WhatsAppHttpClient for MockWhatsAppClient {
        async fn post_json(
            &self,
            url: &str,
            bearer_token: &str,
            body: &serde_json::Value,
        ) -> Result<HttpResponse> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.last_calls.lock().unwrap().push((
                url.to_string(),
                bearer_token.to_string(),
                body.clone(),
            ));

            Ok(HttpResponse {
                status: self.response_status,
                body: self.response_body.clone(),
            })
        }
    }

    fn disabled_config() -> WhatsAppConfig {
        WhatsAppConfig {
            phone_number_id: None,
            access_token: None,
            business_account_id: None,
            app_secret: None,
            webhook_verify_token: None,
            to_numbers: vec![],
            source_enabled: false,
            listen_phone_number_id: None,
            poll_interval_ms: None,
        }
    }

    fn enabled_config() -> WhatsAppConfig {
        WhatsAppConfig {
            phone_number_id: Some("123456789".to_string()),
            access_token: Some("access_token_xyz".into()),
            business_account_id: None,
            app_secret: None,
            webhook_verify_token: None,
            to_numbers: vec!["+15559876543".to_string()],
            source_enabled: false,
            listen_phone_number_id: None,
            poll_interval_ms: None,
        }
    }

    fn multi_recipient_config() -> WhatsAppConfig {
        WhatsAppConfig {
            phone_number_id: Some("123456789".to_string()),
            access_token: Some("access_token_xyz".into()),
            business_account_id: None,
            app_secret: None,
            webhook_verify_token: None,
            to_numbers: vec![
                "+15551111111".to_string(),
                "+15552222222".to_string(),
                "+15553333333".to_string(),
            ],
            source_enabled: false,
            listen_phone_number_id: None,
            poll_interval_ms: None,
        }
    }

    fn partial_config_no_phone_id() -> WhatsAppConfig {
        WhatsAppConfig {
            phone_number_id: None,
            access_token: Some("token".into()),
            business_account_id: None,
            app_secret: None,
            webhook_verify_token: None,
            to_numbers: vec!["+0987654321".to_string()],
            source_enabled: false,
            listen_phone_number_id: None,
            poll_interval_ms: None,
        }
    }

    fn partial_config_no_token() -> WhatsAppConfig {
        WhatsAppConfig {
            phone_number_id: Some("pid".to_string()),
            access_token: None,
            business_account_id: None,
            app_secret: None,
            webhook_verify_token: None,
            to_numbers: vec!["+0987654321".to_string()],
            source_enabled: false,
            listen_phone_number_id: None,
            poll_interval_ms: None,
        }
    }

    fn partial_config_no_to() -> WhatsAppConfig {
        WhatsAppConfig {
            phone_number_id: Some("pid".to_string()),
            access_token: Some("token".into()),
            business_account_id: None,
            app_secret: None,
            webhook_verify_token: None,
            to_numbers: vec![],
            source_enabled: false,
            listen_phone_number_id: None,
            poll_interval_ms: None,
        }
    }

    // --- Basic trait tests ---

    #[test]
    fn test_name() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
        assert_eq!(notifier.name(), "whatsapp");
    }

    #[test]
    fn test_is_enabled() {
        let notifier = WhatsAppNotifier::new(enabled_config(), empty_registry());
        assert!(notifier.is_enabled());

        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
        assert!(!notifier.is_enabled());
    }

    #[test]
    fn test_is_enabled_partial_configs() {
        assert!(
            !WhatsAppNotifier::new(partial_config_no_phone_id(), empty_registry()).is_enabled()
        );
        assert!(!WhatsAppNotifier::new(partial_config_no_token(), empty_registry()).is_enabled());
        assert!(!WhatsAppNotifier::new(partial_config_no_to(), empty_registry()).is_enabled());
    }

    // --- Disabled config tests (silent no-op) ---

    #[tokio::test]
    async fn test_notify_start_disabled() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier.notify_start(&issue).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_success_disabled() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier
            .notify_success(&issue, "https://github.com/org/repo/pull/1")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_completed_disabled() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier.notify_completed(&issue).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_failed_disabled() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier.notify_failed(&issue, "Error message").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_failed_long_error() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let long_error = "x".repeat(200);
        let result = notifier.notify_failed(&issue, &long_error).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_status_disabled() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());

        let result = notifier.notify_status("Status update").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_empty() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());

        let result = notifier.notify_urgent_issues(&[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_disabled() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
        let issues = vec![
            Issue::new("1", "PROJ-1", "Issue 1", "https://example.com", "linear"),
            Issue::new("2", "PROJ-2", "Issue 2", "https://example.com", "linear"),
        ];

        let result = notifier.notify_urgent_issues(&issues).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_truncated_to_three() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
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
        let notifier = WhatsAppNotifier::new(multi_recipient_config(), empty_registry());
        assert!(notifier.is_enabled());
    }

    // --- Mock-based tests for HTTP-dependent functionality ---

    #[tokio::test]
    async fn test_send_message_success() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
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
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
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
        assert!(calls[0].0.contains("graph.facebook.com"));
        assert!(calls[0].0.contains("v21.0"));
        assert!(calls[0].0.contains("123456789")); // phone_number_id in URL
        assert!(calls[0].0.contains("messages"));
    }

    #[tokio::test]
    async fn test_send_message_uses_bearer_auth() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        assert_eq!(calls[0].1, "access_token_xyz"); // bearer_token
    }

    #[tokio::test]
    async fn test_send_message_sends_correct_json_body() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].2;
        assert_eq!(body["messaging_product"], "whatsapp");
        assert_eq!(body["to"], "+15559876543");
        assert_eq!(body["type"], "text");
        assert!(body["text"]["body"]
            .as_str()
            .unwrap()
            .contains("Processing"));
    }

    #[tokio::test]
    async fn test_send_message_multiple_recipients() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(multi_recipient_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        let result = notifier.notify_start(&issue).await;

        assert!(result.is_ok());
        assert_eq!(notifier.http.get_call_count(), 3); // One call per recipient
    }

    #[tokio::test]
    async fn test_send_message_error_response() {
        let mock = MockWhatsAppClient::error(400, "Invalid phone number");
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
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
        assert!(err_str.contains("WhatsApp API error"));
        assert!(err_str.contains("Invalid phone number"));
    }

    #[tokio::test]
    async fn test_send_message_server_error() {
        let mock = MockWhatsAppClient::error(500, "Internal server error");
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
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
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);

        // Create a message longer than 4096 chars
        let long_message = "x".repeat(5000);
        notifier.notify_status(&long_message).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body_text = calls[0].2["text"]["body"].as_str().unwrap();
        // Body should be truncated to 4096 chars + "..."
        assert!(body_text.len() <= 4200); // "[Claudear] " + truncated body
        assert!(body_text.ends_with("..."));
    }

    #[tokio::test]
    async fn test_notify_success_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
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
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("[Claudear]"));
        assert!(body.contains("PR Created"));
        assert!(body.contains("PROJ-123"));
        assert!(body.contains("https://github.com/org/repo/pull/42"));
    }

    #[tokio::test]
    async fn test_notify_completed_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier.notify_completed(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("Completed"));
        assert!(body.contains("no PR URL"));
    }

    #[tokio::test]
    async fn test_notify_failed_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
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
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("FAILED"));
        assert!(body.contains("PROJ-123"));
        assert!(body.contains("Build failed"));
    }

    #[tokio::test]
    async fn test_notify_failed_truncates_long_error() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
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
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        // Error should be truncated to 100 chars including "..."
        assert!(body.contains("..."));
    }

    #[tokio::test]
    async fn test_notify_status_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);

        notifier.notify_status("System is healthy").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert_eq!(body, "[Claudear] System is healthy");
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issues = vec![
            Issue::new("1", "PROJ-1", "Issue 1", "https://example.com", "linear"),
            Issue::new("2", "PROJ-2", "Issue 2", "https://example.com", "linear"),
        ];

        notifier.notify_urgent_issues(&issues).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("2 urgent issue(s)"));
        assert!(body.contains("PROJ-1"));
        assert!(body.contains("PROJ-2"));
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_truncates_to_three() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
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
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("10 urgent issue(s)"));
        // Only first 3 are listed
        assert!(body.contains("PROJ-1"));
        assert!(body.contains("PROJ-2"));
        assert!(body.contains("PROJ-3"));
        assert!(!body.contains("PROJ-4"));
    }

    #[tokio::test]
    async fn test_send_message_stops_on_first_error() {
        let mock = MockWhatsAppClient::error(400, "Bad request");
        let notifier = WhatsAppNotifier::with_http_client(multi_recipient_config(), mock);
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
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);

        assert!(notifier.is_enabled());
        assert_eq!(notifier.name(), "whatsapp");
    }

    #[test]
    fn test_reqwest_whatsapp_client_default() {
        let client = ReqwestWhatsAppClient::default();
        // Just verify it can be constructed
        assert!(std::mem::size_of_val(&client) > 0);
    }

    #[test]
    fn test_resolve_recipients_returns_config_numbers_when_no_issue() {
        let config = WhatsAppConfig {
            phone_number_id: Some("pid".to_string()),
            access_token: Some("token".into()),
            business_account_id: None,
            app_secret: None,
            webhook_verify_token: None,
            to_numbers: vec!["+1111".to_string(), "+2222".to_string()],
            source_enabled: false,
            listen_phone_number_id: None,
            poll_interval_ms: None,
        };
        let notifier = WhatsAppNotifier::with_http_client(config, MockWhatsAppClient::success());
        let recipients = notifier.resolve_recipients(None);
        assert_eq!(recipients, vec!["+1111".to_string(), "+2222".to_string()]);
    }

    #[test]
    fn test_resolve_recipients_returns_config_numbers_when_no_resolved_user() {
        let notifier =
            WhatsAppNotifier::with_http_client(enabled_config(), MockWhatsAppClient::success());
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let recipients = notifier.resolve_recipients(Some(&issue));
        assert_eq!(recipients, vec!["+15559876543".to_string()]);
    }

    #[test]
    fn test_resolve_recipients_uses_resolved_user_whatsapp_number() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                whatsapp_number: Some("+15550001111".to_string()),
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier = WhatsAppNotifier::with_http_client_and_registry(
            enabled_config(),
            MockWhatsAppClient::success(),
            registry,
        );
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "jake");
        let recipients = notifier.resolve_recipients(Some(&issue));
        assert_eq!(recipients, vec!["+15550001111".to_string()]);
    }

    #[test]
    fn test_resolve_recipients_falls_back_when_user_has_no_whatsapp() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                whatsapp_number: None,
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier = WhatsAppNotifier::with_http_client_and_registry(
            enabled_config(),
            MockWhatsAppClient::success(),
            registry,
        );
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "jake");
        let recipients = notifier.resolve_recipients(Some(&issue));
        // Falls back to config to_numbers
        assert_eq!(recipients, vec!["+15559876543".to_string()]);
    }

    #[tokio::test]
    async fn test_ask_question_message_contains_question() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test Issue", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-wa-1".to_string(),
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
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(!body.contains("[CLAUDEAR-Q:"));
        assert!(body.contains("Human input needed for LIN-1"));
        assert!(body.contains("Which branch?"));
    }

    #[tokio::test]
    async fn test_ask_question_delivery_channel_is_whatsapp() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-wa-2".to_string(),
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
        assert_eq!(delivery.channel, "whatsapp");
        assert!(delivery.target.is_none());
        assert_eq!(delivery.message_id.as_deref(), Some("wamid.xxx"));
    }

    #[test]
    fn test_supports_replies_requires_source_enabled() {
        let notifier =
            WhatsAppNotifier::with_http_client(enabled_config(), MockWhatsAppClient::success());
        assert!(!notifier.supports_replies());

        let mut cfg = enabled_config();
        cfg.source_enabled = true;
        let notifier = WhatsAppNotifier::with_http_client(cfg, MockWhatsAppClient::success());
        assert!(notifier.supports_replies());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // single-threaded tokio test; mutex serializes global inbox access
    async fn test_poll_question_replies_matches_reply_context() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();

        let mut cfg = enabled_config();
        cfg.source_enabled = true;
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(cfg, mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-wa-reply".to_string(),
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
        crate::ask_reply_inbox::record_whatsapp_message(
            crate::ask_reply_inbox::WhatsAppInboundMessage {
                message_id: "wamid.reply".to_string(),
                from: "+15559876543".to_string(),
                text: "Use feature/ask-loop".to_string(),
                replied_at: now,
                context_message_id: Some("wamid.xxx".to_string()),
            },
        );

        let replies = notifier
            .poll_question_replies(&request, now - chrono::Duration::seconds(1))
            .await
            .unwrap();

        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].channel, "whatsapp");
        assert_eq!(replies[0].responder.as_deref(), Some("+15559876543"));
        assert_eq!(replies[0].answer, "Use feature/ask-loop");
    }

    #[tokio::test]
    async fn test_notify_start_message_includes_source_and_title() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "1",
            "SEN-42",
            "Memory leak in worker",
            "https://sentry.io/42",
            "sentry",
        );
        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("SEN-42"));
        assert!(body.contains("sentry"));
        assert!(body.contains("Memory leak in worker"));
    }

    #[tokio::test]
    async fn test_notify_failed_short_error_not_truncated() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");

        notifier.notify_failed(&issue, "Short error").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("Short error"));
        assert!(!body.contains("..."));
    }

    #[tokio::test]
    async fn test_notify_failed_exact_100_char_error_not_truncated() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");

        let error = "x".repeat(100);
        notifier.notify_failed(&issue, &error).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains(&error));
        assert!(!body.ends_with("..."));
    }

    #[tokio::test]
    async fn test_send_message_within_limit_not_truncated() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);

        let message = "x".repeat(100);
        notifier.notify_status(&message).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(!body.ends_with("..."));
    }

    #[tokio::test]
    async fn test_notify_routes_to_resolved_user_whatsapp_number() {
        let mock = MockWhatsAppClient::success();
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                whatsapp_number: Some("+15550009999".to_string()),
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier =
            WhatsAppNotifier::with_http_client_and_registry(enabled_config(), mock, registry);
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "jake");

        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let to = calls[0].2["to"].as_str().unwrap();
        assert_eq!(to, "+15550009999");
    }

    // --- Tests for cascade success message ---

    #[tokio::test]
    async fn test_notify_success_cascade_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "downstream/repo");

        notifier
            .notify_success(&issue, "https://github.com/downstream/repo/pull/5")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("Cascade PR"));
        assert!(body.contains("LIN-1"));
        assert!(body.contains("downstream/repo"));
        assert!(body.contains("https://github.com/downstream/repo/pull/5"));
    }

    // --- Tests for PR update success message ---

    #[tokio::test]
    async fn test_notify_success_pr_update_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("is_pr_update", true);

        notifier
            .notify_success(&issue, "https://github.com/org/repo/pull/77")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("PR Updated"));
        assert!(body.contains("LIN-1"));
        assert!(body.contains("https://github.com/org/repo/pull/77"));
    }

    // --- Tests for regression resolved completed message ---

    #[tokio::test]
    async fn test_notify_completed_regression_resolved_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "SEN-1", "Error", "https://sentry.io/1", "sentry");
        issue.set_metadata("regression_resolved", true);

        notifier.notify_completed(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("Regression Resolved"));
        assert!(body.contains("SEN-1"));
        assert!(body.contains("no regression"));
    }

    // --- Tests for regression detected failed message ---

    #[tokio::test]
    async fn test_notify_failed_regression_detected_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "SEN-1", "Error", "https://sentry.io/1", "sentry");
        issue.set_metadata("regression_detected", true);

        notifier
            .notify_failed(&issue, "Tests failing again")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("REGRESSION"));
        assert!(body.contains("SEN-1"));
        assert!(body.contains("Tests failing again"));
    }

    // --- Tests for cascade failed message ---

    #[tokio::test]
    async fn test_notify_failed_cascade_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "downstream/repo");

        notifier.notify_failed(&issue, "Build error").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("CASCADE FAILED"));
        assert!(body.contains("LIN-1"));
        assert!(body.contains("downstream/repo"));
        assert!(body.contains("Build error"));
    }

    // --- Tests for notify_merged and notify_closed ---

    #[tokio::test]
    async fn test_notify_merged_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Fix", "https://example.com", "linear");

        notifier
            .notify_merged(&issue, "https://github.com/org/repo/pull/42")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("PR Merged"));
        assert!(body.contains("PROJ-1"));
        assert!(body.contains("https://github.com/org/repo/pull/42"));
    }

    #[tokio::test]
    async fn test_notify_closed_message_format() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Fix", "https://example.com", "linear");

        notifier
            .notify_closed(&issue, "https://github.com/org/repo/pull/43")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("PR Closed"));
        assert!(body.contains("PROJ-1"));
        assert!(body.contains("https://github.com/org/repo/pull/43"));
    }

    // --- Test failed cascade with long error truncation ---

    #[tokio::test]
    async fn test_notify_failed_cascade_truncates_long_error() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "downstream/repo");

        let long_error = "e".repeat(200);
        notifier.notify_failed(&issue, &long_error).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("CASCADE FAILED"));
        assert!(body.contains("..."));
    }

    // --- Test regression with long error truncation ---

    #[tokio::test]
    async fn test_notify_failed_regression_truncates_long_error() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "SEN-1", "Error", "https://sentry.io/1", "sentry");
        issue.set_metadata("regression_detected", true);

        let long_error = "r".repeat(200);
        notifier.notify_failed(&issue, &long_error).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("REGRESSION"));
        assert!(body.contains("..."));
    }

    // --- Additional test: JSON payload structure ---

    #[tokio::test]
    async fn test_json_payload_has_correct_structure() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);

        notifier.notify_status("test message").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let payload = &calls[0].2;

        // Verify all required fields exist
        assert!(payload.get("messaging_product").is_some());
        assert!(payload.get("to").is_some());
        assert!(payload.get("type").is_some());
        assert!(payload.get("text").is_some());
        assert!(payload["text"].get("body").is_some());
    }

    // --- Test http response fields ---

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
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");
        let result = notifier
            .notify_merged(&issue, "https://github.com/pr/1")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_closed_disabled() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");
        let result = notifier
            .notify_closed(&issue, "https://github.com/pr/1")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_ask_question_disabled() {
        let notifier = WhatsAppNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-wa-disabled".to_string(),
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
        let mock = MockWhatsAppClient::error(400, "Bad request");
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-wa-err".to_string(),
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
                whatsapp_number: Some("+15550001111".to_string()),
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier = WhatsAppNotifier::with_http_client_and_registry(
            enabled_config(),
            MockWhatsAppClient::success(),
            registry,
        );
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "unknown_user");
        let recipients = notifier.resolve_recipients(Some(&issue));
        assert_eq!(recipients, vec!["+15559876543".to_string()]);
    }

    #[tokio::test]
    async fn test_send_message_exactly_4096_not_truncated() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);

        let msg = "x".repeat(4085);
        notifier.notify_status(&msg).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert_eq!(body.len(), 4096);
        assert!(!body.ends_with("..."));
    }

    #[tokio::test]
    async fn test_notify_success_cascade_takes_precedence_over_pr_update() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "downstream/repo");
        issue.set_metadata("is_pr_update", true);

        notifier
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("Cascade PR"));
        assert!(!body.contains("PR Updated"));
    }

    #[tokio::test]
    async fn test_multi_recipient_each_gets_correct_to_number() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(multi_recipient_config(), mock);

        notifier.notify_status("broadcast").await.unwrap();

        let calls = notifier.http.get_last_calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].2["to"], "+15551111111");
        assert_eq!(calls[1].2["to"], "+15552222222");
        assert_eq!(calls[2].2["to"], "+15553333333");
    }

    #[tokio::test]
    async fn test_send_message_empty_recipients_no_api_calls() {
        let mock = MockWhatsAppClient::success();
        let config = WhatsAppConfig {
            phone_number_id: Some("pid".to_string()),
            access_token: Some("token".into()),
            business_account_id: None,
            app_secret: None,
            webhook_verify_token: None,
            to_numbers: vec![],
            source_enabled: false,
            listen_phone_number_id: None,
            poll_interval_ms: None,
        };
        let notifier = WhatsAppNotifier::with_http_client(config, mock);

        let result = notifier.notify_status("hello").await;
        assert!(result.is_ok());
        assert_eq!(notifier.http.get_call_count(), 0);
    }

    #[tokio::test]
    async fn test_notify_status_unicode_passthrough() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);

        notifier
            .notify_status("Status: OK \u{2705} \u{1F680}")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("\u{2705}"));
        assert!(body.contains("\u{1F680}"));
    }

    #[tokio::test]
    async fn test_notify_failed_error_101_chars_gets_truncated() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");

        let error_101 = "x".repeat(101);
        notifier.notify_failed(&issue, &error_101).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("..."));
        assert!(!body.contains(&error_101));
    }

    #[tokio::test]
    async fn test_notify_success_normal_pr_created() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");

        notifier
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("PR Created"));
        assert!(!body.contains("Cascade"));
        assert!(!body.contains("Updated"));
    }

    #[tokio::test]
    async fn test_notify_completed_normal_no_regression() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");

        notifier.notify_completed(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("Completed"));
        assert!(body.contains("no PR URL"));
        assert!(!body.contains("Regression"));
    }

    #[tokio::test]
    async fn test_notify_failed_normal_no_regression_no_cascade() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");

        notifier
            .notify_failed(&issue, "compile error")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("FAILED"));
        assert!(body.contains("compile error"));
        assert!(!body.contains("REGRESSION"));
        assert!(!body.contains("CASCADE"));
    }

    #[tokio::test]
    async fn test_send_message_error_response_299() {
        let mock = MockWhatsAppClient::new(299, "OK");
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.notify_status("test").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_message_error_response_300() {
        let mock = MockWhatsAppClient::new(300, "Redirect");
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let result = notifier.notify_status("test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_start_with_trigger_reason() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("trigger_reason", "Retry attempt 1: connection error");
        notifier.notify_start(&issue).await.unwrap();
        let calls = notifier.http.get_last_calls();
        let text = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(text.contains("Trigger: Retry attempt 1: connection error"));
    }

    #[tokio::test]
    async fn test_notify_start_without_trigger_reason() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        notifier.notify_start(&issue).await.unwrap();
        let calls = notifier.http.get_last_calls();
        let text = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(!text.contains("Trigger:"));
    }

    #[test]
    fn test_extract_reply_text_normal() {
        assert_eq!(
            WhatsAppNotifier::<MockWhatsAppClient>::extract_reply_text("hello world"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn test_extract_reply_text_empty() {
        assert_eq!(
            WhatsAppNotifier::<MockWhatsAppClient>::extract_reply_text(""),
            None
        );
    }

    #[test]
    fn test_extract_reply_text_whitespace_only() {
        assert_eq!(
            WhatsAppNotifier::<MockWhatsAppClient>::extract_reply_text("   \n\t  "),
            None
        );
    }

    #[test]
    fn test_extract_reply_text_trims_whitespace() {
        assert_eq!(
            WhatsAppNotifier::<MockWhatsAppClient>::extract_reply_text("  answer  "),
            Some("answer".to_string())
        );
    }

    #[test]
    fn test_whatsapp_send_message_response_deserialize_success() {
        let json = r#"{"messages":[{"id":"wamid.abc123"}]}"#;
        let parsed: WhatsAppSendMessageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].id, "wamid.abc123");
    }

    #[test]
    fn test_whatsapp_send_message_response_deserialize_empty_messages() {
        let json = r#"{"messages":[]}"#;
        let parsed: WhatsAppSendMessageResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn test_whatsapp_send_message_response_deserialize_missing_messages() {
        let json = r#"{}"#;
        let parsed: WhatsAppSendMessageResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn test_whatsapp_send_message_response_deserialize_multiple() {
        let json = r#"{"messages":[{"id":"wamid.1"},{"id":"wamid.2"}]}"#;
        let parsed: WhatsAppSendMessageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].id, "wamid.1");
        assert_eq!(parsed.messages[1].id, "wamid.2");
    }

    #[tokio::test]
    async fn test_send_message_with_ids_returns_message_id() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");

        let ids = notifier
            .send_message_with_ids("test body", Some(&issue))
            .await
            .unwrap();

        assert_eq!(ids, vec!["wamid.xxx".to_string()]);
    }

    #[tokio::test]
    async fn test_send_message_with_ids_no_config_returns_empty() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(disabled_config(), mock);

        let ids = notifier.send_message_with_ids("test", None).await.unwrap();
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn test_send_message_with_ids_empty_message_id_skipped() {
        let mock = MockWhatsAppClient::new(200, r#"{"messages":[{"id":"  "}]}"#);
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);

        let ids = notifier.send_message_with_ids("test", None).await.unwrap();
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn test_send_message_with_ids_unparseable_response_returns_empty() {
        let mock = MockWhatsAppClient::new(200, "not json");
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);

        let ids = notifier.send_message_with_ids("test", None).await.unwrap();
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn test_notify_success_with_trigger_reason() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("trigger_reason", "Manual retry");
        notifier
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .unwrap();
        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("PR Created"));
        assert!(body.contains("Trigger: Manual retry"));
    }

    #[tokio::test]
    async fn test_notify_failed_with_trigger_reason() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("trigger_reason", "Scheduled run");
        notifier.notify_failed(&issue, "timeout").await.unwrap();
        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("FAILED"));
        assert!(body.contains("Trigger: Scheduled run"));
    }

    #[tokio::test]
    async fn test_notify_completed_with_completion_reason() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("completion_reason", "Already fixed upstream");
        notifier.notify_completed(&issue).await.unwrap();
        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("Completed"));
        assert!(body.contains("Already fixed upstream"));
        assert!(!body.contains("no PR URL"));
    }

    #[test]
    fn test_supports_replies_disabled_config() {
        let notifier =
            WhatsAppNotifier::with_http_client(disabled_config(), MockWhatsAppClient::success());
        assert!(!notifier.supports_replies());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_poll_question_replies_source_not_enabled_returns_empty() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let notifier =
            WhatsAppNotifier::with_http_client(enabled_config(), MockWhatsAppClient::success());
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-wa-no-src".to_string(),
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
        let replies = notifier
            .poll_question_replies(&request, chrono::Utc::now() - chrono::Duration::seconds(10))
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_poll_question_replies_ignores_unrelated_messages() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let mut cfg = enabled_config();
        cfg.source_enabled = true;
        cfg.to_numbers = vec!["+15559876543".to_string(), "+15551112222".to_string()];
        let notifier = WhatsAppNotifier::with_http_client(cfg, MockWhatsAppClient::success());
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-wa-unrelated".to_string(),
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

        // Record a message without matching context_message_id
        crate::ask_reply_inbox::record_whatsapp_message(
            crate::ask_reply_inbox::WhatsAppInboundMessage {
                message_id: "wamid.unrelated".to_string(),
                from: "+1555999".to_string(),
                text: "Random text".to_string(),
                replied_at: chrono::Utc::now(),
                context_message_id: Some("wamid.different".to_string()),
            },
        );

        let replies = notifier
            .poll_question_replies(&request, chrono::Utc::now() - chrono::Duration::seconds(10))
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_poll_question_replies_single_sender_fallback() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let mut cfg = enabled_config();
        cfg.source_enabled = true;
        // Single sender = fallback mode
        cfg.to_numbers = vec!["+15559876543".to_string()];
        let notifier = WhatsAppNotifier::with_http_client(cfg, MockWhatsAppClient::success());
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-wa-fallback".to_string(),
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
        notifier.ask_question(&issue, &request).await.unwrap();

        // Record a message from the single configured sender, no context_message_id
        let now = chrono::Utc::now();
        crate::ask_reply_inbox::record_whatsapp_message(
            crate::ask_reply_inbox::WhatsAppInboundMessage {
                message_id: "wamid.fallback".to_string(),
                from: "+15559876543".to_string(),
                text: "My answer".to_string(),
                replied_at: now,
                context_message_id: None,
            },
        );

        let replies = notifier
            .poll_question_replies(&request, now - chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].answer, "My answer");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_poll_question_replies_empty_text_skipped() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let mut cfg = enabled_config();
        cfg.source_enabled = true;
        cfg.to_numbers = vec!["+15559876543".to_string()];
        let notifier = WhatsAppNotifier::with_http_client(cfg, MockWhatsAppClient::success());
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-wa-empty-reply".to_string(),
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
        notifier.ask_question(&issue, &request).await.unwrap();

        let now = chrono::Utc::now();
        crate::ask_reply_inbox::record_whatsapp_message(
            crate::ask_reply_inbox::WhatsAppInboundMessage {
                message_id: "wamid.empty".to_string(),
                from: "+15559876543".to_string(),
                text: "   ".to_string(),
                replied_at: now,
                context_message_id: Some("wamid.xxx".to_string()),
            },
        );

        let replies = notifier
            .poll_question_replies(&request, now - chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    // --- Dynamic dispatch (Box<dyn Notifier>) tests for tarpaulin coverage ---

    fn boxed_notifier(mock: MockWhatsAppClient, config: WhatsAppConfig) -> Box<dyn Notifier> {
        Box::new(WhatsAppNotifier::with_http_client(config, mock))
    }

    #[tokio::test]
    async fn test_dyn_notify_start() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        assert!(n.notify_start(&issue).await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_start_with_trigger_reason() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let mut issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        issue.set_metadata("trigger_reason", "Retry");
        assert!(n.notify_start(&issue).await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_success_regular() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        assert!(n
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_success_cascade() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let mut issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "down/repo");
        assert!(n
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_success_pr_update() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let mut issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        issue.set_metadata("is_pr_update", true);
        assert!(n
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_success_with_trigger_reason() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let mut issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        issue.set_metadata("trigger_reason", "Auto");
        assert!(n
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_completed_regular() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        assert!(n.notify_completed(&issue).await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_completed_regression_resolved() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let mut issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        issue.set_metadata("regression_resolved", true);
        assert!(n.notify_completed(&issue).await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_completed_with_completion_reason() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let mut issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        issue.set_metadata("completion_reason", "Already fixed");
        assert!(n.notify_completed(&issue).await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_failed_regular() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        assert!(n.notify_failed(&issue, "error").await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_failed_regression() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let mut issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        issue.set_metadata("regression_detected", true);
        assert!(n.notify_failed(&issue, "regression error").await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_failed_cascade() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let mut issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "down/repo");
        assert!(n.notify_failed(&issue, "cascade error").await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_failed_with_trigger_reason() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let mut issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        issue.set_metadata("trigger_reason", "Scheduled");
        assert!(n.notify_failed(&issue, "timeout").await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_failed_long_error() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        let long_error = "x".repeat(200);
        assert!(n.notify_failed(&issue, &long_error).await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_merged() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        assert!(n
            .notify_merged(&issue, "https://github.com/pr/1")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_closed() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        assert!(n
            .notify_closed(&issue, "https://github.com/pr/1")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_status() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        assert!(n.notify_status("test status").await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_urgent_issues_empty() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        assert!(n.notify_urgent_issues(&[]).await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_notify_urgent_issues_multiple() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let issues = vec![
            Issue::new("1", "DYN-1", "Bug 1", "https://example.com", "linear"),
            Issue::new("2", "DYN-2", "Bug 2", "https://example.com", "linear"),
            Issue::new("3", "DYN-3", "Bug 3", "https://example.com", "linear"),
            Issue::new("4", "DYN-4", "Bug 4", "https://example.com", "linear"),
        ];
        assert!(n.notify_urgent_issues(&issues).await.is_ok());
    }

    #[tokio::test]
    async fn test_dyn_ask_question() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let issue = Issue::new("1", "DYN-1", "Dyn test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-dyn-ask".to_string(),
            source: "linear".to_string(),
            repo: None,
            issue_id: "1".to_string(),
            short_id: "DYN-1".to_string(),
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
        let delivery = n.ask_question(&issue, &request).await.unwrap().unwrap();
        assert_eq!(delivery.channel, "whatsapp");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_dyn_poll_question_replies_source_disabled() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-dyn-poll".to_string(),
            source: "linear".to_string(),
            repo: None,
            issue_id: "1".to_string(),
            short_id: "DYN-1".to_string(),
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
        let replies = n
            .poll_question_replies(&request, chrono::Utc::now() - chrono::Duration::seconds(10))
            .await
            .unwrap();
        assert!(replies.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_dyn_poll_question_replies_with_source_enabled() {
        let _inbox_guard = crate::ask_reply_inbox::clear_for_tests();
        let mut cfg = enabled_config();
        cfg.source_enabled = true;
        let n: Box<dyn Notifier> = Box::new(WhatsAppNotifier::with_http_client(
            cfg,
            MockWhatsAppClient::success(),
        ));
        let issue = Issue::new("1", "DYN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-dyn-poll-en".to_string(),
            source: "linear".to_string(),
            repo: None,
            issue_id: "1".to_string(),
            short_id: "DYN-1".to_string(),
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
        n.ask_question(&issue, &request).await.unwrap();

        let now = chrono::Utc::now();
        crate::ask_reply_inbox::record_whatsapp_message(
            crate::ask_reply_inbox::WhatsAppInboundMessage {
                message_id: "wamid.dyn-reply".to_string(),
                from: "+15559876543".to_string(),
                text: "Use main branch".to_string(),
                replied_at: now,
                context_message_id: Some("wamid.xxx".to_string()),
            },
        );

        let replies = n
            .poll_question_replies(&request, now - chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].answer, "Use main branch");
    }

    #[tokio::test]
    async fn test_notify_success_with_confidence() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("confidence", 85u8);
        notifier
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .unwrap();
        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(body.contains("Fix Confidence: 85/100"));
    }

    #[tokio::test]
    async fn test_notify_success_without_confidence() {
        let mock = MockWhatsAppClient::success();
        let notifier = WhatsAppNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        notifier
            .notify_success(&issue, "https://github.com/pr/1")
            .await
            .unwrap();
        let calls = notifier.http.get_last_calls();
        let body = calls[0].2["text"]["body"].as_str().unwrap();
        assert!(!body.contains("Fix Confidence"));
    }

    #[test]
    fn test_dyn_name_and_is_enabled() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        assert_eq!(n.name(), "whatsapp");
        assert!(n.is_enabled());
    }

    #[test]
    fn test_dyn_supports_replies_enabled() {
        let mut cfg = enabled_config();
        cfg.source_enabled = true;
        let n: Box<dyn Notifier> = Box::new(WhatsAppNotifier::with_http_client(
            cfg,
            MockWhatsAppClient::success(),
        ));
        assert!(n.supports_replies());
    }

    #[test]
    fn test_dyn_supports_replies_disabled() {
        let n = boxed_notifier(MockWhatsAppClient::success(), enabled_config());
        assert!(!n.supports_replies());
    }
}
