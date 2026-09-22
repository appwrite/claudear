//! Linear API client for webhook management.

use claudear_config::config::LinearConfig;
use claudear_core::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// Client for Linear GraphQL API webhook operations.
pub struct LinearApiClient {
    api_key: String,
    client: reqwest::Client,
}

/// Result of a webhook registration.
#[derive(Debug, Clone)]
pub struct WebhookRegistration {
    /// The webhook ID.
    pub id: String,
    /// The webhook URL.
    pub url: String,
    /// Whether the webhook is enabled.
    pub enabled: bool,
    /// The signing secret for HMAC verification.
    pub secret: String,
}

#[derive(Debug, Serialize)]
struct GraphQLRequest {
    query: String,
    variables: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct GraphQLResponse<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQLError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct WebhookCreateResponse {
    #[serde(rename = "webhookCreate")]
    webhook_create: WebhookCreateResult,
}

#[derive(Debug, Deserialize)]
struct WebhookCreateResult {
    success: bool,
    webhook: Option<WebhookData>,
}

#[derive(Debug, Deserialize)]
struct WebhookData {
    id: String,
    url: String,
    enabled: bool,
    secret: String,
}

#[derive(Debug, Deserialize)]
struct WebhooksResponse {
    webhooks: WebhooksConnection,
}

#[derive(Debug, Deserialize)]
struct WebhooksConnection {
    nodes: Vec<WebhookNode>,
}

#[derive(Debug, Deserialize)]
struct WebhookNode {
    id: String,
    url: String,
    enabled: bool,
}

impl LinearApiClient {
    const API_URL: &'static str = "https://api.linear.app/graphql";

