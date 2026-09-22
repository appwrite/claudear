//! SMS notifier via Twilio.

use super::Notifier;
use async_trait::async_trait;
use claudear_config::config::SmsConfig;
use claudear_config::users::UserRegistry;
use claudear_core::error::{Error, Result};
use claudear_core::http::HttpResponse;
use claudear_core::types::{AskDelivery, AskRequest, Issue};

/// Trait for HTTP client used by SMS notifier.
#[async_trait]
pub trait SmsHttpClient: Send + Sync {
    async fn post_form(
        &self,
        url: &str,
        auth_user: &str,
        auth_pass: &str,
        params: &[(&str, &str)],
    ) -> Result<HttpResponse>;
}

/// Real HTTP client using reqwest.
pub struct ReqwestSmsClient {
    client: reqwest::Client,
}

impl ReqwestSmsClient {
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

impl Default for ReqwestSmsClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SmsHttpClient for ReqwestSmsClient {
    async fn post_form(
        &self,
        url: &str,
        auth_user: &str,
        auth_pass: &str,
        params: &[(&str, &str)],
    ) -> Result<HttpResponse> {
        let response = self
            .client
            .post(url)
            .basic_auth(auth_user, Some(auth_pass))
            .form(params)
            .send()
            .await?;

        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();

        Ok(HttpResponse { status, body })
    }
}

/// SMS notifier that sends notifications via Twilio.
pub struct SmsNotifier<H: SmsHttpClient = ReqwestSmsClient> {
    config: SmsConfig,
    http: H,
    user_registry: UserRegistry,
}

impl SmsNotifier<ReqwestSmsClient> {
    /// Create a new SMS notifier.
    pub fn new(config: SmsConfig, user_registry: UserRegistry) -> Self {
        Self {
            config,
            http: ReqwestSmsClient::new(),
            user_registry,
        }
    }
}

impl<H: SmsHttpClient> SmsNotifier<H> {
    /// Create a new SMS notifier with custom HTTP client.
    pub fn with_http_client(config: SmsConfig, http: H) -> Self {
        Self {
            config,
            http,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
        }
    }

    /// Create a new SMS notifier with custom HTTP client and user registry.
    pub fn with_http_client_and_registry(
        config: SmsConfig,
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
                    if let Some(ref number) = user.sms_number {
                        return vec![number.clone()];
                    }
                }
            }
        }
        self.config.to_numbers.clone()
    }

    async fn send_sms(&self, body: &str, issue: Option<&Issue>) -> Result<()> {
        let (account_sid, auth_token, from_number) = match (
            &self.config.account_sid,
            &self.config.auth_token,
            &self.config.from_number,
        ) {
            (Some(sid), Some(token), Some(from)) => (sid, token.expose(), from),
            _ => return Ok(()),
        };

        let url = format!(
            "https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json",
            account_sid
        );

        // Truncate message to SMS limit (160 chars for basic SMS, 1600 for modern)
        let truncated_body = if body.len() > 1500 {
            format!("{}...", &body[..body.floor_char_boundary(1497)])
        } else {
            body.to_string()
        };

        let recipients = self.resolve_recipients(issue);

        for to_number in &recipients {
            let params = [
                ("From", from_number.as_str()),
                ("To", to_number.as_str()),
                ("Body", &truncated_body),
            ];

            let response = self
                .http
                .post_form(&url, account_sid, auth_token, &params)
                .await?;

            if response.status < 200 || response.status >= 300 {
                return Err(Error::notifier(
                    "sms",
                    format!("Twilio error: {}", response.body),
                ));
            }
        }

        Ok(())
    }
}

