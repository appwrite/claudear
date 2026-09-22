//! Sentry API client for webhook management.

use claudear_config::config::SentryConfig;
use claudear_core::error::{Error, Result};
use claudear_core::secret::SecretValue;
use serde::{Deserialize, Serialize};

/// Client for Sentry REST API webhook operations.
pub struct SentryApiClient {
    auth_token: SecretValue,
    org_slug: String,
    client: reqwest::Client,
}

/// Result of a webhook registration.
#[derive(Clone)]
pub struct SentryWebhookRegistration {
    /// The webhook/hook ID.
    pub id: String,
    /// The webhook URL.
    pub url: String,
    /// The signing secret for HMAC verification.
    pub secret: String,
    /// The events subscribed to.
    pub events: Vec<String>,
    /// The project slug this hook is for.
    pub project_slug: String,
}

impl std::fmt::Debug for SentryWebhookRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SentryWebhookRegistration")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("secret", &"[REDACTED]")
            .field("events", &self.events)
            .field("project_slug", &self.project_slug)
            .finish()
    }
}

#[derive(Debug, Serialize)]
struct CreateHookRequest {
    url: String,
    events: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct HookResponse {
    id: String,
    url: String,
    secret: String,
    events: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct HookListItem {
    id: String,
    url: String,
    events: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SentryErrorResponse {
    detail: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProjectResponse {
    slug: String,
}

/// Validate that a slug contains only safe characters for URL path components.
/// Allows alphanumeric, hyphens, underscores, and dots.
fn validate_slug(slug: &str, label: &str) -> Result<()> {
    if slug.is_empty() {
        return Err(Error::config(format!("{} slug cannot be empty", label)));
    }
    if !slug
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(Error::config(format!(
            "{} slug contains invalid characters: {}",
            label, slug
        )));
    }
    Ok(())
}

impl SentryApiClient {
    const BASE_URL: &'static str = "https://sentry.io/api/0";

    /// Create a new Sentry API client.
    pub fn new(auth_token: SecretValue, org_slug: String) -> Self {
        Self {
            auth_token,
            org_slug,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// Create from a SentryConfig.
    pub fn from_config(config: &SentryConfig) -> Self {
        Self::new(config.auth_token.clone(), config.org_slug.clone())
    }

    /// Get the organization slug.
    pub fn org_slug(&self) -> &str {
        &self.org_slug
    }

    /// List all projects in the organization.
    pub async fn list_projects(&self) -> Result<Vec<String>> {
        validate_slug(&self.org_slug, "organization")?;
        let url = format!(
            "{}/organizations/{}/projects/",
            Self::BASE_URL,
            self.org_slug
        );

        let response = self
            .client
            .get(&url)
            .header(
                "Authorization",
                format!("Bearer {}", self.auth_token.expose()),
            )
            .send()
            .await
            .map_err(|e| Error::api(format!("Failed to send request to Sentry API: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(Error::api(format!(
                "Sentry API returned status {}: {}",
                status, body
            )));
        }

        let projects: Vec<ProjectResponse> = response
            .json()
            .await
            .map_err(|e| Error::api(format!("Failed to parse Sentry API response: {}", e)))?;

        Ok(projects.into_iter().map(|p| p.slug).collect())
    }

    /// Register a new webhook (service hook) for a project.
    ///
    /// # Arguments
    /// * `project_slug` - The project slug to register the hook for
    /// * `url` - The URL where Sentry will send webhook events
    /// * `events` - List of events to subscribe to (e.g., ["event.created", "event.alert"])
    ///
    /// # Returns
    /// The webhook registration result including the signing secret.
    pub async fn create_webhook(
        &self,
        project_slug: &str,
        url: &str,
        events: &[&str],
    ) -> Result<SentryWebhookRegistration> {
        validate_slug(&self.org_slug, "organization")?;
        validate_slug(project_slug, "project")?;
        let api_url = format!(
            "{}/projects/{}/{}/hooks/",
            Self::BASE_URL,
            self.org_slug,
            project_slug
        );

        let request_body = CreateHookRequest {
            url: url.to_string(),
            events: events.iter().map(|s| s.to_string()).collect(),
        };

        let response = self
            .client
            .post(&api_url)
            .header(
                "Authorization",
                format!("Bearer {}", self.auth_token.expose()),
            )
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| Error::api(format!("Failed to send request to Sentry API: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());

            // Try to parse the error message
            if let Ok(error_resp) = serde_json::from_str::<SentryErrorResponse>(&body) {
                if let Some(detail) = error_resp.detail {
                    return Err(Error::api(format!(
                        "Sentry API error ({}): {}",
                        status, detail
                    )));
                }
            }

            return Err(Error::api(format!(
                "Sentry API returned status {}: {}",
                status, body
            )));
        }

        let hook: HookResponse = response
            .json()
            .await
            .map_err(|e| Error::api(format!("Failed to parse Sentry API response: {}", e)))?;

        Ok(SentryWebhookRegistration {
            id: hook.id,
            url: hook.url,
            secret: hook.secret,
            events: hook.events,
            project_slug: project_slug.to_string(),
        })
    }

    /// List existing webhooks for a project.
    pub async fn list_webhooks(
        &self,
        project_slug: &str,
    ) -> Result<Vec<(String, String, Vec<String>)>> {
        validate_slug(&self.org_slug, "organization")?;
        validate_slug(project_slug, "project")?;
        let url = format!(
            "{}/projects/{}/{}/hooks/",
            Self::BASE_URL,
            self.org_slug,
            project_slug
        );

        let response = self
            .client
            .get(&url)
            .header(
                "Authorization",
                format!("Bearer {}", self.auth_token.expose()),
            )
            .send()
            .await
            .map_err(|e| Error::api(format!("Failed to send request to Sentry API: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(Error::api(format!(
                "Sentry API returned status {}: {}",
                status, body
            )));
        }

        let hooks: Vec<HookListItem> = response
            .json()
            .await
            .map_err(|e| Error::api(format!("Failed to parse Sentry API response: {}", e)))?;

        Ok(hooks.into_iter().map(|h| (h.id, h.url, h.events)).collect())
    }

    /// Check if a webhook with the given URL already exists for a project.
    pub async fn webhook_exists(&self, project_slug: &str, url: &str) -> Result<bool> {
        let webhooks = self.list_webhooks(project_slug).await?;
        Ok(webhooks.iter().any(|(_, wh_url, _)| wh_url == url))
    }

    /// Delete a webhook by ID for a project.
    pub async fn delete_webhook(&self, project_slug: &str, hook_id: &str) -> Result<()> {
        validate_slug(&self.org_slug, "organization")?;
        validate_slug(project_slug, "project")?;
        if hook_id.is_empty() || !hook_id.chars().all(|c| c.is_alphanumeric() || c == '-') {
            return Err(Error::config(format!(
                "hook ID contains invalid characters: {}",
                hook_id
            )));
        }
        let url = format!(
            "{}/projects/{}/{}/hooks/{}/",
            Self::BASE_URL,
            self.org_slug,
            project_slug,
            hook_id
        );

        let response = self
            .client
            .delete(&url)
            .header(
                "Authorization",
                format!("Bearer {}", self.auth_token.expose()),
            )
            .send()
            .await
            .map_err(|e| Error::api(format!("Failed to send request to Sentry API: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(Error::api(format!(
                "Sentry API returned status {}: {}",
                status, body
            )));
        }

        Ok(())
    }

    /// Register webhooks for multiple projects.
    ///
    /// If `project_slugs` is empty, attempts to register for all projects in the org.
    /// Returns a list of registration results (one per project).
    pub async fn create_webhooks_for_projects(
        &self,
        project_slugs: &[String],
        url: &str,
        events: &[&str],
    ) -> Result<Vec<SentryWebhookRegistration>> {
        let slugs = if project_slugs.is_empty() {
            self.list_projects().await?
        } else {
            project_slugs.to_vec()
        };

        let mut registrations = Vec::new();
        let mut errors = Vec::new();

        for slug in &slugs {
            match self.create_webhook(slug, url, events).await {
                Ok(reg) => registrations.push(reg),
                Err(e) => {
                    // Log but continue with other projects
                    tracing::warn!("Failed to create webhook for project {}: {}", slug, e);
                    errors.push(format!("{}: {}", slug, e));
                }
            }
        }

        if registrations.is_empty() && !errors.is_empty() {
            return Err(Error::api(format!(
                "Failed to create webhooks for any project: {}",
                errors.join("; ")
            )));
        }

        Ok(registrations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_config::config::TopIssuesPeriod;

    #[test]
    fn test_sentry_api_client_new() {
        let client = SentryApiClient::new(SecretValue::new("token"), "my-org".to_string());
        assert_eq!(client.auth_token.expose(), "token");
        assert_eq!(client.org_slug, "my-org");
    }

    #[test]
    fn test_sentry_api_client_from_config() {
        let config = SentryConfig {
            enabled: true,
            auth_token: SecretValue::new("sentry_token"),
            org_slug: "test-org".to_string(),
            project_slugs: vec!["proj1".to_string()],
            top_issues_count: 100,
            top_issues_period: TopIssuesPeriod::OneDay,
            min_event_count: 10,
            escalation_threshold_percent: 50,
            client_secret: None,
            ..Default::default()
        };
        let client = SentryApiClient::from_config(&config);
        assert_eq!(client.auth_token.expose(), "sentry_token");
        assert_eq!(client.org_slug, "test-org");
    }

    #[test]
    fn test_sentry_api_client_org_slug() {
        let client = SentryApiClient::new(SecretValue::new("token"), "my-org".to_string());
        assert_eq!(client.org_slug(), "my-org");
    }

    #[test]
    fn test_sentry_webhook_registration_fields() {
        let registration = SentryWebhookRegistration {
            id: "hook_123".to_string(),
            url: "https://example.com/webhook".to_string(),
            secret: "secret_abc".to_string(),
            events: vec!["event.created".to_string()],
            project_slug: "my-project".to_string(),
        };
        assert_eq!(registration.id, "hook_123");
        assert_eq!(registration.url, "https://example.com/webhook");
        assert_eq!(registration.secret, "secret_abc");
        assert_eq!(registration.events, vec!["event.created"]);
        assert_eq!(registration.project_slug, "my-project");
    }

    #[test]
    fn test_create_hook_request_serialization() {
        let request = CreateHookRequest {
            url: "https://example.com/hook".to_string(),
            events: vec!["event.created".to_string(), "event.alert".to_string()],
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("url"));
        assert!(json.contains("events"));
        assert!(json.contains("event.created"));
    }

    #[test]
    fn test_hook_response_deserialization() {
        let json = r#"{
            "id": "123",
            "url": "https://example.com/hook",
            "secret": "abc123",
            "events": ["event.created"],
            "status": "active",
            "dateCreated": "2024-01-01T00:00:00Z"
        }"#;
        let response: HookResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, "123");
        assert_eq!(response.url, "https://example.com/hook");
        assert_eq!(response.secret, "abc123");
    }

    #[test]
    fn test_hook_list_item_deserialization() {
        let json = r#"[
            {
                "id": "1",
                "url": "https://example.com/hook1",
                "events": ["event.created"],
                "status": "active"
            },
            {
                "id": "2",
                "url": "https://example.com/hook2",
                "events": ["event.alert"],
                "status": "disabled"
            }
        ]"#;
        let hooks: Vec<HookListItem> = serde_json::from_str(json).unwrap();
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].id, "1");
        assert_eq!(hooks[1].url, "https://example.com/hook2");
    }