    /// Create a new Linear API client.
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: claudear_core::tls::client_builder()
                .timeout(std::time::Duration::from_secs(30))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| claudear_core::tls::client()),
        }
    }

    /// Create from a LinearConfig.
    pub fn from_config(config: &LinearConfig) -> Self {
        Self::new(config.api_key.expose().to_string())
    }

    /// Register a new webhook with Linear.
    ///
    /// # Arguments
    /// * `url` - The URL where Linear will send webhook events
    /// * `team_id` - Optional team ID to filter events (if None, receives all public team events)
    /// * `resource_types` - List of resource types to subscribe to (e.g., ["Issue"])
    ///
    /// # Returns
    /// The webhook registration result including the signing secret.
    pub async fn create_webhook(
        &self,
        url: &str,
        team_id: Option<&str>,
        resource_types: &[&str],
    ) -> Result<WebhookRegistration> {
        let mutation = r#"
            mutation WebhookCreate($input: WebhookCreateInput!) {
                webhookCreate(input: $input) {
                    success
                    webhook {
                        id
                        url
                        enabled
                        secret
                    }
                }
            }
        "#;

        let resource_types_json: Vec<serde_json::Value> = resource_types
            .iter()
            .map(|r| serde_json::Value::String(r.to_string()))
            .collect();

        let mut input = serde_json::json!({
            "url": url,
            "resourceTypes": resource_types_json,
        });

        if let Some(tid) = team_id {
            input["teamId"] = serde_json::Value::String(tid.to_string());
        } else {
            input["allPublicTeams"] = serde_json::Value::Bool(true);
        }

        let request = GraphQLRequest {
            query: mutation.to_string(),
            variables: serde_json::json!({ "input": input }),
        };

        let response = self
            .client
            .post(Self::API_URL)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::api(format!("Failed to send request to Linear API: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(Error::api(format!(
                "Linear API returned status {}: {}",
                status, body
            )));
        }

        let result: GraphQLResponse<WebhookCreateResponse> = response
            .json()
            .await
            .map_err(|e| Error::api(format!("Failed to parse Linear API response: {}", e)))?;

        if let Some(errors) = result.errors {
            let error_messages: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
            return Err(Error::api(format!(
                "Linear API errors: {}",
                error_messages.join(", ")
            )));
        }

        let data = result
            .data
            .ok_or_else(|| Error::api("No data in Linear API response"))?;

        if !data.webhook_create.success {
            return Err(Error::api("Webhook creation failed"));
        }

        let webhook = data
            .webhook_create
            .webhook
            .ok_or_else(|| Error::api("No webhook data in response"))?;

        Ok(WebhookRegistration {
            id: webhook.id,
            url: webhook.url,
            enabled: webhook.enabled,
            secret: webhook.secret,
        })
    }

    /// List existing webhooks for the organization.
    pub async fn list_webhooks(&self) -> Result<Vec<(String, String, bool)>> {
        let query = r#"
            query {
                webhooks {
                    nodes {
                        id
                        url
                        enabled
                    }
                }
            }
        "#;

        let request = GraphQLRequest {
            query: query.to_string(),
            variables: serde_json::json!({}),
        };

        let response = self
            .client
            .post(Self::API_URL)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::api(format!("Failed to send request to Linear API: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(Error::api(format!(
                "Linear API returned status {}: {}",
                status, body
            )));
        }

        let result: GraphQLResponse<WebhooksResponse> = response
            .json()
            .await
            .map_err(|e| Error::api(format!("Failed to parse Linear API response: {}", e)))?;

        if let Some(errors) = result.errors {
            let error_messages: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
            return Err(Error::api(format!(
                "Linear API errors: {}",
                error_messages.join(", ")
            )));
        }

        let data = result
            .data
            .ok_or_else(|| Error::api("No data in Linear API response"))?;

        Ok(data
            .webhooks
            .nodes
            .into_iter()
            .map(|w| (w.id, w.url, w.enabled))
            .collect())
    }

    /// Check if a webhook with the given URL already exists.
    pub async fn webhook_exists(&self, url: &str) -> Result<bool> {
        let webhooks = self.list_webhooks().await?;
        Ok(webhooks.iter().any(|(_, wh_url, _)| wh_url == url))
    }

    /// Delete a webhook by ID.
    pub async fn delete_webhook(&self, webhook_id: &str) -> Result<bool> {
        let mutation = r#"
            mutation WebhookDelete($id: String!) {
                webhookDelete(id: $id) {
                    success
                }
            }
        "#;

        let request = GraphQLRequest {
            query: mutation.to_string(),
            variables: serde_json::json!({ "id": webhook_id }),
        };

        let response = self
            .client
            .post(Self::API_URL)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::api(format!("Failed to send request to Linear API: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(Error::api(format!(
                "Linear API returned status {}: {}",
                status, body
            )));
        }

        #[derive(Debug, Deserialize)]
        struct DeleteResponse {
            #[serde(rename = "webhookDelete")]
            webhook_delete: DeleteResult,
        }

        #[derive(Debug, Deserialize)]
        struct DeleteResult {
            success: bool,
        }

        let result: GraphQLResponse<DeleteResponse> = response
            .json()
            .await
            .map_err(|e| Error::api(format!("Failed to parse Linear API response: {}", e)))?;

        if let Some(errors) = result.errors {
            let error_messages: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
            return Err(Error::api(format!(
                "Linear API errors: {}",
                error_messages.join(", ")
            )));
        }

        let data = result
            .data
            .ok_or_else(|| Error::api("No data in Linear API response"))?;

        Ok(data.webhook_delete.success)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_core::secret::SecretValue;

    #[test]
    fn test_linear_api_client_new() {
        let client = LinearApiClient::new("lin_api_key".to_string());
        assert_eq!(client.api_key, "lin_api_key");
    }

    #[test]
    fn test_linear_api_client_from_config() {
        let config = LinearConfig {
            enabled: true,
            api_key: SecretValue::new("lin_config_key"),
            trigger_labels: vec![],
            trigger_states: vec![],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let client = LinearApiClient::from_config(&config);
        assert_eq!(client.api_key, "lin_config_key");
    }

    #[test]
    fn test_webhook_registration_fields() {
        let registration = WebhookRegistration {
            id: "wh_123".to_string(),
            url: "https://example.com/webhook".to_string(),
            enabled: true,
            secret: "secret_abc".to_string(),
        };
        assert_eq!(registration.id, "wh_123");
        assert_eq!(registration.url, "https://example.com/webhook");
        assert!(registration.enabled);
        assert_eq!(registration.secret, "secret_abc");
    }

    #[test]
    fn test_graphql_request_serialization() {
        let request = GraphQLRequest {
            query: "query { test }".to_string(),
            variables: serde_json::json!({"key": "value"}),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("query"));
        assert!(json.contains("variables"));
    }

    // --- GraphQL response deserialization tests ---

    #[test]
    fn test_graphql_response_with_data() {
        let json = r#"{
            "data": {
                "webhookCreate": {
                    "success": true,
                    "webhook": {
                        "id": "wh_abc123",
                        "url": "https://example.com/hook",
                        "enabled": true,
                        "secret": "whsec_xyz"
                    }
                }
            }
        }"#;
        let resp: GraphQLResponse<WebhookCreateResponse> =
            serde_json::from_str(json).expect("should deserialize");
        assert!(resp.errors.is_none());
        let data = resp.data.expect("data should be present");
        assert!(data.webhook_create.success);
        let webhook = data
            .webhook_create
            .webhook
            .expect("webhook should be present");
        assert_eq!(webhook.id, "wh_abc123");
        assert_eq!(webhook.url, "https://example.com/hook");
        assert!(webhook.enabled);
        assert_eq!(webhook.secret, "whsec_xyz");
    }

    #[test]
    fn test_graphql_response_with_errors() {
        let json = r#"{
            "data": null,
            "errors": [
                {"message": "Authentication required"},
                {"message": "Rate limit exceeded"}
            ]
        }"#;
        let resp: GraphQLResponse<WebhookCreateResponse> =
            serde_json::from_str(json).expect("should deserialize");
        assert!(resp.data.is_none());
        let errors = resp.errors.expect("errors should be present");
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].message, "Authentication required");
        assert_eq!(errors[1].message, "Rate limit exceeded");
    }

    #[test]
    fn test_graphql_response_no_data_no_errors() {
        let json = r#"{"data": null}"#;
        let resp: GraphQLResponse<WebhookCreateResponse> =
            serde_json::from_str(json).expect("should deserialize");
        assert!(resp.data.is_none());
        assert!(resp.errors.is_none());
    }

    #[test]
    fn test_webhook_create_result_not_success() {
        let json = r#"{
            "success": false,
            "webhook": null
        }"#;
        let result: WebhookCreateResult = serde_json::from_str(json).expect("should deserialize");
        assert!(!result.success);
        assert!(result.webhook.is_none());
    }

    #[test]
    fn test_webhook_create_result_success_with_webhook() {
        let json = r#"{
            "success": true,
            "webhook": {
                "id": "wh_full_test",
                "url": "https://hooks.example.com/linear",
                "enabled": true,
                "secret": "s3cr3t_k3y"
            }
        }"#;
        let result: WebhookCreateResult = serde_json::from_str(json).expect("should deserialize");
        assert!(result.success);
        let webhook = result.webhook.expect("webhook should be present");
        assert_eq!(webhook.id, "wh_full_test");
        assert_eq!(webhook.url, "https://hooks.example.com/linear");
        assert!(webhook.enabled);
        assert_eq!(webhook.secret, "s3cr3t_k3y");
    }

    #[test]
    fn test_webhooks_response_empty_nodes() {
        let json = r#"{"webhooks": {"nodes": []}}"#;
        let resp: WebhooksResponse = serde_json::from_str(json).expect("should deserialize");
        assert!(resp.webhooks.nodes.is_empty());
    }

    #[test]
    fn test_webhooks_response_multiple_nodes() {
        let json = r#"{
            "webhooks": {
                "nodes": [
                    {"id": "wh_1", "url": "https://a.com/hook", "enabled": true},
                    {"id": "wh_2", "url": "https://b.com/hook", "enabled": false},
                    {"id": "wh_3", "url": "https://c.com/hook", "enabled": true}
                ]
            }
        }"#;
        let resp: WebhooksResponse = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(resp.webhooks.nodes.len(), 3);
        assert_eq!(resp.webhooks.nodes[0].id, "wh_1");
        assert!(resp.webhooks.nodes[0].enabled);
        assert_eq!(resp.webhooks.nodes[1].id, "wh_2");
        assert!(!resp.webhooks.nodes[1].enabled);
        assert_eq!(resp.webhooks.nodes[2].id, "wh_3");
        assert_eq!(resp.webhooks.nodes[2].url, "https://c.com/hook");
        assert!(resp.webhooks.nodes[2].enabled);
    }

    #[test]
    fn test_webhook_node_deserialization() {
        let json = r#"{"id": "node_42", "url": "https://endpoint.io/wh", "enabled": false}"#;
        let node: WebhookNode = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(node.id, "node_42");
        assert_eq!(node.url, "https://endpoint.io/wh");
        assert!(!node.enabled);
    }

    // --- WebhookRegistration trait tests ---

    #[test]
    fn test_webhook_registration_clone() {
        let original = WebhookRegistration {
            id: "wh_clone".to_string(),
            url: "https://clone.test/hook".to_string(),
            enabled: true,
            secret: "clone_secret".to_string(),
        };
        let cloned = original.clone();
        assert_eq!(cloned.id, original.id);
        assert_eq!(cloned.url, original.url);
        assert_eq!(cloned.enabled, original.enabled);
        assert_eq!(cloned.secret, original.secret);
    }

    #[test]
    fn test_webhook_registration_debug() {
        let registration = WebhookRegistration {
            id: "wh_dbg".to_string(),
            url: "https://debug.test/hook".to_string(),
            enabled: false,
            secret: "dbg_secret".to_string(),
        };
        let debug_str = format!("{:?}", registration);
        assert!(debug_str.contains("wh_dbg"));
        assert!(debug_str.contains("https://debug.test/hook"));
        assert!(debug_str.contains("false"));
        assert!(debug_str.contains("dbg_secret"));
    }

    // --- GraphQLRequest tests ---

    #[test]
    fn test_graphql_request_with_complex_variables() {
        let request = GraphQLRequest {
            query: "mutation { create }".to_string(),
            variables: serde_json::json!({
                "input": {
                    "url": "https://nested.example.com",
                    "resourceTypes": ["Issue", "Comment"],
                    "teamId": "team_123",
                    "allPublicTeams": false
                }
            }),
        };
        let serialized = serde_json::to_value(&request).expect("should serialize");
        let vars = &serialized["variables"]["input"];
        assert_eq!(vars["url"], "https://nested.example.com");
        assert_eq!(vars["resourceTypes"][0], "Issue");
        assert_eq!(vars["resourceTypes"][1], "Comment");
        assert_eq!(vars["teamId"], "team_123");
        assert_eq!(vars["allPublicTeams"], false);
    }

    #[test]
    fn test_graphql_request_empty_variables() {
        let request = GraphQLRequest {
            query: "query { webhooks { nodes { id } } }".to_string(),
            variables: serde_json::json!({}),
        };
        let serialized = serde_json::to_value(&request).expect("should serialize");
        assert_eq!(serialized["variables"], serde_json::json!({}));
        assert_eq!(serialized["query"], "query { webhooks { nodes { id } } }");
    }

    // --- Constructor tests ---

    #[test]
    fn test_linear_api_client_api_key_stored() {
        let key = "lin_api_test_key_12345".to_string();
        let client = LinearApiClient::new(key.clone());
        assert_eq!(client.api_key, key);
    }

    #[test]
    fn test_from_config_with_full_config() {
        let config = LinearConfig {
            enabled: true,
            api_key: SecretValue::new("lin_full_config_key"),
            trigger_labels: vec!["claudear".to_string(), "autofix".to_string()],
            trigger_assignee: Some("Jake".to_string()),
            trigger_states: vec!["Todo".to_string(), "In Progress".to_string()],
            team_id: Some("team_abc".to_string()),
            project_id: Some("proj_xyz".to_string()),
            webhook_secret: Some("whsec_config".into()),
            max_issues_per_cycle: Some(10),
            max_concurrent: Some(3),
            poll_interval_ms: Some(5000),
        };
        let client = LinearApiClient::from_config(&config);
        assert_eq!(client.api_key, "lin_full_config_key");
    }

    // --- API URL constant ---

    #[test]
    fn test_api_url_constant() {
        assert_eq!(LinearApiClient::API_URL, "https://api.linear.app/graphql");
    }

    #[test]
    fn test_api_url_is_https() {
        assert!(LinearApiClient::API_URL.starts_with("https://"));
    }

    #[test]
    fn test_api_url_ends_with_graphql() {
        assert!(LinearApiClient::API_URL.ends_with("/graphql"));
    }

    // --- Constructor edge cases ---

    #[test]
    fn test_new_with_empty_api_key() {
        let client = LinearApiClient::new(String::new());
        assert_eq!(client.api_key, "");
    }

    #[test]
    fn test_new_with_very_long_api_key() {
        let long_key = "k".repeat(10_000);
        let client = LinearApiClient::new(long_key.clone());
        assert_eq!(client.api_key, long_key);
    }

    #[test]
    fn test_new_with_unicode_api_key() {
        let unicode_key = "lin_api_\u{1F680}_key".to_string();
        let client = LinearApiClient::new(unicode_key.clone());
        assert_eq!(client.api_key, unicode_key);
    }

    #[test]
    fn test_new_with_whitespace_api_key() {
        let key = "  lin_api_key  ".to_string();
        let client = LinearApiClient::new(key.clone());
        // Client stores the key as-is, no trimming
        assert_eq!(client.api_key, "  lin_api_key  ");
    }

    #[test]
    fn test_new_with_special_characters_api_key() {
        let key = "lin_api!@#$%^&*()_key".to_string();
        let client = LinearApiClient::new(key.clone());
        assert_eq!(client.api_key, key);
    }

    #[test]
    fn test_from_config_with_default_config() {
        let config = LinearConfig::default();
        let client = LinearApiClient::from_config(&config);
        assert_eq!(client.api_key, "");
    }

    #[test]
    fn test_from_config_only_uses_api_key() {
        // Verify that from_config only extracts the api_key field
        let config = LinearConfig {
            enabled: false,
            api_key: SecretValue::new("only_this_matters"),
            trigger_labels: vec!["irrelevant".to_string()],
            trigger_assignee: Some("ignored".to_string()),
            trigger_states: vec!["ignored".to_string()],
            team_id: Some("ignored".to_string()),
            project_id: Some("ignored".to_string()),
            webhook_secret: Some("ignored".into()),
            max_issues_per_cycle: Some(999),
            max_concurrent: Some(999),
            poll_interval_ms: Some(999),
        };
        let client = LinearApiClient::from_config(&config);
        assert_eq!(client.api_key, "only_this_matters");
    }

    // --- WebhookRegistration edge cases ---

    #[test]
    fn test_webhook_registration_empty_fields() {
        let registration = WebhookRegistration {
            id: String::new(),
            url: String::new(),
            enabled: false,
            secret: String::new(),
        };
        assert_eq!(registration.id, "");
        assert_eq!(registration.url, "");
        assert!(!registration.enabled);
        assert_eq!(registration.secret, "");
    }

    #[test]
    fn test_webhook_registration_disabled() {
        let registration = WebhookRegistration {
            id: "wh_disabled".to_string(),
            url: "https://disabled.test/hook".to_string(),
            enabled: false,
            secret: "secret".to_string(),
        };
        assert!(!registration.enabled);
    }

    #[test]
    fn test_webhook_registration_clone_independence() {
        let original = WebhookRegistration {
            id: "wh_orig".to_string(),
            url: "https://orig.test".to_string(),
            enabled: true,
            secret: "orig_secret".to_string(),
        };
        let mut cloned = original.clone();
        cloned.id = "wh_modified".to_string();
        cloned.enabled = false;
        // Original should be unchanged
        assert_eq!(original.id, "wh_orig");
        assert!(original.enabled);
        assert_eq!(cloned.id, "wh_modified");
        assert!(!cloned.enabled);
    }

    #[test]
    fn test_webhook_registration_debug_contains_all_fields() {
        let registration = WebhookRegistration {
            id: "wh_debug_all".to_string(),
            url: "https://debug-all.test/hook".to_string(),
            enabled: true,
            secret: "debug_all_secret".to_string(),
        };
        let debug = format!("{:?}", registration);
        assert!(debug.contains("WebhookRegistration"));
        assert!(debug.contains("wh_debug_all"));
        assert!(debug.contains("https://debug-all.test/hook"));
        assert!(debug.contains("true"));
        assert!(debug.contains("debug_all_secret"));
    }

    #[test]
    fn test_webhook_registration_with_unicode_url() {
        let registration = WebhookRegistration {
            id: "wh_unicode".to_string(),
            url: "https://example.com/webhook?param=\u{00E9}\u{00F1}".to_string(),
            enabled: true,
            secret: "secret".to_string(),
        };
        assert!(registration.url.contains("\u{00E9}"));
    }

    #[test]
    fn test_webhook_registration_with_long_secret() {
        let long_secret = "s".repeat(1024);
        let registration = WebhookRegistration {
            id: "wh_long".to_string(),
            url: "https://example.com".to_string(),
            enabled: true,
            secret: long_secret.clone(),
        };
        assert_eq!(registration.secret.len(), 1024);
    }

    // --- GraphQLRequest serialization edge cases ---

    #[test]
    fn test_graphql_request_serialization_preserves_query() {
        let query = "mutation WebhookCreate($input: WebhookCreateInput!) { webhookCreate(input: $input) { success } }";
        let request = GraphQLRequest {
            query: query.to_string(),
            variables: serde_json::json!({}),
        };
        let serialized = serde_json::to_value(&request).unwrap();
        assert_eq!(serialized["query"].as_str().unwrap(), query);
    }

    #[test]
    fn test_graphql_request_with_null_variables() {
        let request = GraphQLRequest {
            query: "query { test }".to_string(),
            variables: serde_json::Value::Null,
        };
        let serialized = serde_json::to_value(&request).unwrap();
        assert!(serialized["variables"].is_null());
    }

    #[test]
    fn test_graphql_request_with_nested_variables() {
        let request = GraphQLRequest {
            query: "mutation { m }".to_string(),
            variables: serde_json::json!({
                "input": {
                    "nested": {
                        "deeply": {
                            "nested": "value"
                        }
                    }
                }
            }),
        };
        let serialized = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serialized["variables"]["input"]["nested"]["deeply"]["nested"],
            "value"
        );
    }

    #[test]
    fn test_graphql_request_with_array_variables() {
        let request = GraphQLRequest {
            query: "mutation { m }".to_string(),
            variables: serde_json::json!({
                "input": {
                    "resourceTypes": ["Issue", "Comment", "Project"]
                }
            }),
        };
        let serialized = serde_json::to_value(&request).unwrap();
        let types = serialized["variables"]["input"]["resourceTypes"]
            .as_array()
            .unwrap();
        assert_eq!(types.len(), 3);
    }

    #[test]
    fn test_graphql_request_with_empty_query() {
        let request = GraphQLRequest {
            query: String::new(),
            variables: serde_json::json!({}),
        };
        let serialized = serde_json::to_value(&request).unwrap();
        assert_eq!(serialized["query"], "");
    }

    #[test]
    fn test_graphql_request_serialized_has_exactly_two_keys() {
        let request = GraphQLRequest {
            query: "q".to_string(),
            variables: serde_json::json!({}),
        };
        let serialized = serde_json::to_value(&request).unwrap();
        let obj = serialized.as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert!(obj.contains_key("query"));
        assert!(obj.contains_key("variables"));
    }

    #[test]
    fn test_graphql_request_debug_trait() {
        let request = GraphQLRequest {
            query: "query { test }".to_string(),
            variables: serde_json::json!({"key": "val"}),
        };
        let debug = format!("{:?}", request);
        assert!(debug.contains("GraphQLRequest"));
        assert!(debug.contains("query"));
    }

    // --- GraphQLResponse deserialization edge cases ---

    #[test]
    fn test_graphql_response_with_both_data_and_errors() {
        // GraphQL spec allows both data and errors simultaneously (partial success)
        let json = r#"{
            "data": {
                "webhookCreate": {
                    "success": true,
                    "webhook": {
                        "id": "wh_partial",
                        "url": "https://partial.test",
                        "enabled": true,
                        "secret": "sec"
                    }
                }
            },
            "errors": [
                {"message": "Partial error warning"}
            ]
        }"#;
        let resp: GraphQLResponse<WebhookCreateResponse> =
            serde_json::from_str(json).expect("should deserialize");
        assert!(resp.data.is_some());
        assert!(resp.errors.is_some());
        assert_eq!(resp.errors.unwrap().len(), 1);
    }

    #[test]
    fn test_graphql_response_with_empty_errors_array() {
        let json = r#"{
            "data": null,
            "errors": []
        }"#;
        let resp: GraphQLResponse<WebhookCreateResponse> =
            serde_json::from_str(json).expect("should deserialize");
        assert!(resp.data.is_none());
        let errors = resp.errors.unwrap();
        assert!(errors.is_empty());
    }

    #[test]
    fn test_graphql_response_single_error() {
        let json = r#"{
            "data": null,
            "errors": [{"message": "Only error"}]
        }"#;
        let resp: GraphQLResponse<WebhookCreateResponse> =
            serde_json::from_str(json).expect("should deserialize");
        let errors = resp.errors.unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "Only error");
    }

    #[test]
    fn test_graphql_response_many_errors() {
        let json = r#"{
            "data": null,
            "errors": [
                {"message": "err1"},
                {"message": "err2"},
                {"message": "err3"},
                {"message": "err4"},
                {"message": "err5"}
            ]
        }"#;
        let resp: GraphQLResponse<WebhookCreateResponse> =
            serde_json::from_str(json).expect("should deserialize");
        assert_eq!(resp.errors.unwrap().len(), 5);
    }

    #[test]
    fn test_graphql_error_with_empty_message() {
        let json = r#"{"message": ""}"#;
        let error: GraphQLError = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(error.message, "");
    }

    #[test]
    fn test_graphql_error_with_unicode_message() {
        let json = r#"{"message": "错误: 速率限制 \u2014 请稍后重试"}"#;
        let error: GraphQLError = serde_json::from_str(json).expect("should deserialize");
        assert!(error.message.contains("错误"));
    }

    #[test]
    fn test_graphql_error_with_long_message() {
        let long_msg = "e".repeat(5000);
        let json = format!(r#"{{"message": "{}"}}"#, long_msg);
        let error: GraphQLError = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(error.message.len(), 5000);
    }

    #[test]
    fn test_graphql_error_debug_trait() {
        let json = r#"{"message": "test error"}"#;
        let error: GraphQLError = serde_json::from_str(json).unwrap();
        let debug = format!("{:?}", error);
        assert!(debug.contains("GraphQLError"));
        assert!(debug.contains("test error"));
    }

    // --- WebhookCreateResponse deserialization ---

    #[test]
    fn test_webhook_create_response_success_no_webhook() {
        // Edge case: success is true but webhook is null
        let json = r#"{
            "webhookCreate": {
                "success": true,
                "webhook": null
            }
        }"#;
        let resp: WebhookCreateResponse = serde_json::from_str(json).expect("should deserialize");
        assert!(resp.webhook_create.success);
        assert!(resp.webhook_create.webhook.is_none());
    }

    #[test]
    fn test_webhook_create_response_failure_with_webhook() {
        // Edge case: success is false but webhook data is present
        let json = r#"{
            "webhookCreate": {
                "success": false,
                "webhook": {
                    "id": "wh_fail",
                    "url": "https://fail.test",
                    "enabled": false,
                    "secret": "fail_secret"
                }
            }
        }"#;
        let resp: WebhookCreateResponse = serde_json::from_str(json).expect("should deserialize");
        assert!(!resp.webhook_create.success);
        assert!(resp.webhook_create.webhook.is_some());
    }

    #[test]
    fn test_webhook_create_response_debug_trait() {
        let json = r#"{
            "webhookCreate": {
                "success": true,
                "webhook": null
            }
        }"#;
        let resp: WebhookCreateResponse = serde_json::from_str(json).unwrap();
        let debug = format!("{:?}", resp);
        assert!(debug.contains("WebhookCreateResponse"));
    }

    // --- WebhookData deserialization edge cases ---

    #[test]
    fn test_webhook_data_empty_strings() {
        let json = r#"{"id": "", "url": "", "enabled": false, "secret": ""}"#;
        let data: WebhookData = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(data.id, "");
        assert_eq!(data.url, "");
        assert!(!data.enabled);
        assert_eq!(data.secret, "");
    }

    #[test]
    fn test_webhook_data_enabled_false() {
        let json = r#"{"id": "wh_dis", "url": "https://disabled.test", "enabled": false, "secret": "sec"}"#;
        let data: WebhookData = serde_json::from_str(json).expect("should deserialize");
        assert!(!data.enabled);
    }

    #[test]
    fn test_webhook_data_with_special_chars_in_url() {
        let json = r#"{"id": "wh_sc", "url": "https://example.com/hook?token=abc&type=linear", "enabled": true, "secret": "sec"}"#;
        let data: WebhookData = serde_json::from_str(json).expect("should deserialize");
        assert!(data.url.contains("token=abc"));
        assert!(data.url.contains("&type=linear"));
    }

    #[test]
    fn test_webhook_data_with_very_long_secret() {
        let long_secret = "s".repeat(2048);
        let json = format!(
            r#"{{"id": "wh_long", "url": "https://test.com", "enabled": true, "secret": "{}"}}"#,
            long_secret
        );
        let data: WebhookData = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(data.secret.len(), 2048);
    }

    #[test]
    fn test_webhook_data_debug_trait() {
        let json =
            r#"{"id": "wh_dbg", "url": "https://test.com", "enabled": true, "secret": "sec"}"#;
        let data: WebhookData = serde_json::from_str(json).unwrap();
        let debug = format!("{:?}", data);
        assert!(debug.contains("WebhookData"));
        assert!(debug.contains("wh_dbg"));
    }

    // --- WebhooksResponse deserialization edge cases ---

    #[test]
    fn test_webhooks_response_single_node() {
        let json = r#"{
            "webhooks": {
                "nodes": [
                    {"id": "wh_single", "url": "https://single.test", "enabled": true}
                ]
            }
        }"#;
        let resp: WebhooksResponse = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(resp.webhooks.nodes.len(), 1);
        assert_eq!(resp.webhooks.nodes[0].id, "wh_single");
    }

    #[test]
    fn test_webhooks_response_large_number_of_nodes() {
        let nodes: Vec<String> = (0..100)
            .map(|i| {
                format!(
                    r#"{{"id": "wh_{}", "url": "https://test{}.com", "enabled": {}}}"#,
                    i,
                    i,
                    if i % 2 == 0 { "true" } else { "false" }
                )
            })
            .collect();
        let json = format!(r#"{{"webhooks": {{"nodes": [{}]}}}}"#, nodes.join(","));
        let resp: WebhooksResponse = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(resp.webhooks.nodes.len(), 100);
        assert!(resp.webhooks.nodes[0].enabled);
        assert!(!resp.webhooks.nodes[1].enabled);
    }

    #[test]
    fn test_webhooks_connection_debug_trait() {
        let json = r#"{"nodes": []}"#;
        let conn: WebhooksConnection = serde_json::from_str(json).unwrap();
        let debug = format!("{:?}", conn);
        assert!(debug.contains("WebhooksConnection"));
    }

    #[test]
    fn test_webhooks_response_debug_trait() {
        let json = r#"{"webhooks": {"nodes": []}}"#;
        let resp: WebhooksResponse = serde_json::from_str(json).unwrap();
        let debug = format!("{:?}", resp);
        assert!(debug.contains("WebhooksResponse"));
    }

    // --- WebhookNode edge cases ---

    #[test]
    fn test_webhook_node_empty_id_and_url() {
        let json = r#"{"id": "", "url": "", "enabled": false}"#;
        let node: WebhookNode = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(node.id, "");
        assert_eq!(node.url, "");
        assert!(!node.enabled);
    }

    #[test]
    fn test_webhook_node_debug_trait() {
        let json = r#"{"id": "wh_nd", "url": "https://nd.test", "enabled": true}"#;
        let node: WebhookNode = serde_json::from_str(json).unwrap();
        let debug = format!("{:?}", node);
        assert!(debug.contains("WebhookNode"));
        assert!(debug.contains("wh_nd"));
    }

    // --- Deserialization failure tests ---

    #[test]
    fn test_graphql_error_missing_message_field() {
        let json = r#"{"other": "field"}"#;
        let result = serde_json::from_str::<GraphQLError>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_webhook_data_missing_required_field() {
        // Missing "secret"
        let json = r#"{"id": "wh_1", "url": "https://test.com", "enabled": true}"#;
        let result = serde_json::from_str::<WebhookData>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_webhook_data_missing_id() {
        let json = r#"{"url": "https://test.com", "enabled": true, "secret": "sec"}"#;
        let result = serde_json::from_str::<WebhookData>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_webhook_data_missing_url() {
        let json = r#"{"id": "wh_1", "enabled": true, "secret": "sec"}"#;
        let result = serde_json::from_str::<WebhookData>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_webhook_data_missing_enabled() {
        let json = r#"{"id": "wh_1", "url": "https://test.com", "secret": "sec"}"#;
        let result = serde_json::from_str::<WebhookData>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_webhook_node_missing_id() {
        let json = r#"{"url": "https://test.com", "enabled": true}"#;
        let result = serde_json::from_str::<WebhookNode>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_webhook_node_missing_url() {
        let json = r#"{"id": "wh_1", "enabled": true}"#;
        let result = serde_json::from_str::<WebhookNode>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_webhook_node_missing_enabled() {
        let json = r#"{"id": "wh_1", "url": "https://test.com"}"#;
        let result = serde_json::from_str::<WebhookNode>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_webhook_create_result_missing_success() {
        let json = r#"{"webhook": null}"#;
        let result = serde_json::from_str::<WebhookCreateResult>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_webhooks_connection_missing_nodes() {
        let json = r#"{}"#;
        let result = serde_json::from_str::<WebhooksConnection>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_graphql_response_invalid_json() {
        let json = r#"not json at all"#;
        let result = serde_json::from_str::<GraphQLResponse<WebhookCreateResponse>>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_graphql_response_empty_json_object() {
        // Both `data` and `errors` are `Option<T>`, so serde defaults them to `None`
        // when the keys are absent. An empty JSON object is therefore valid.
        let json = r#"{}"#;
        let result: GraphQLResponse<WebhookCreateResponse> =
            serde_json::from_str(json).expect("should deserialize with optional fields absent");
        assert!(result.data.is_none());
        assert!(result.errors.is_none());
    }

    #[test]
    fn test_webhook_data_wrong_type_for_enabled() {
        let json =
            r#"{"id": "wh_1", "url": "https://test.com", "enabled": "yes", "secret": "sec"}"#;
        let result = serde_json::from_str::<WebhookData>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_webhook_node_wrong_type_for_id() {
        let json = r#"{"id": 123, "url": "https://test.com", "enabled": true}"#;
        let result = serde_json::from_str::<WebhookNode>(json);
        assert!(result.is_err());
    }

    // --- Serialization roundtrip tests ---

    #[test]
    fn test_graphql_request_roundtrip_serialization() {
        let request = GraphQLRequest {
            query: "mutation { test($input: Input!) }".to_string(),
            variables: serde_json::json!({
                "input": {
                    "url": "https://roundtrip.test",
                    "resourceTypes": ["Issue"]
                }
            }),
        };
        let serialized = serde_json::to_string(&request).unwrap();
        let deserialized: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized["query"], "mutation { test($input: Input!) }");
        assert_eq!(
            deserialized["variables"]["input"]["url"],
            "https://roundtrip.test"
        );
    }

    #[test]
    fn test_graphql_request_serialization_json_string_format() {
        let request = GraphQLRequest {
            query: "query { q }".to_string(),
            variables: serde_json::json!({}),
        };
        let json_str = serde_json::to_string(&request).unwrap();
        // Should be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.is_object());
    }

    // --- create_webhook input construction simulation ---

    #[test]
    fn test_create_webhook_input_with_team_id() {
        // Simulate the input construction from create_webhook method
        let url = "https://example.com/webhook";
        let team_id = Some("team_123");
        let resource_types: &[&str] = &["Issue", "Comment"];

        let resource_types_json: Vec<serde_json::Value> = resource_types
            .iter()
            .map(|r| serde_json::Value::String(r.to_string()))
            .collect();

        let mut input = serde_json::json!({
            "url": url,
            "resourceTypes": resource_types_json,
        });

        if let Some(tid) = team_id {
            input["teamId"] = serde_json::Value::String(tid.to_string());
        } else {
            input["allPublicTeams"] = serde_json::Value::Bool(true);
        }

        assert_eq!(input["url"], "https://example.com/webhook");
        assert_eq!(input["teamId"], "team_123");
        assert!(input.get("allPublicTeams").is_none());
        let types = input["resourceTypes"].as_array().unwrap();
        assert_eq!(types.len(), 2);
        assert_eq!(types[0], "Issue");
        assert_eq!(types[1], "Comment");
    }

    #[test]
    fn test_create_webhook_input_without_team_id() {
        // Simulate the input construction when team_id is None
        let url = "https://example.com/webhook";
        let team_id: Option<&str> = None;
        let resource_types: &[&str] = &["Issue"];

        let resource_types_json: Vec<serde_json::Value> = resource_types
            .iter()
            .map(|r| serde_json::Value::String(r.to_string()))
            .collect();

        let mut input = serde_json::json!({
            "url": url,
            "resourceTypes": resource_types_json,
        });

        if let Some(tid) = team_id {
            input["teamId"] = serde_json::Value::String(tid.to_string());
        } else {
            input["allPublicTeams"] = serde_json::Value::Bool(true);
        }

        assert_eq!(input["url"], "https://example.com/webhook");
        assert!(input.get("teamId").is_none());
        assert_eq!(input["allPublicTeams"], true);
    }

    #[test]
    fn test_create_webhook_input_empty_resource_types() {
        let resource_types: &[&str] = &[];
        let resource_types_json: Vec<serde_json::Value> = resource_types
            .iter()
            .map(|r| serde_json::Value::String(r.to_string()))
            .collect();

        let input = serde_json::json!({
            "url": "https://test.com",
            "resourceTypes": resource_types_json,
        });

        assert!(input["resourceTypes"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_create_webhook_input_many_resource_types() {
        let resource_types: &[&str] = &["Issue", "Comment", "Project", "Cycle", "Label", "Team"];
        let resource_types_json: Vec<serde_json::Value> = resource_types
            .iter()
            .map(|r| serde_json::Value::String(r.to_string()))
            .collect();

        let input = serde_json::json!({
            "url": "https://test.com",
            "resourceTypes": resource_types_json,
        });

        assert_eq!(input["resourceTypes"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn test_create_webhook_variables_wrapping() {
        // Simulate the full variables wrapping
        let input = serde_json::json!({
            "url": "https://test.com",
            "resourceTypes": ["Issue"],
            "allPublicTeams": true
        });
        let variables = serde_json::json!({ "input": input });
        assert!(variables["input"].is_object());
        assert_eq!(variables["input"]["url"], "https://test.com");
    }

    // --- Error message formatting simulation ---

    #[test]
    fn test_error_message_for_graphql_errors() {
        let errors = [
            GraphQLError {
                message: "Auth required".to_string(),
            },
            GraphQLError {
                message: "Rate limit".to_string(),
            },
        ];
        let error_messages: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
        let joined = error_messages.join(", ");
        assert_eq!(joined, "Auth required, Rate limit");
        let full_msg = format!("Linear API errors: {}", joined);
        assert_eq!(full_msg, "Linear API errors: Auth required, Rate limit");
    }

    #[test]
    fn test_error_message_for_single_graphql_error() {
        let errors = [GraphQLError {
            message: "Unauthorized".to_string(),
        }];
        let error_messages: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
        let joined = error_messages.join(", ");
        assert_eq!(joined, "Unauthorized");
    }

    #[test]
    fn test_error_message_for_empty_graphql_errors() {
        let errors: Vec<GraphQLError> = vec![];
        let error_messages: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
        let joined = error_messages.join(", ");
        assert_eq!(joined, "");
    }

    #[test]
    fn test_error_api_formatting() {
        let err = Error::api("Linear API returned status 401: Unauthorized");
        assert_eq!(
            err.to_string(),
            "API error: Linear API returned status 401: Unauthorized"
        );
    }

    #[test]
    fn test_error_api_no_data_message() {
        let err = Error::api("No data in Linear API response");
        assert!(err.to_string().contains("No data"));
    }

    #[test]
    fn test_error_api_webhook_creation_failed() {
        let err = Error::api("Webhook creation failed");
        assert!(err.to_string().contains("Webhook creation failed"));
    }

    #[test]
    fn test_error_api_no_webhook_data() {
        let err = Error::api("No webhook data in response");
        assert!(err.to_string().contains("No webhook data"));
    }

    #[test]
    fn test_error_api_http_status_formatting() {
        // Simulate the error message format used in the code
        let status = 500u16;
        let body = "Internal Server Error";
        let msg = format!("Linear API returned status {}: {}", status, body);
        let err = Error::api(msg);
        assert!(err.to_string().contains("500"));
        assert!(err.to_string().contains("Internal Server Error"));
    }

    #[test]
    fn test_error_api_parse_failure_message() {
        let msg = "Failed to parse Linear API response: invalid type";
        let err = Error::api(msg);
        assert!(err.to_string().contains("Failed to parse"));
    }

    #[test]
    fn test_error_api_send_failure_message() {
        let msg = "Failed to send request to Linear API: connection timeout";
        let err = Error::api(msg);
        assert!(err.to_string().contains("Failed to send request"));
        assert!(err.to_string().contains("connection timeout"));
    }

    // --- GraphQL response extraction simulation (mimics method logic) ---

    #[test]
    fn test_extract_webhook_registration_from_response() {
        let json = r#"{
            "data": {
                "webhookCreate": {
                    "success": true,
                    "webhook": {
                        "id": "wh_extracted",
                        "url": "https://extracted.test/hook",
                        "enabled": true,
                        "secret": "extracted_secret"
                    }
                }
            }
        }"#;
        let result: GraphQLResponse<WebhookCreateResponse> = serde_json::from_str(json).unwrap();

        // Simulate the extraction logic from create_webhook
        assert!(result.errors.is_none());
        let data = result.data.unwrap();
        assert!(data.webhook_create.success);
        let webhook = data.webhook_create.webhook.unwrap();

        let registration = WebhookRegistration {
            id: webhook.id,
            url: webhook.url,
            enabled: webhook.enabled,
            secret: webhook.secret,
        };

        assert_eq!(registration.id, "wh_extracted");
        assert_eq!(registration.url, "https://extracted.test/hook");
        assert!(registration.enabled);
        assert_eq!(registration.secret, "extracted_secret");
    }

    #[test]
    fn test_extract_webhook_list_from_response() {
        let json = r#"{
            "data": {
                "webhooks": {
                    "nodes": [
                        {"id": "wh_1", "url": "https://a.test", "enabled": true},
                        {"id": "wh_2", "url": "https://b.test", "enabled": false}
                    ]
                }
            }
        }"#;
        let result: GraphQLResponse<WebhooksResponse> = serde_json::from_str(json).unwrap();

        assert!(result.errors.is_none());
        let data = result.data.unwrap();

        // Simulate the extraction logic from list_webhooks
        let list: Vec<(String, String, bool)> = data
            .webhooks
            .nodes
            .into_iter()
            .map(|w| (w.id, w.url, w.enabled))
            .collect();

        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0],
            ("wh_1".to_string(), "https://a.test".to_string(), true)
        );
        assert_eq!(
            list[1],
            ("wh_2".to_string(), "https://b.test".to_string(), false)
        );
    }

    #[test]
    fn test_webhook_exists_logic_found() {
        // Simulate webhook_exists logic
        let webhooks = [
            ("wh_1".to_string(), "https://a.test/hook".to_string(), true),
            ("wh_2".to_string(), "https://b.test/hook".to_string(), false),
        ];
        let target_url = "https://a.test/hook";
        let exists = webhooks.iter().any(|(_, wh_url, _)| wh_url == target_url);
        assert!(exists);
    }

    #[test]
    fn test_webhook_exists_logic_not_found() {
        let webhooks = [("wh_1".to_string(), "https://a.test/hook".to_string(), true)];
        let target_url = "https://not-found.test/hook";
        let exists = webhooks.iter().any(|(_, wh_url, _)| wh_url == target_url);
        assert!(!exists);
    }

    #[test]
    fn test_webhook_exists_logic_empty_list() {
        let webhooks: Vec<(String, String, bool)> = vec![];
        let target_url = "https://any.test/hook";
        let exists = webhooks.iter().any(|(_, wh_url, _)| wh_url == target_url);
        assert!(!exists);
    }

    #[test]
    fn test_webhook_exists_url_exact_match() {
        // URL matching should be exact, not substring
        let webhooks = [(
            "wh_1".to_string(),
            "https://example.com/hook".to_string(),
            true,
        )];
        let target_url = "https://example.com/hook/extra";
        let exists = webhooks.iter().any(|(_, wh_url, _)| wh_url == target_url);
        assert!(!exists);
    }

    #[test]
    fn test_webhook_exists_url_case_sensitive() {
        let webhooks = [(
            "wh_1".to_string(),
            "https://Example.Com/Hook".to_string(),
            true,
        )];
        let target_url = "https://example.com/hook";
        let exists = webhooks.iter().any(|(_, wh_url, _)| wh_url == target_url);
        assert!(!exists);
    }

    // --- Delete webhook response simulation ---

    /// Test webhook_exists logic returns true when a matching webhook URL is present.
    #[test]
    fn test_webhook_exists_true() {
        // Simulate the webhook_exists logic: list_webhooks returns tuples, then
        // we check if any URL matches.
        let webhooks = [
            (
                "wh_1".to_string(),
                "https://example.com/webhook/linear".to_string(),
                true,
            ),
            (
                "wh_2".to_string(),
                "https://other.com/hook".to_string(),
                true,
            ),
        ];
        let target_url = "https://example.com/webhook/linear";
        let exists = webhooks.iter().any(|(_, wh_url, _)| wh_url == target_url);
        assert!(exists, "should find a webhook with matching URL");
    }

    /// Test webhook_exists logic returns false when no matching webhook URL is present.
    #[test]
    fn test_webhook_exists_false() {
        let webhooks = [
            (
                "wh_1".to_string(),
                "https://example.com/webhook/linear".to_string(),
                true,
            ),
            (
                "wh_2".to_string(),
                "https://other.com/hook".to_string(),
                false,
            ),
        ];
        let target_url = "https://nonexistent.com/webhook";
        let exists = webhooks.iter().any(|(_, wh_url, _)| wh_url == target_url);
        assert!(
            !exists,
            "should not find a webhook when URL does not match any"
        );

        // Also test with empty list
        let empty: Vec<(String, String, bool)> = vec![];
        let exists_empty = empty.iter().any(|(_, wh_url, _)| wh_url == target_url);
        assert!(!exists_empty, "should not find a webhook in an empty list");
    }

    #[test]
    fn test_delete_webhook_success_response() {
        #[derive(Debug, Deserialize)]
        struct DeleteResponse {
            #[serde(rename = "webhookDelete")]
            webhook_delete: DeleteResult,
        }
        #[derive(Debug, Deserialize)]
        struct DeleteResult {
            success: bool,
        }

        let json = r#"{
            "data": {
                "webhookDelete": {
                    "success": true
                }
            }
        }"#;
        let result: GraphQLResponse<DeleteResponse> = serde_json::from_str(json).unwrap();
        assert!(result.data.unwrap().webhook_delete.success);
    }

    #[test]
    fn test_delete_webhook_failure_response() {
        #[derive(Debug, Deserialize)]
        struct DeleteResponse {
            #[serde(rename = "webhookDelete")]
            webhook_delete: DeleteResult,
        }
        #[derive(Debug, Deserialize)]
        struct DeleteResult {
            success: bool,
        }

        let json = r#"{
            "data": {
                "webhookDelete": {
                    "success": false
                }
            }
        }"#;
        let result: GraphQLResponse<DeleteResponse> = serde_json::from_str(json).unwrap();
        assert!(!result.data.unwrap().webhook_delete.success);
    }

    #[test]
    fn test_delete_webhook_with_errors_response() {
        #[derive(Debug, Deserialize)]
        #[expect(dead_code)]
        struct DeleteResponse {
            #[serde(rename = "webhookDelete")]
            webhook_delete: DeleteResult,
        }
        #[derive(Debug, Deserialize)]
        #[expect(dead_code)]
        struct DeleteResult {
            success: bool,
        }

        let json = r#"{
            "data": null,
            "errors": [{"message": "Webhook not found"}]
        }"#;
        let result: GraphQLResponse<DeleteResponse> = serde_json::from_str(json).unwrap();
        assert!(result.data.is_none());
        assert_eq!(result.errors.unwrap()[0].message, "Webhook not found");
    }

    // --- Extra deserialization tests with additional/unknown fields ---

    #[test]
    fn test_webhook_data_ignores_extra_fields() {
        // serde by default ignores unknown fields
        let json = r#"{
            "id": "wh_extra",
            "url": "https://extra.test",
            "enabled": true,
            "secret": "sec",
            "extra_field": "should be ignored",
            "another": 42
        }"#;
        let data: WebhookData = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(data.id, "wh_extra");
    }

    #[test]
    fn test_webhook_node_ignores_extra_fields() {
        let json = r#"{
            "id": "wh_extra",
            "url": "https://extra.test",
            "enabled": true,
            "label": "some label",
            "resourceTypes": ["Issue"]
        }"#;
        let node: WebhookNode = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(node.id, "wh_extra");
    }

    #[test]
    fn test_graphql_error_ignores_extra_fields() {
        // GraphQL errors often have extra fields like "locations", "path", "extensions"
        let json = r#"{
            "message": "error msg",
            "locations": [{"line": 1, "column": 2}],
            "path": ["webhookCreate"],
            "extensions": {"code": "FORBIDDEN"}
        }"#;
        let error: GraphQLError = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(error.message, "error msg");
    }

    // --- GraphQLResponse with different data types ---

    #[test]
    fn test_graphql_response_webhooks_list_type() {
        let json = r#"{
            "data": {
                "webhooks": {
                    "nodes": []
                }
            }
        }"#;
        let resp: GraphQLResponse<WebhooksResponse> =
            serde_json::from_str(json).expect("should deserialize");
        assert!(resp.data.is_some());
        assert!(resp.data.unwrap().webhooks.nodes.is_empty());
    }

    #[test]
    fn test_graphql_response_webhooks_list_with_errors() {
        let json = r#"{
            "data": null,
            "errors": [{"message": "Forbidden"}]
        }"#;
        let resp: GraphQLResponse<WebhooksResponse> =
            serde_json::from_str(json).expect("should deserialize");
        assert!(resp.data.is_none());
        assert_eq!(resp.errors.unwrap()[0].message, "Forbidden");
    }

    // --- WebhookCreateResult edge cases ---

    #[test]
    fn test_webhook_create_result_success_false_webhook_none() {
        let json = r#"{"success": false, "webhook": null}"#;
        let result: WebhookCreateResult = serde_json::from_str(json).unwrap();
        assert!(!result.success);
        assert!(result.webhook.is_none());
    }

    #[test]
    fn test_webhook_create_result_debug_trait() {
        let json = r#"{"success": true, "webhook": null}"#;
        let result: WebhookCreateResult = serde_json::from_str(json).unwrap();
        let debug = format!("{:?}", result);
        assert!(debug.contains("WebhookCreateResult"));
    }

    // --- WebhookData deserialization ---

    #[test]
    fn test_webhook_data_deserialization() {
        let json = r#"{
            "id": "wh_data_1",
            "url": "https://data.test/hook",
            "enabled": false,
            "secret": "data_secret"
        }"#;
        let data: WebhookData = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(data.id, "wh_data_1");
        assert_eq!(data.url, "https://data.test/hook");
        assert!(!data.enabled);
        assert_eq!(data.secret, "data_secret");
    }

    #[test]
    fn test_webhook_data_debug() {
        let json = r#"{
            "id": "wh_dbg",
            "url": "https://debug.test",
            "enabled": true,
            "secret": "sec"
        }"#;
        let data: WebhookData = serde_json::from_str(json).unwrap();
        let debug = format!("{:?}", data);
        assert!(debug.contains("WebhookData"));
        assert!(debug.contains("wh_dbg"));
    }

    // --- WebhooksConnection deserialization ---

    #[test]
    fn test_webhooks_connection_empty() {
        let json = r#"{"nodes": []}"#;
        let conn: WebhooksConnection = serde_json::from_str(json).unwrap();
        assert!(conn.nodes.is_empty());
    }

    #[test]
    fn test_webhooks_connection_with_nodes() {
        let json = r#"{"nodes": [
            {"id": "n1", "url": "https://a.com", "enabled": true},
            {"id": "n2", "url": "https://b.com", "enabled": false}
        ]}"#;
        let conn: WebhooksConnection = serde_json::from_str(json).unwrap();
        assert_eq!(conn.nodes.len(), 2);
        assert_eq!(conn.nodes[0].id, "n1");
        assert!(!conn.nodes[1].enabled);
    }

    // --- Full GraphQL response round-trips for WebhooksResponse ---

    #[test]
    fn test_graphql_webhooks_response_full() {
        let json = r#"{
            "data": {
                "webhooks": {
                    "nodes": [
                        {"id": "wh_a", "url": "https://a.test", "enabled": true},
                        {"id": "wh_b", "url": "https://b.test", "enabled": false}
                    ]
                }
            }
        }"#;
        let resp: GraphQLResponse<WebhooksResponse> = serde_json::from_str(json).unwrap();
        let data = resp.data.unwrap();
        assert_eq!(data.webhooks.nodes.len(), 2);
        assert_eq!(data.webhooks.nodes[0].id, "wh_a");
        assert!(data.webhooks.nodes[0].enabled);
        assert_eq!(data.webhooks.nodes[1].url, "https://b.test");
    }

    // --- GraphQLError deserialization ---

    #[test]
    fn test_graphql_error_deserialization() {
        let json = r#"{"message": "Something went wrong"}"#;
        let err: GraphQLError = serde_json::from_str(json).unwrap();
        assert_eq!(err.message, "Something went wrong");
    }

    #[test]
    fn test_graphql_error_debug() {
        let json = r#"{"message": "debug test"}"#;
        let err: GraphQLError = serde_json::from_str(json).unwrap();
        let debug = format!("{:?}", err);
        assert!(debug.contains("GraphQLError"));
        assert!(debug.contains("debug test"));
    }

    #[test]
    fn test_graphql_error_empty_message() {
        let json = r#"{"message": ""}"#;
        let err: GraphQLError = serde_json::from_str(json).unwrap();
        assert_eq!(err.message, "");
    }

    // --- WebhookCreateResponse deserialization (extended) ---

    #[test]
    fn test_webhook_create_response_success_no_webhook_2() {
        let json = r#"{
            "webhookCreate": {
                "success": true,
                "webhook": null
            }
        }"#;
        let resp: WebhookCreateResponse = serde_json::from_str(json).unwrap();
        assert!(resp.webhook_create.success);
        assert!(resp.webhook_create.webhook.is_none());
    }

    #[test]
    fn test_webhook_create_response_with_all_fields() {
        let json = r#"{
            "webhookCreate": {
                "success": true,
                "webhook": {
                    "id": "wh_full",
                    "url": "https://full.test/hook",
                    "enabled": true,
                    "secret": "full_secret"
                }
            }
        }"#;
        let resp: WebhookCreateResponse = serde_json::from_str(json).unwrap();
        assert!(resp.webhook_create.success);
        let wh = resp.webhook_create.webhook.unwrap();
        assert_eq!(wh.id, "wh_full");
        assert_eq!(wh.url, "https://full.test/hook");
        assert!(wh.enabled);
        assert_eq!(wh.secret, "full_secret");
    }

    // --- WebhookNode edge cases ---

    #[test]
    fn test_webhook_node_with_empty_fields() {
        let json = r#"{"id": "", "url": "", "enabled": false}"#;
        let node: WebhookNode = serde_json::from_str(json).unwrap();
        assert_eq!(node.id, "");
        assert_eq!(node.url, "");
        assert!(!node.enabled);
    }

    #[test]
    fn test_webhook_node_debug() {
        let json = r#"{"id": "dbg_node", "url": "https://dbg.test", "enabled": true}"#;
        let node: WebhookNode = serde_json::from_str(json).unwrap();
        let debug = format!("{:?}", node);
        assert!(debug.contains("WebhookNode"));
        assert!(debug.contains("dbg_node"));
    }

    // --- GraphQLResponse with WebhooksResponse error path ---

    #[test]
    fn test_graphql_webhooks_response_with_errors_only() {
        let json = r#"{
            "data": null,
            "errors": [{"message": "Unauthorized access"}]
        }"#;
        let resp: GraphQLResponse<WebhooksResponse> = serde_json::from_str(json).unwrap();
        assert!(resp.data.is_none());
        let errors = resp.errors.unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "Unauthorized access");
    }

    // --- WebhookCreateResult with success true but no webhook data ---

    #[test]
    fn test_webhook_create_result_success_true_no_webhook() {
        let json = r#"{"success": true, "webhook": null}"#;
        let result: WebhookCreateResult = serde_json::from_str(json).unwrap();
        assert!(result.success);
        assert!(result.webhook.is_none());
    }

    // --- Multiple GraphQL errors ---

    #[test]
    fn test_graphql_response_multiple_errors_for_webhooks() {
        let json = r#"{
            "data": null,
            "errors": [
                {"message": "err_a"},
                {"message": "err_b"},
                {"message": "err_c"}
            ]
        }"#;
        let resp: GraphQLResponse<WebhooksResponse> = serde_json::from_str(json).unwrap();
        let errors = resp.errors.unwrap();
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0].message, "err_a");
        assert_eq!(errors[1].message, "err_b");
        assert_eq!(errors[2].message, "err_c");
    }
}