#[async_trait]
impl<H: SmsHttpClient + 'static> Notifier for SmsNotifier<H> {
    fn name(&self) -> &str {
        "sms"
    }

    fn is_enabled(&self) -> bool {
        self.config.account_sid.is_some()
            && self.config.auth_token.is_some()
            && self.config.from_number.is_some()
            && !self.config.to_numbers.is_empty()
    }

    async fn notify_start(&self, issue: &Issue) -> Result<()> {
        let mut body = format!(
            "[Claudear] Processing {} from {} - {}",
            issue.short_id, issue.source, issue.title
        );
        if let Some(reason) = issue.get_metadata::<String>("trigger_reason") {
            let truncated = if reason.len() > 50 {
                format!("{}...", &reason[..reason.floor_char_boundary(47)])
            } else {
                reason
            };
            body.push_str(&format!("\nTrigger: {}", truncated));
        }
        self.send_sms(&body, Some(issue)).await
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
            let truncated = if reason.len() > 50 {
                format!("{}...", &reason[..reason.floor_char_boundary(47)])
            } else {
                reason
            };
            body.push_str(&format!("\nTrigger: {}", truncated));
        }
        self.send_sms(&body, Some(issue)).await
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
        self.send_sms(&body, Some(issue)).await
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
            let truncated = if reason.len() > 50 {
                format!("{}...", &reason[..reason.floor_char_boundary(47)])
            } else {
                reason
            };
            body.push_str(&format!("\nTrigger: {}", truncated));
        }
        self.send_sms(&body, Some(issue)).await
    }

    async fn notify_merged(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        let body = format!("[Claudear] PR Merged for {}: {}", issue.short_id, pr_url);
        self.send_sms(&body, Some(issue)).await
    }

    async fn notify_closed(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        let body = format!("[Claudear] PR Closed for {}: {}", issue.short_id, pr_url);
        self.send_sms(&body, Some(issue)).await
    }

    async fn notify_status(&self, message: &str) -> Result<()> {
        let body = format!("[Claudear] {}", message);
        self.send_sms(&body, None).await
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
        self.send_sms(&body, None).await
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
        self.send_sms(&body, Some(issue)).await?;
        Ok(Some(AskDelivery {
            channel: "sms".to_string(),
            target: None,
            message_id: None,
        }))
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

    /// Mock SMS HTTP client for testing.
    #[expect(clippy::type_complexity)]
    struct MockSmsClient {
        response_status: u16,
        response_body: String,
        call_count: AtomicUsize,
        last_calls: Mutex<Vec<(String, String, String, Vec<(String, String)>)>>,
    }

    impl MockSmsClient {
        fn new(status: u16, body: &str) -> Self {
            Self {
                response_status: status,
                response_body: body.to_string(),
                call_count: AtomicUsize::new(0),
                last_calls: Mutex::new(Vec::new()),
            }
        }

        fn success() -> Self {
            Self::new(200, r#"{"sid": "SMxxx", "status": "queued"}"#)
        }

        fn error(status: u16, body: &str) -> Self {
            Self::new(status, body)
        }

        fn get_call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }

        #[expect(clippy::type_complexity)]
        fn get_last_calls(&self) -> Vec<(String, String, String, Vec<(String, String)>)> {
            self.last_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SmsHttpClient for MockSmsClient {
        async fn post_form(
            &self,
            url: &str,
            auth_user: &str,
            auth_pass: &str,
            params: &[(&str, &str)],
        ) -> Result<HttpResponse> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let params_owned: Vec<(String, String)> = params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            self.last_calls.lock().unwrap().push((
                url.to_string(),
                auth_user.to_string(),
                auth_pass.to_string(),
                params_owned,
            ));

            Ok(HttpResponse {
                status: self.response_status,
                body: self.response_body.clone(),
            })
        }
    }

    fn disabled_config() -> SmsConfig {
        SmsConfig {
            account_sid: None,
            auth_token: None,
            from_number: None,
            to_numbers: vec![],
        }
    }

    fn enabled_config() -> SmsConfig {
        SmsConfig {
            account_sid: Some("AC123456".to_string()),
            auth_token: Some("auth_token_xyz".into()),
            from_number: Some("+15551234567".to_string()),
            to_numbers: vec!["+15559876543".to_string()],
        }
    }

    fn multi_recipient_config() -> SmsConfig {
        SmsConfig {
            account_sid: Some("AC123456".to_string()),
            auth_token: Some("auth_token_xyz".into()),
            from_number: Some("+15551234567".to_string()),
            to_numbers: vec![
                "+15551111111".to_string(),
                "+15552222222".to_string(),
                "+15553333333".to_string(),
            ],
        }
    }

    fn partial_config_no_sid() -> SmsConfig {
        SmsConfig {
            account_sid: None,
            auth_token: Some("token".into()),
            from_number: Some("+1234567890".to_string()),
            to_numbers: vec!["+0987654321".to_string()],
        }
    }

    fn partial_config_no_token() -> SmsConfig {
        SmsConfig {
            account_sid: Some("sid".to_string()),
            auth_token: None,
            from_number: Some("+1234567890".to_string()),
            to_numbers: vec!["+0987654321".to_string()],
        }
    }

    fn partial_config_no_from() -> SmsConfig {
        SmsConfig {
            account_sid: Some("sid".to_string()),
            auth_token: Some("token".into()),
            from_number: None,
            to_numbers: vec!["+0987654321".to_string()],
        }
    }

    fn partial_config_no_to() -> SmsConfig {
        SmsConfig {
            account_sid: Some("sid".to_string()),
            auth_token: Some("token".into()),
            from_number: Some("+1234567890".to_string()),
            to_numbers: vec![],
        }
    }

    #[test]
    fn test_is_enabled() {
        let enabled_config = SmsConfig {
            account_sid: Some("test".to_string()),
            auth_token: Some("test".into()),
            from_number: Some("+1234567890".to_string()),
            to_numbers: vec!["+0987654321".to_string()],
        };
        let notifier = SmsNotifier::new(enabled_config, empty_registry());
        assert!(notifier.is_enabled());

        let disabled_config = SmsConfig {
            account_sid: None,
            auth_token: None,
            from_number: None,
            to_numbers: vec![],
        };
        let notifier = SmsNotifier::new(disabled_config, empty_registry());
        assert!(!notifier.is_enabled());
    }

    #[test]
    fn test_name() {
        let notifier = SmsNotifier::new(disabled_config(), empty_registry());
        assert_eq!(notifier.name(), "sms");
    }

    #[test]
    fn test_is_enabled_partial_configs() {
        assert!(!SmsNotifier::new(partial_config_no_sid(), empty_registry()).is_enabled());
        assert!(!SmsNotifier::new(partial_config_no_token(), empty_registry()).is_enabled());
        assert!(!SmsNotifier::new(partial_config_no_from(), empty_registry()).is_enabled());
        assert!(!SmsNotifier::new(partial_config_no_to(), empty_registry()).is_enabled());
    }

    #[tokio::test]
    async fn test_notify_start_disabled() {
        let notifier = SmsNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier.notify_start(&issue).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_success_disabled() {
        let notifier = SmsNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier
            .notify_success(&issue, "https://github.com/org/repo/pull/1")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_completed_disabled() {
        let notifier = SmsNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier.notify_completed(&issue).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_failed_disabled() {
        let notifier = SmsNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        let result = notifier.notify_failed(&issue, "Error message").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_failed_long_error() {
        let notifier = SmsNotifier::new(disabled_config(), empty_registry());
        let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", "linear");

        // Error longer than 100 characters
        let long_error = "x".repeat(200);
        let result = notifier.notify_failed(&issue, &long_error).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_status_disabled() {
        let notifier = SmsNotifier::new(disabled_config(), empty_registry());

        let result = notifier.notify_status("Status update").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_empty() {
        let notifier = SmsNotifier::new(disabled_config(), empty_registry());

        let result = notifier.notify_urgent_issues(&[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_disabled() {
        let notifier = SmsNotifier::new(disabled_config(), empty_registry());
        let issues = vec![
            Issue::new("1", "PROJ-1", "Issue 1", "https://example.com", "linear"),
            Issue::new("2", "PROJ-2", "Issue 2", "https://example.com", "linear"),
        ];

        let result = notifier.notify_urgent_issues(&issues).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_truncated_to_three() {
        let notifier = SmsNotifier::new(disabled_config(), empty_registry());
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
        let config = SmsConfig {
            account_sid: Some("sid".to_string()),
            auth_token: Some("token".into()),
            from_number: Some("+1234567890".to_string()),
            to_numbers: vec![
                "+1111111111".to_string(),
                "+2222222222".to_string(),
                "+3333333333".to_string(),
            ],
        };

        let notifier = SmsNotifier::new(config, empty_registry());
        assert!(notifier.is_enabled());
    }

    // Mock-based tests for HTTP-dependent functionality

    #[tokio::test]
    async fn test_send_sms_success() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
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
    async fn test_send_sms_verifies_url_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
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
        assert!(calls[0].0.contains("api.twilio.com"));
        assert!(calls[0].0.contains("AC123456")); // Account SID in URL
        assert!(calls[0].0.contains("Messages.json"));
    }

    #[tokio::test]
    async fn test_send_sms_uses_basic_auth() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        assert_eq!(calls[0].1, "AC123456"); // auth_user
        assert_eq!(calls[0].2, "auth_token_xyz"); // auth_pass
    }

    #[tokio::test]
    async fn test_send_sms_sends_correct_params() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let params = &calls[0].3;
        assert!(params
            .iter()
            .any(|(k, v)| k == "From" && v == "+15551234567"));
        assert!(params.iter().any(|(k, v)| k == "To" && v == "+15559876543"));
        assert!(params
            .iter()
            .any(|(k, v)| k == "Body" && v.contains("Processing")));
    }

    #[tokio::test]
    async fn test_send_sms_multiple_recipients() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(multi_recipient_config(), mock);
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
    async fn test_send_sms_error_response() {
        let mock = MockSmsClient::error(400, "Invalid phone number");
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
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
        assert!(err_str.contains("Twilio error"));
        assert!(err_str.contains("Invalid phone number"));
    }

    #[tokio::test]
    async fn test_send_sms_server_error() {
        let mock = MockSmsClient::error(500, "Internal server error");
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
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
    async fn test_send_sms_truncates_long_message() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);

        // Create a message longer than 1500 chars
        let long_message = "x".repeat(2000);
        notifier.notify_status(&long_message).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body_param = calls[0].3.iter().find(|(k, _)| k == "Body").unwrap();
        // Body should be truncated to 1500 chars + "..."
        assert!(body_param.1.len() <= 1600); // "[Claudear] " + truncated body
        assert!(body_param.1.ends_with("..."));
    }

    #[tokio::test]
    async fn test_notify_success_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
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
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("[Claudear]"));
        assert!(body.contains("PR Created"));
        assert!(body.contains("PROJ-123"));
        assert!(body.contains("https://github.com/org/repo/pull/42"));
    }

    #[tokio::test]
    async fn test_notify_completed_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        notifier.notify_completed(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("Completed"));
        assert!(body.contains("no PR URL"));
    }

    #[tokio::test]
    async fn test_notify_failed_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
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
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("FAILED"));
        assert!(body.contains("PROJ-123"));
        assert!(body.contains("Build failed"));
    }

    #[tokio::test]
    async fn test_notify_failed_truncates_long_error() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
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
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        // Error should be truncated to 100 chars including "..."
        assert!(body.contains("..."));
    }

    #[tokio::test]
    async fn test_notify_status_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);

        notifier.notify_status("System is healthy").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert_eq!(body, "[Claudear] System is healthy");
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issues = vec![
            Issue::new("1", "PROJ-1", "Issue 1", "https://example.com", "linear"),
            Issue::new("2", "PROJ-2", "Issue 2", "https://example.com", "linear"),
        ];

        notifier.notify_urgent_issues(&issues).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("2 urgent issue(s)"));
        assert!(body.contains("PROJ-1"));
        assert!(body.contains("PROJ-2"));
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_truncates_to_three() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
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
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("10 urgent issue(s)"));
        // Only first 3 are listed
        assert!(body.contains("PROJ-1"));
        assert!(body.contains("PROJ-2"));
        assert!(body.contains("PROJ-3"));
        assert!(!body.contains("PROJ-4"));
    }

    #[tokio::test]
    async fn test_send_sms_stops_on_first_error() {
        let mock = MockSmsClient::error(400, "Bad request");
        let notifier = SmsNotifier::with_http_client(multi_recipient_config(), mock);
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
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);

        assert!(notifier.is_enabled());
        assert_eq!(notifier.name(), "sms");
    }

    #[test]
    fn test_reqwest_sms_client_default() {
        let client = ReqwestSmsClient::default();
        // Just verify it can be constructed
        assert!(std::mem::size_of_val(&client) > 0);
    }

    #[test]
    fn test_http_response_fields() {
        let response = HttpResponse {
            status: 201,
            body: "Created".to_string(),
        };
        assert_eq!(response.status, 201);
        assert_eq!(response.body, "Created");
    }

    #[test]
    fn test_resolve_recipients_returns_config_numbers_when_no_issue() {
        let config = SmsConfig {
            account_sid: Some("sid".to_string()),
            auth_token: Some("token".into()),
            from_number: Some("+1000".to_string()),
            to_numbers: vec!["+1111".to_string(), "+2222".to_string()],
        };
        let notifier = SmsNotifier::with_http_client(config, MockSmsClient::success());
        let recipients = notifier.resolve_recipients(None);
        assert_eq!(recipients, vec!["+1111".to_string(), "+2222".to_string()]);
    }

    #[test]
    fn test_resolve_recipients_returns_config_numbers_when_no_resolved_user() {
        let notifier = SmsNotifier::with_http_client(enabled_config(), MockSmsClient::success());
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let recipients = notifier.resolve_recipients(Some(&issue));
        assert_eq!(recipients, vec!["+15559876543".to_string()]);
    }

    #[test]
    fn test_resolve_recipients_uses_resolved_user_sms_number() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                sms_number: Some("+15550001111".to_string()),
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier = SmsNotifier::with_http_client_and_registry(
            enabled_config(),
            MockSmsClient::success(),
            registry,
        );
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "jake");
        let recipients = notifier.resolve_recipients(Some(&issue));
        assert_eq!(recipients, vec!["+15550001111".to_string()]);
    }

    #[test]
    fn test_resolve_recipients_falls_back_when_user_has_no_sms() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                sms_number: None,
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier = SmsNotifier::with_http_client_and_registry(
            enabled_config(),
            MockSmsClient::success(),
            registry,
        );
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "jake");
        let recipients = notifier.resolve_recipients(Some(&issue));
        // Falls back to config to_numbers
        assert_eq!(recipients, vec!["+15559876543".to_string()]);
    }

    #[tokio::test]
    async fn test_ask_question_message_contains_token() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test Issue", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-sms-1".to_string(),
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
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(!body.contains("[CLAUDEAR-Q:"));
        assert!(body.contains("Human input needed for LIN-1"));
        assert!(body.contains("Which branch?"));
    }

    #[tokio::test]
    async fn test_ask_question_delivery_channel_is_sms() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        let request = claudear_core::types::AskRequest {
            correlation_id: "tok-sms-2".to_string(),
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
        assert_eq!(delivery.channel, "sms");
        assert!(delivery.target.is_none());
        assert!(delivery.message_id.is_none());
    }

    #[tokio::test]
    async fn test_notify_start_message_includes_source_and_title() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new(
            "1",
            "SEN-42",
            "Memory leak in worker",
            "https://sentry.io/42",
            "sentry",
        );
        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("SEN-42"));
        assert!(body.contains("sentry"));
        assert!(body.contains("Memory leak in worker"));
    }

    #[tokio::test]
    async fn test_notify_failed_short_error_not_truncated() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");

        notifier.notify_failed(&issue, "Short error").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("Short error"));
        assert!(!body.contains("..."));
    }

    #[tokio::test]
    async fn test_notify_failed_exact_100_char_error_not_truncated() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Test", "https://example.com", "linear");

        let error = "x".repeat(100);
        notifier.notify_failed(&issue, &error).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains(&error));
        assert!(!body.ends_with("..."));
    }

    #[tokio::test]
    async fn test_send_sms_message_within_limit_not_truncated() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);

        let message = "x".repeat(100);
        notifier.notify_status(&message).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(!body.ends_with("..."));
    }

    #[tokio::test]
    async fn test_notify_routes_to_resolved_user_sms_number() {
        let mock = MockSmsClient::success();
        let mut users = std::collections::HashMap::new();
        users.insert(
            "jake".to_string(),
            claudear_config::config::UserConfig {
                sms_number: Some("+15550009999".to_string()),
                ..Default::default()
            },
        );
        let registry = claudear_config::users::UserRegistry::new(users);
        let notifier = SmsNotifier::with_http_client_and_registry(enabled_config(), mock, registry);
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata("resolved_user", "jake");

        notifier.notify_start(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let to_param = calls[0].3.iter().find(|(k, _)| k == "To").unwrap();
        assert_eq!(to_param.1, "+15550009999");
    }

    // --- Tests for cascade success message ---

    #[tokio::test]
    async fn test_notify_success_cascade_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "downstream/repo");

        notifier
            .notify_success(&issue, "https://github.com/downstream/repo/pull/5")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("Cascade PR"));
        assert!(body.contains("LIN-1"));
        assert!(body.contains("downstream/repo"));
        assert!(body.contains("https://github.com/downstream/repo/pull/5"));
    }

    // --- Tests for PR update success message ---

    #[tokio::test]
    async fn test_notify_success_pr_update_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("is_pr_update", true);

        notifier
            .notify_success(&issue, "https://github.com/org/repo/pull/77")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("PR Updated"));
        assert!(body.contains("LIN-1"));
        assert!(body.contains("https://github.com/org/repo/pull/77"));
    }

    // --- Tests for regression resolved completed message ---

    #[tokio::test]
    async fn test_notify_completed_regression_resolved_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "SEN-1", "Error", "https://sentry.io/1", "sentry");
        issue.set_metadata("regression_resolved", true);

        notifier.notify_completed(&issue).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("Regression Resolved"));
        assert!(body.contains("SEN-1"));
        assert!(body.contains("no regression"));
    }

    // --- Tests for regression detected failed message ---

    #[tokio::test]
    async fn test_notify_failed_regression_detected_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "SEN-1", "Error", "https://sentry.io/1", "sentry");
        issue.set_metadata("regression_detected", true);

        notifier
            .notify_failed(&issue, "Tests failing again")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("REGRESSION"));
        assert!(body.contains("SEN-1"));
        assert!(body.contains("Tests failing again"));
    }

    // --- Tests for cascade failed message ---

    #[tokio::test]
    async fn test_notify_failed_cascade_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "downstream/repo");

        notifier.notify_failed(&issue, "Build error").await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("CASCADE FAILED"));
        assert!(body.contains("LIN-1"));
        assert!(body.contains("downstream/repo"));
        assert!(body.contains("Build error"));
    }

    // --- Tests for notify_merged and notify_closed ---

    #[tokio::test]
    async fn test_notify_merged_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Fix", "https://example.com", "linear");

        notifier
            .notify_merged(&issue, "https://github.com/org/repo/pull/42")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("PR Merged"));
        assert!(body.contains("PROJ-1"));
        assert!(body.contains("https://github.com/org/repo/pull/42"));
    }

    #[tokio::test]
    async fn test_notify_closed_message_format() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "PROJ-1", "Fix", "https://example.com", "linear");

        notifier
            .notify_closed(&issue, "https://github.com/org/repo/pull/43")
            .await
            .unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("PR Closed"));
        assert!(body.contains("PROJ-1"));
        assert!(body.contains("https://github.com/org/repo/pull/43"));
    }

    // --- Test failed cascade with long error truncation ---

    #[tokio::test]
    async fn test_notify_failed_cascade_truncates_long_error() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Fix", "https://example.com", "linear");
        issue.set_metadata("cascade_downstream_repo", "downstream/repo");

        let long_error = "e".repeat(200);
        notifier.notify_failed(&issue, &long_error).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("CASCADE FAILED"));
        assert!(body.contains("..."));
    }

    // --- Test regression with long error truncation ---

    #[tokio::test]
    async fn test_notify_failed_regression_truncates_long_error() {
        let mock = MockSmsClient::success();
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "SEN-1", "Error", "https://sentry.io/1", "sentry");
        issue.set_metadata("regression_detected", true);

        let long_error = "r".repeat(200);
        notifier.notify_failed(&issue, &long_error).await.unwrap();

        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("REGRESSION"));
        assert!(body.contains("..."));
    }

    #[tokio::test]
    async fn test_notify_start_with_trigger_reason() {
        let mock = MockSmsClient::new(201, "OK");
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let mut issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        issue.set_metadata(
            "trigger_reason",
            "Retry attempt 3: PR closed without merge by the maintainer",
        );
        notifier.notify_start(&issue).await.unwrap();
        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(body.contains("Trigger: "));
        assert!(body.contains("..."));
        assert!(!body.contains("maintainer"));
    }

    #[tokio::test]
    async fn test_notify_start_without_trigger_reason() {
        let mock = MockSmsClient::new(201, "OK");
        let notifier = SmsNotifier::with_http_client(enabled_config(), mock);
        let issue = Issue::new("1", "LIN-1", "Test", "https://example.com", "linear");
        notifier.notify_start(&issue).await.unwrap();
        let calls = notifier.http.get_last_calls();
        let body = &calls[0].3.iter().find(|(k, _)| k == "Body").unwrap().1;
        assert!(!body.contains("Trigger:"));
    }
}