    #[test]
    fn test_validate_slug_valid_alphanumeric() {
        assert!(validate_slug("my-project", "test").is_ok());
    }

    #[test]
    fn test_validate_slug_valid_with_dots() {
        assert!(validate_slug("my.project.v2", "test").is_ok());
    }

    #[test]
    fn test_validate_slug_valid_with_underscores() {
        assert!(validate_slug("my_project", "test").is_ok());
    }

    #[test]
    fn test_validate_slug_empty() {
        let err = validate_slug("", "test").unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("empty"), "expected 'empty' in error: {}", msg);
    }

    #[test]
    fn test_validate_slug_with_slashes() {
        let err = validate_slug("my/project", "test").unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("invalid"),
            "expected 'invalid' in error: {}",
            msg
        );
    }

    #[test]
    fn test_validate_slug_with_spaces() {
        let err = validate_slug("my project", "test").unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("invalid"),
            "expected 'invalid' in error: {}",
            msg
        );
    }

    #[test]
    fn test_validate_slug_with_special_chars() {
        let err = validate_slug("my@project!", "test").unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("invalid"),
            "expected 'invalid' in error: {}",
            msg
        );
    }

    #[test]
    fn test_validate_slug_unicode() {
        // Note: Rust's is_alphanumeric() considers CJK characters as alphanumeric,
        // so "日本語" passes validation. Use a non-alphanumeric Unicode symbol instead.
        let err = validate_slug("project-\u{2603}", "test").unwrap_err(); // ☃ snowman
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("invalid"),
            "expected 'invalid' in error: {}",
            msg
        );
    }

    #[test]
    fn test_validate_slug_single_char() {
        assert!(validate_slug("a", "test").is_ok());
    }

    #[test]
    fn test_validate_slug_with_numbers_only() {
        assert!(validate_slug("123", "test").is_ok());
    }

    #[test]
    fn test_sentry_error_response_deserialization() {
        let json = r#"{"detail": "Project not found"}"#;
        let resp: SentryErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.detail, Some("Project not found".to_string()));
    }

    #[test]
    fn test_sentry_error_response_no_detail() {
        let json = r#"{"detail": null}"#;
        let resp: SentryErrorResponse = serde_json::from_str(json).unwrap();
        assert!(resp.detail.is_none());
    }

    #[test]
    fn test_project_response_deserialization() {
        let json = r#"{"slug": "my-project"}"#;
        let resp: ProjectResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.slug, "my-project");
    }

    #[test]
    fn test_hook_response_extra_fields_ignored() {
        let json = r#"{
            "id": "456",
            "url": "https://example.com/hook",
            "secret": "s3cret",
            "events": ["event.alert"],
            "status": "active",
            "dateCreated": "2024-06-15T12:00:00Z",
            "unknownField": true,
            "nested": {"key": "value"}
        }"#;
        let resp: HookResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, "456");
        assert_eq!(resp.url, "https://example.com/hook");
        assert_eq!(resp.secret, "s3cret");
        assert_eq!(resp.events, vec!["event.alert"]);
    }

    #[test]
    fn test_hook_list_item_empty_events() {
        let json = r#"{"id": "99", "url": "https://example.com/hook", "events": []}"#;
        let item: HookListItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.id, "99");
        assert!(item.events.is_empty());
    }

    #[test]
    fn test_sentry_webhook_registration_clone() {
        let original = SentryWebhookRegistration {
            id: "hook_1".to_string(),
            url: "https://example.com/wh".to_string(),
            secret: "secret_val".to_string(),
            events: vec!["event.created".to_string(), "event.alert".to_string()],
            project_slug: "proj-a".to_string(),
        };
        let cloned = original.clone();
        assert_eq!(cloned.id, original.id);
        assert_eq!(cloned.url, original.url);
        assert_eq!(cloned.secret, original.secret);
        assert_eq!(cloned.events, original.events);
        assert_eq!(cloned.project_slug, original.project_slug);
    }

    #[test]
    fn test_validate_slug_valid() {
        // Various valid slugs: alphanumeric, hyphens, underscores, dots
        assert!(validate_slug("my-project", "test").is_ok());
        assert!(validate_slug("my_project", "test").is_ok());
        assert!(validate_slug("my.project.v2", "test").is_ok());
        assert!(validate_slug("abc123", "test").is_ok());
        assert!(validate_slug("a", "test").is_ok());
        assert!(validate_slug("UPPERCASE", "test").is_ok());
        assert!(validate_slug("mix-Case_123.v4", "test").is_ok());
    }

    #[test]
    fn test_validate_slug_invalid_chars() {
        // Slugs with spaces, slashes, unicode symbols
        let space = validate_slug("has space", "test");
        assert!(space.is_err());
        assert!(space
            .unwrap_err()
            .to_string()
            .to_lowercase()
            .contains("invalid"));

        let slash = validate_slug("has/slash", "test");
        assert!(slash.is_err());
        assert!(slash
            .unwrap_err()
            .to_string()
            .to_lowercase()
            .contains("invalid"));

        // Non-alphanumeric unicode symbol (snowman)
        let unicode = validate_slug("proj-\u{2603}", "test");
        assert!(unicode.is_err());
        assert!(unicode
            .unwrap_err()
            .to_string()
            .to_lowercase()
            .contains("invalid"));

        let at_sign = validate_slug("proj@v2", "test");
        assert!(at_sign.is_err());

        let bang = validate_slug("proj!", "test");
        assert!(bang.is_err());
    }

    #[test]
    fn test_create_hook_request_events_order() {
        let request = CreateHookRequest {
            url: "https://example.com/hook".to_string(),
            events: vec![
                "event.created".to_string(),
                "event.alert".to_string(),
                "issue.assigned".to_string(),
            ],
        };
        let json = serde_json::to_value(&request).unwrap();
        let events = json["events"].as_array().unwrap();
        assert_eq!(events[0], "event.created");
        assert_eq!(events[1], "event.alert");
        assert_eq!(events[2], "issue.assigned");
    }
}
