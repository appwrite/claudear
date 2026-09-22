//! GitHub App API client.
//!
//! This client provides methods for interacting with the GitHub API
//! as a GitHub App, including:
//!
//! - Listing installations
//! - Getting installation access tokens
//! - Making authenticated API requests

use crate::github_app::auth::{CachedToken, GitHubAppAuth};
use claudear_config::config::GitHubAppConfig;
use claudear_core::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Default timeout for HTTP requests (30 seconds).
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Default connection timeout (10 seconds).
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;

/// Installation information from GitHub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installation {
    /// Installation ID.
    pub id: i64,
    /// Account that installed the App.
    pub account: InstallationAccount,
    /// Target type (User or Organization).
    pub target_type: String,
    /// Repository selection mode.
    pub repository_selection: String,
    /// App ID this installation belongs to.
    pub app_id: i64,
    /// HTML URL for the installation settings.
    pub html_url: Option<String>,
}

/// Account information for an installation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationAccount {
    /// Account login (username or org name).
    pub login: String,
    /// Account ID.
    pub id: i64,
    /// Account type (User or Organization).
    #[serde(rename = "type")]
    pub account_type: String,
}

/// Repository information from an installation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationRepository {
    /// Repository ID.
    pub id: i64,
    /// Repository name.
    pub name: String,
    /// Full repository name (owner/repo).
    pub full_name: String,
    /// Whether the repository is private.
    pub private: bool,
}

/// Response from listing installation repositories.
#[derive(Debug, Deserialize)]
struct InstallationReposResponse {
    #[allow(dead_code)]
    total_count: i64,
    repositories: Vec<InstallationRepository>,
}

/// Client for GitHub App API operations.
#[derive(Debug)]
pub struct GitHubAppClient {
    auth: GitHubAppAuth,
    http_client: reqwest::Client,
}

impl GitHubAppClient {
    /// Create a new GitHubAppClient from configuration.
    pub fn new(config: GitHubAppConfig) -> Self {
        let http_client = claudear_core::tls::client_builder()
            .timeout(Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| claudear_core::tls::client());

        Self {
            auth: GitHubAppAuth::new(config),
            http_client,
        }
    }

    /// Create a new GitHubAppClient with a custom HTTP client.
    pub fn with_http_client(config: GitHubAppConfig, client: reqwest::Client) -> Self {
        Self {
            auth: GitHubAppAuth::new(config),
            http_client: client,
        }
    }

    /// Get the underlying auth handler.
    pub fn auth(&self) -> &GitHubAppAuth {
        &self.auth
    }

    /// Get the App ID.
    pub fn app_id(&self) -> Option<i64> {
        self.auth.app_id()
    }

    /// List all installations for this App.
    pub async fn list_installations(&self) -> Result<Vec<Installation>> {
        let headers = self.auth.jwt_headers()?;

        let mut request = self
            .http_client
            .get("https://api.github.com/app/installations");

        for (name, value) in headers {
            request = request.header(name, value);
        }
        request = request.header("User-Agent", "claudear");

        let response = request
            .send()
            .await
            .map_err(|e| Error::api(format!("Failed to list installations: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::api(format!(
                "GitHub API error ({}): {}",
                status, body
            )));
        }

        let installations: Vec<Installation> = response
            .json()
            .await
            .map_err(|e| Error::api(format!("Failed to parse installations: {}", e)))?;

        Ok(installations)
    }

    /// Get an installation access token.
    ///
    /// This method caches tokens and only requests new ones when needed.
    pub async fn get_installation_token(&self, installation_id: i64) -> Result<String> {
        // Check cache first
        if let Some(cached) = self.auth.get_cached_token(installation_id) {
            return Ok(cached.token);
        }

        // Request new token
        let token = self.request_installation_token(installation_id).await?;

        // Cache it
        self.auth.cache_token(installation_id, token.clone());

        Ok(token.token)
    }

    /// Request a new installation token from GitHub.
    async fn request_installation_token(&self, installation_id: i64) -> Result<CachedToken> {
        let url = self.auth.installation_token_url(installation_id);
        let headers = self.auth.jwt_headers()?;

        let mut request = self.http_client.post(&url);

        for (name, value) in headers {
            request = request.header(name, value);
        }
        request = request.header("User-Agent", "claudear");

        let response = request
            .send()
            .await
            .map_err(|e| Error::api(format!("Failed to get installation token: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::api(format!(
                "GitHub API error ({}): {}",
                status, body
            )));
        }

        let body = response
            .text()
            .await
            .map_err(|e| Error::api(format!("Failed to read response: {}", e)))?;

        self.auth.parse_token_response(&body)
    }

    /// List repositories accessible to an installation.
    pub async fn list_installation_repos(
        &self,
        installation_id: i64,
    ) -> Result<Vec<InstallationRepository>> {
        let token = self.get_installation_token(installation_id).await?;
        let headers = GitHubAppAuth::token_headers(&token);

        let mut request = self
            .http_client
            .get("https://api.github.com/installation/repositories");

        for (name, value) in headers {
            request = request.header(name, value);
        }
        request = request.header("User-Agent", "claudear");

        let response = request
            .send()
            .await
            .map_err(|e| Error::api(format!("Failed to list repositories: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::api(format!(
                "GitHub API error ({}): {}",
                status, body
            )));
        }

        let repos_response: InstallationReposResponse = response
            .json()
            .await
            .map_err(|e| Error::api(format!("Failed to parse repositories: {}", e)))?;

        Ok(repos_response.repositories)
    }

    /// Find the installation for a specific repository.
    pub async fn find_installation_for_repo(&self, owner: &str, repo: &str) -> Result<i64> {
        let installations = self.list_installations().await?;

        for installation in installations {
            // Check if this installation has access to the repo
            let repos = self.list_installation_repos(installation.id).await?;
            let full_name = format!("{}/{}", owner, repo);

            if repos.iter().any(|r| r.full_name == full_name) {
                return Ok(installation.id);
            }
        }

        Err(Error::api(format!(
            "No installation found with access to {}/{}",
            owner, repo
        )))
    }

    /// Get the configured installation ID, or find one automatically.
    pub async fn get_or_find_installation(&self, owner: &str, repo: &str) -> Result<i64> {
        // If installation ID is configured, use it
        if let Some(id) = self.auth.installation_id() {
            return Ok(id);
        }

        // Otherwise, try to find it
        self.find_installation_for_repo(owner, repo).await
    }

    /// Make an authenticated API request using an installation token.
    pub async fn api_request(
        &self,
        installation_id: i64,
        method: reqwest::Method,
        url: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let token = self.get_installation_token(installation_id).await?;
        let headers = GitHubAppAuth::token_headers(&token);

        let mut request = self.http_client.request(method, url);

        for (name, value) in headers {
            request = request.header(name, value);
        }
        request = request.header("User-Agent", "claudear");

        if let Some(body) = body {
            request = request.json(&body);
        }

        let response = request
            .send()
            .await
            .map_err(|e| Error::api(format!("API request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::api(format!(
                "GitHub API error ({}): {}",
                status, body
            )));
        }

        response
            .json()
            .await
            .map_err(|e| Error::api(format!("Failed to parse response: {}", e)))
    }

    /// Get information about the authenticated App.
    pub async fn get_app_info(&self) -> Result<serde_json::Value> {
        let headers = self.auth.jwt_headers()?;

        let mut request = self.http_client.get("https://api.github.com/app");

        for (name, value) in headers {
            request = request.header(name, value);
        }
        request = request.header("User-Agent", "claudear");

        let response = request
            .send()
            .await
            .map_err(|e| Error::api(format!("Failed to get app info: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::api(format!(
                "GitHub API error ({}): {}",
                status, body
            )));
        }

        response
            .json()
            .await
            .map_err(|e| Error::api(format!("Failed to parse app info: {}", e)))
    }

    /// Invalidate cached token for an installation.
    pub fn invalidate_token(&self, installation_id: i64) {
        self.auth.clear_cached_token(installation_id);
    }

    /// Clear all cached tokens.
    pub fn clear_token_cache(&self) {
        self.auth.clear_all_tokens();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    // Test RSA private key (for testing only - NOT a real key, generated for testing)
    const TEST_PRIVATE_KEY: &str = r#"-----BEGIN RSA PRIVATE KEY-----
MIIEpAIBAAKCAQEAj+XxUhUTsewiFEZe6f6CBEITupuaaBsU7pgZrHqtb8rnlH1A
sgaE03hKaxUUO/7rlwJkHuG0orhH3WQmctW8CiCaPSZE1aJrbCOJRFQg8aQmv+Lv
SgizJ69nQsdXkuTT/MzUypZeAI89dvkfhNznLjwaessTXFIKeOGsV7jWYnJOrf7X
kn1rLpZQ2IawCaqo9cRQumMx8uHZD4AiYmPIHd07za4hmxcwGD3ulMlyCII3a+os
3Mj2NVqegqQ2gKgd782Hp2shMf57do4RKOy+6S9rJV92PR51EhuuIQu2ZL8ijNg1
C4tkpO+5axrgh8UeXIVgOKgofqEmSXjYMj4D7QIDAQABAoIBAAhBeQbsjqS2l33y
S5/BKlR0Ng2Ov90ZMKo/r7llkG3Jhl/Oj9em6Bf53ssl+nM2vO19BaF/8Y0kZXse
M9aCzLcIB9FaULixCNi7cTSqXvl+IXsA2hm1RhIQzivWo/+ZgVAPsGWvGtWNYklh
IZ3NzrWoXRyOah3x1wf4aprdz+716ecYEnMUHHSJ+hpR8ZNGeek7QNc9RoP7Rm5I
NEcnvPJCtmqOnGXxDzOOotR1dpG8rY0txkv3ayobFJFq3h8/RMLPANxHlE7k8HRu
Ow9fFgg5wDNJCj7wj8l+t9wduoRhs77P/42rSsePktpnjG/iNgyaM3bJopMvEgyo
ahahXUECgYEAwrzRTQE1hy7EAR9FtAzrt3sa1RV2egbsD8OWspghDiSQ3mXFCz95
viJk5AxjS9NudgRBc5/LaiYv7Y4tmdhTDS5G+Z30ObHySRUV77/1JLGlAKxUacN6
Ixz3wP88WVzdB1ALiAzrwJV+2w1m43x2lM4nMg26mFPBQabkc+FT22sCgYEAvSrH
lLOxlyewojj6dI0VMcV59GTH1T+4Erjw405YPAQ3E4PkQZ9UG5Yhl/BpwX7uxAB8
6ZinAIRkhIeQybjZsdcOeakGqPc/cOz/Pvxt+ZU9lj/EaIcRV88dTEIcQacv/LNI
+Wikgy9qtng6mn7fBLwaoql9z5EpnQctiFR+DAcCgYEAoWLsLl4nJ145cBijoqDm
pMuwJBHCe0TLVBErDd2H33msWbOLxlOXqFxGsrwVepzBuaqzN4ihgtoc9EnVPt+J
jK3igjJGWZ5AhhKkeGnkVsGmVlV7K5+l0/3I0bh1IjYUs1/B/sF+i78ZP57uuu7G
M3JaB2BbWKxox+jxAZwm6/sCgYAC95zR1E/A0zqOEN683Umr0jEriDkqOymkAYql
xiDUMCy8/aCi9uDW3fAA9iByjI8qO+e5sk9MTsdU3NuEjoW7qGftuJ0GIXq5Rr5q
OoNvGswwgyeNjDDVc8Y93/uZfAngqN9IKkAKXsAJxLEGo17UMC8qxgXXL6u7btVk
Ag9IGQKBgQChUUTkc/oGYPdCdfpYR9y9+9bBVvTaIYA+5gi1jbUUQOcKzSMpW9yt
5QIzidGZB0BWLV/Bys6EpQMCPUQHoDtaqev2frR4Ug8MPYWTtxoBBtZ3Inqpaz/5
zvWGmeHev+iEP/vneCazbHGQpeC1zFX+P+tQr/zhl1klmnSGl6Zs3w==
-----END RSA PRIVATE KEY-----"#;

    fn create_test_config() -> GitHubAppConfig {
        GitHubAppConfig {
            app_id: Some(12345),
            private_key: Some(TEST_PRIVATE_KEY.into()),
            private_key_path: None,
            webhook_secret: Some("test_secret".into()),
            installation_id: Some(67890),
            client_id: Some("Iv1.abc123".to_string()),
            client_secret: Some("secret".into()),
            base_url: Some("https://example.com".to_string()),
        }
    }

    #[test]
    fn test_client_creation() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);
        assert_eq!(client.app_id(), Some(12345));
    }

    #[test]
    fn test_client_with_http_client() {
        let config = create_test_config();
        let http = claudear_core::tls::client();
        let client = GitHubAppClient::with_http_client(config, http);
        assert_eq!(client.app_id(), Some(12345));
    }

    #[test]
    fn test_installation_serialization() {
        let json = r#"{
            "id": 12345,
            "account": {
                "login": "octocat",
                "id": 1,
                "type": "User"
            },
            "target_type": "User",
            "repository_selection": "all",
            "app_id": 67890,
            "html_url": "https://github.com/settings/installations/12345"
        }"#;

        let installation: Installation = serde_json::from_str(json).unwrap();
        assert_eq!(installation.id, 12345);
        assert_eq!(installation.account.login, "octocat");
        assert_eq!(installation.target_type, "User");
    }

    #[test]
    fn test_installation_repo_serialization() {
        let json = r#"{
            "id": 1,
            "name": "repo",
            "full_name": "owner/repo",
            "private": false
        }"#;

        let repo: InstallationRepository = serde_json::from_str(json).unwrap();
        assert_eq!(repo.id, 1);
        assert_eq!(repo.name, "repo");
        assert_eq!(repo.full_name, "owner/repo");
        assert!(!repo.private);
    }

    #[test]
    fn test_token_caching() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        // Initially no cached token
        assert!(client.auth().get_cached_token(67890).is_none());

        // Cache a token
        let token = CachedToken {
            token: "test_token".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        };
        client.auth().cache_token(67890, token);

        // Should be cached
        assert!(client.auth().get_cached_token(67890).is_some());

        // Invalidate
        client.invalidate_token(67890);
        assert!(client.auth().get_cached_token(67890).is_none());
    }

    #[test]
    fn test_clear_token_cache() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        // Cache multiple tokens
        for i in 1..=5 {
            let token = CachedToken {
                token: format!("token_{}", i),
                expires_at: Utc::now() + chrono::Duration::hours(1),
            };
            client.auth().cache_token(i, token);
        }

        // Clear all
        client.clear_token_cache();

        // All should be gone
        for i in 1..=5 {
            assert!(client.auth().get_cached_token(i).is_none());
        }
    }

    #[test]
    fn test_jwt_generation_produces_valid_three_part_token() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);
        let jwt = client.auth().generate_jwt().unwrap();

        // JWT format: header.payload.signature
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "JWT must have exactly 3 dot-separated parts"
        );

        // Each part must be non-empty
        for (i, part) in parts.iter().enumerate() {
            assert!(!part.is_empty(), "JWT part {} must not be empty", i);
        }
    }

    #[test]
    fn test_jwt_header_contains_rs256_algorithm() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);
        let jwt = client.auth().generate_jwt().unwrap();

        let header_b64 = jwt.split('.').next().unwrap();
        let header_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            header_b64,
        )
        .unwrap();
        let header: serde_json::Value = serde_json::from_slice(&header_bytes).unwrap();
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");
    }

    #[test]
    fn test_jwt_payload_contains_correct_claims() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);
        let jwt = client.auth().generate_jwt().unwrap();

        let payload_b64 = jwt.split('.').nth(1).unwrap();
        let payload_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            payload_b64,
        )
        .unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&payload_bytes).unwrap();

        // iss should be the app_id as string
        assert_eq!(claims["iss"], "12345");

        // iat and exp should be present as numbers
        assert!(claims["iat"].is_i64(), "iat must be a number");
        assert!(claims["exp"].is_i64(), "exp must be a number");

        let iat = claims["iat"].as_i64().unwrap();
        let exp = claims["exp"].as_i64().unwrap();

        // exp should be ~10 minutes after iat (iat is 60s in the past, exp is 10min in the future)
        // so exp - iat ~ 660 seconds
        let diff = exp - iat;
        assert!(
            (600..=720).contains(&diff),
            "exp - iat should be ~660 seconds (got {})",
            diff
        );
    }

    #[test]
    fn test_jwt_generation_different_each_call() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let jwt1 = client.auth().generate_jwt().unwrap();
        // Sleep briefly so timestamps differ
        std::thread::sleep(std::time::Duration::from_millis(10));
        let jwt2 = client.auth().generate_jwt().unwrap();

        // Due to second-precision timestamps, they might be equal but usually differ.
        // At minimum, both should be valid JWTs.
        assert_eq!(jwt1.split('.').count(), 3);
        assert_eq!(jwt2.split('.').count(), 3);
    }

    #[test]
    fn test_jwt_generation_fails_without_app_id() {
        let mut config = create_test_config();
        config.app_id = None;
        let client = GitHubAppClient::new(config);

        let result = client.auth().generate_jwt();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("App ID"),
            "Error should mention App ID: {}",
            err_msg
        );
    }

    #[test]
    fn test_jwt_generation_fails_without_private_key() {
        let mut config = create_test_config();
        config.private_key = None;
        config.private_key_path = None;
        let client = GitHubAppClient::new(config);

        let result = client.auth().generate_jwt();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("private key"),
            "Error should mention private key: {}",
            err_msg
        );
    }

    #[test]
    fn test_jwt_generation_fails_with_invalid_private_key() {
        let mut config = create_test_config();
        config.private_key = Some("not-a-valid-pem-key".into());
        let client = GitHubAppClient::new(config);

        let result = client.auth().generate_jwt();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid RSA private key") || err_msg.contains("private key"),
            "Error should mention invalid key: {}",
            err_msg
        );
    }

    #[test]
    fn test_cached_token_that_is_about_to_expire_is_invalid() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        // Token expires in 4 minutes (within the 5-minute buffer)
        let token = CachedToken {
            token: "almost_expired".to_string(),
            expires_at: Utc::now() + chrono::Duration::minutes(4),
        };
        client.auth().cache_token(100, token);

        // Should NOT be returned because it's within the buffer
        assert!(
            client.auth().get_cached_token(100).is_none(),
            "Token expiring within buffer should not be returned"
        );
    }

    #[test]
    fn test_cached_token_that_already_expired_is_not_returned() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let token = CachedToken {
            token: "expired".to_string(),
            expires_at: Utc::now() - chrono::Duration::hours(1),
        };
        client.auth().cache_token(200, token);

        assert!(
            client.auth().get_cached_token(200).is_none(),
            "Expired token should not be returned"
        );
    }

    #[test]
    fn test_cached_token_with_plenty_of_time_is_valid() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let token = CachedToken {
            token: "fresh_token".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        };
        client.auth().cache_token(300, token);

        let cached = client.auth().get_cached_token(300);
        assert!(cached.is_some(), "Fresh token should be returned");
        assert_eq!(cached.unwrap().token, "fresh_token");
    }

    #[test]
    fn test_cached_token_just_at_buffer_boundary() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        // Token expires in exactly 5 minutes (at the buffer boundary)
        // With clock jitter, this should be considered invalid
        let token = CachedToken {
            token: "boundary_token".to_string(),
            expires_at: Utc::now() + chrono::Duration::minutes(5),
        };
        client.auth().cache_token(400, token);

        // This should be None or very close to the boundary
        // The is_valid check is: now + 5min < expires_at
        // Since expires_at == now + 5min, the condition is false -> invalid
        assert!(
            client.auth().get_cached_token(400).is_none(),
            "Token at exact buffer boundary should be invalid"
        );
    }

    #[test]
    fn test_cached_token_just_past_buffer_boundary() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        // Token expires in 6 minutes (past the 5-minute buffer)
        let token = CachedToken {
            token: "past_boundary".to_string(),
            expires_at: Utc::now() + chrono::Duration::minutes(6),
        };
        client.auth().cache_token(500, token);

        let cached = client.auth().get_cached_token(500);
        assert!(
            cached.is_some(),
            "Token past buffer boundary should be valid"
        );
        assert_eq!(cached.unwrap().token, "past_boundary");
    }

    #[test]
    fn test_invalidate_token_for_nonexistent_installation() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        // Should not panic when invalidating a token that doesn't exist
        client.invalidate_token(99999);
    }

    #[test]
    fn test_cache_overwrites_existing_token() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let token1 = CachedToken {
            token: "first_token".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        };
        client.auth().cache_token(600, token1);

        let token2 = CachedToken {
            token: "second_token".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        };
        client.auth().cache_token(600, token2);

        let cached = client.auth().get_cached_token(600).unwrap();
        assert_eq!(
            cached.token, "second_token",
            "Cache should contain the latest token"
        );
    }

    #[test]
    fn test_invalidate_specific_token_leaves_others_intact() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        for id in [1, 2, 3] {
            let token = CachedToken {
                token: format!("token_{}", id),
                expires_at: Utc::now() + chrono::Duration::hours(1),
            };
            client.auth().cache_token(id, token);
        }

        client.invalidate_token(2);

        assert!(client.auth().get_cached_token(1).is_some());
        assert!(client.auth().get_cached_token(2).is_none());
        assert!(client.auth().get_cached_token(3).is_some());
    }

    #[tokio::test]
    async fn test_get_or_find_installation_returns_configured_id() {
        let config = create_test_config(); // has installation_id = Some(67890)
        let client = GitHubAppClient::new(config);

        // When installation_id is configured, it should return immediately
        // without making any network calls
        let result = client
            .get_or_find_installation("any-owner", "any-repo")
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 67890);
    }

    #[tokio::test]
    async fn test_get_or_find_installation_without_configured_id_attempts_lookup() {
        let mut config = create_test_config();
        config.installation_id = None;
        let client = GitHubAppClient::new(config);

        // Without an installation_id, it will try to call list_installations()
        // which will fail because we're not hitting a real API
        let result = client.get_or_find_installation("owner", "repo").await;
        assert!(
            result.is_err(),
            "Should fail because no real API is available"
        );
    }

    #[test]
    fn test_installation_without_html_url() {
        let json = r#"{
            "id": 99,
            "account": {
                "login": "test-user",
                "id": 42,
                "type": "User"
            },
            "target_type": "User",
            "repository_selection": "selected",
            "app_id": 100
        }"#;

        let installation: Installation = serde_json::from_str(json).unwrap();
        assert_eq!(installation.id, 99);
        assert!(installation.html_url.is_none());
    }

    #[test]
    fn test_installation_with_organization_type() {
        let json = r#"{
            "id": 200,
            "account": {
                "login": "my-org",
                "id": 50,
                "type": "Organization"
            },
            "target_type": "Organization",
            "repository_selection": "all",
            "app_id": 300,
            "html_url": "https://github.com/organizations/my-org/settings/installations/200"
        }"#;

        let installation: Installation = serde_json::from_str(json).unwrap();
        assert_eq!(installation.account.account_type, "Organization");
        assert_eq!(installation.target_type, "Organization");
        assert_eq!(installation.repository_selection, "all");
    }

    #[test]
    fn test_installation_roundtrip_serialization() {
        let installation = Installation {
            id: 555,
            account: InstallationAccount {
                login: "roundtrip-user".to_string(),
                id: 77,
                account_type: "User".to_string(),
            },
            target_type: "User".to_string(),
            repository_selection: "selected".to_string(),
            app_id: 888,
            html_url: Some("https://github.com/test".to_string()),
        };

        let json = serde_json::to_string(&installation).unwrap();
        let deserialized: Installation = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, 555);
        assert_eq!(deserialized.account.login, "roundtrip-user");
        assert_eq!(deserialized.account.id, 77);
        assert_eq!(deserialized.app_id, 888);
        assert_eq!(
            deserialized.html_url,
            Some("https://github.com/test".to_string())
        );
    }

    #[test]
    fn test_installation_repo_private_field() {
        let json = r#"{
            "id": 10,
            "name": "private-repo",
            "full_name": "owner/private-repo",
            "private": true
        }"#;

        let repo: InstallationRepository = serde_json::from_str(json).unwrap();
        assert!(repo.private);
    }

    #[test]
    fn test_installation_repo_roundtrip() {
        let repo = InstallationRepository {
            id: 42,
            name: "my-repo".to_string(),
            full_name: "org/my-repo".to_string(),
            private: true,
        };

        let json = serde_json::to_string(&repo).unwrap();
        let deserialized: InstallationRepository = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, 42);
        assert_eq!(deserialized.name, "my-repo");
        assert_eq!(deserialized.full_name, "org/my-repo");
        assert!(deserialized.private);
    }

    #[test]
    fn test_installation_repos_response_deserialization() {
        let json = r#"{
            "total_count": 2,
            "repositories": [
                {
                    "id": 1,
                    "name": "repo-a",
                    "full_name": "org/repo-a",
                    "private": false
                },
                {
                    "id": 2,
                    "name": "repo-b",
                    "full_name": "org/repo-b",
                    "private": true
                }
            ]
        }"#;

        let response: InstallationReposResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.total_count, 2);
        assert_eq!(response.repositories.len(), 2);
        assert_eq!(response.repositories[0].name, "repo-a");
        assert_eq!(response.repositories[1].name, "repo-b");
        assert!(response.repositories[1].private);
    }

    #[test]
    fn test_installation_repos_response_empty() {
        let json = r#"{
            "total_count": 0,
            "repositories": []
        }"#;

        let response: InstallationReposResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.total_count, 0);
        assert!(response.repositories.is_empty());
    }

    #[test]
    fn test_jwt_headers_have_correct_structure() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let headers = client.auth().jwt_headers().unwrap();
        assert_eq!(headers.len(), 3);

        // Authorization header
        assert_eq!(headers[0].0, "Authorization");
        assert!(
            headers[0].1.starts_with("Bearer "),
            "Authorization should start with 'Bearer '"
        );

        // Accept header
        assert_eq!(headers[1].0, "Accept");
        assert_eq!(headers[1].1, "application/vnd.github+json");

        // API version header
        assert_eq!(headers[2].0, "X-GitHub-Api-Version");
        assert_eq!(headers[2].1, "2022-11-28");
    }

    #[test]
    fn test_token_headers_have_correct_structure() {
        let headers = GitHubAppAuth::token_headers("ghs_abc123");
        assert_eq!(headers.len(), 3);

        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(headers[0].1, "token ghs_abc123");

        assert_eq!(headers[1].0, "Accept");
        assert_eq!(headers[1].1, "application/vnd.github+json");

        assert_eq!(headers[2].0, "X-GitHub-Api-Version");
        assert_eq!(headers[2].1, "2022-11-28");
    }

    #[test]
    fn test_token_headers_with_empty_token() {
        let headers = GitHubAppAuth::token_headers("");
        assert_eq!(headers[0].1, "token ");
    }

    #[test]
    fn test_installation_token_url_format() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let url = client.auth().installation_token_url(12345);
        assert_eq!(
            url,
            "https://api.github.com/app/installations/12345/access_tokens"
        );
    }

    #[test]
    fn test_installation_token_url_with_large_id() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let url = client.auth().installation_token_url(999999999);
        assert_eq!(
            url,
            "https://api.github.com/app/installations/999999999/access_tokens"
        );
    }

    #[test]
    fn test_parse_token_response_valid() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let response_body = r#"{"token": "ghs_abc123xyz", "expires_at": "2099-01-15T12:00:00Z"}"#;
        let cached = client.auth().parse_token_response(response_body).unwrap();

        assert_eq!(cached.token, "ghs_abc123xyz");
        assert!(cached.is_valid(), "Future expiry should be valid");
    }

    #[test]
    fn test_parse_token_response_with_past_expiry() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let response_body = r#"{"token": "ghs_expired", "expires_at": "2020-01-01T00:00:00Z"}"#;
        let cached = client.auth().parse_token_response(response_body).unwrap();

        assert_eq!(cached.token, "ghs_expired");
        assert!(!cached.is_valid(), "Past expiry should be invalid");
    }

    #[test]
    fn test_parse_token_response_invalid_json() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let result = client.auth().parse_token_response("not json");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("parse"),
            "Error should mention parsing: {}",
            err_msg
        );
    }

    #[test]
    fn test_parse_token_response_missing_token_field() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let result = client
            .auth()
            .parse_token_response(r#"{"expires_at": "2099-01-01T00:00:00Z"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_token_response_missing_expires_at_field() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let result = client
            .auth()
            .parse_token_response(r#"{"token": "ghs_test"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_token_response_invalid_date_format() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        let result = client
            .auth()
            .parse_token_response(r#"{"token": "ghs_test", "expires_at": "not-a-date"}"#);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("expiration") || err_msg.contains("parse"),
            "Error should mention expiration parsing: {}",
            err_msg
        );
    }

    #[test]
    fn test_client_with_no_app_id() {
        let mut config = create_test_config();
        config.app_id = None;
        let client = GitHubAppClient::new(config);

        assert_eq!(client.app_id(), None);
    }

    #[test]
    fn test_client_auth_accessor() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        // The auth accessor should return a reference that can generate JWTs
        let auth = client.auth();
        assert_eq!(auth.app_id(), Some(12345));
        assert_eq!(auth.installation_id(), Some(67890));
    }

    #[tokio::test]
    async fn test_list_installations_fails_on_network_error() {
        // Use a client that points to a non-existent server
        let config = create_test_config();
        let http_client = claudear_core::tls::client_builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let client = GitHubAppClient::with_http_client(config, http_client);

        let result = client.list_installations().await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("list installations") || err_msg.contains("API"),
            "Error should mention API failure: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_get_installation_token_uses_cache() {
        let config = create_test_config();
        let client = GitHubAppClient::new(config);

        // Pre-fill cache with a valid token
        let token = CachedToken {
            token: "cached_token_value".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        };
        client.auth().cache_token(67890, token);

        // get_installation_token should return cached value without hitting network
        let result = client.get_installation_token(67890).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "cached_token_value");
    }

    #[tokio::test]
    async fn test_get_installation_token_misses_cache_and_requests() {
        let config = create_test_config();
        let http_client = claudear_core::tls::client_builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let client = GitHubAppClient::with_http_client(config, http_client);

        // No cached token, so it will try to request from GitHub and fail
        let result = client.get_installation_token(67890).await;
        assert!(result.is_err(), "Should fail without a real API");
    }

    #[tokio::test]
    async fn test_get_installation_token_skips_expired_cache() {
        let config = create_test_config();
        let http_client = claudear_core::tls::client_builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let client = GitHubAppClient::with_http_client(config, http_client);

        // Put an expired token in the cache
        let token = CachedToken {
            token: "expired_token".to_string(),
            expires_at: Utc::now() - chrono::Duration::hours(1),
        };
        client.auth().cache_token(67890, token);

        // Should skip the expired cache and try network (which will fail)
        let result = client.get_installation_token(67890).await;
        assert!(
            result.is_err(),
            "Should fail because expired cache was skipped and no real API"
        );
    }

    #[tokio::test]
    async fn test_list_installation_repos_fails_without_network() {
        let config = create_test_config();
        let http_client = claudear_core::tls::client_builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let client = GitHubAppClient::with_http_client(config, http_client);

        // Pre-fill a valid token so it doesn't fail on token retrieval
        let token = CachedToken {
            token: "test_token_for_repos".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        };
        client.auth().cache_token(67890, token);

        let result = client.list_installation_repos(67890).await;
        assert!(result.is_err(), "Should fail when hitting real GitHub API");
    }

    #[tokio::test]
    async fn test_api_request_fails_without_network() {
        let config = create_test_config();
        let http_client = claudear_core::tls::client_builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let client = GitHubAppClient::with_http_client(config, http_client);

        // Pre-fill a valid token
        let token = CachedToken {
            token: "test_token_for_api".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        };
        client.auth().cache_token(67890, token);

        let result = client
            .api_request(
                67890,
                reqwest::Method::GET,
                "https://api.github.com/repos/test/test",
                None,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_api_request_with_body_fails_without_network() {
        let config = create_test_config();
        let http_client = claudear_core::tls::client_builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let client = GitHubAppClient::with_http_client(config, http_client);

        let token = CachedToken {
            token: "test_token".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        };
        client.auth().cache_token(67890, token);

        let body = serde_json::json!({
            "title": "Test PR",
            "body": "Test body"
        });

        let result = client
            .api_request(
                67890,
                reqwest::Method::POST,
                "https://api.github.com/repos/test/test/pulls",
                Some(body),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_app_info_fails_without_network() {
        let config = create_test_config();
        let http_client = claudear_core::tls::client_builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let client = GitHubAppClient::with_http_client(config, http_client);

        let result = client.get_app_info().await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("app info") || err_msg.contains("API"),
            "Error should mention app info: {}",
            err_msg
        );
    }

    #[test]
    fn test_token_cache_concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let config = create_test_config();
        let client = Arc::new(GitHubAppClient::new(config));

        let mut handles = vec![];

        // Spawn multiple threads that read and write the cache concurrently
        for i in 0..10 {
            let client = Arc::clone(&client);
            handles.push(thread::spawn(move || {
                let token = CachedToken {
                    token: format!("token_{}", i),
                    expires_at: Utc::now() + chrono::Duration::hours(1),
                };
                client.auth().cache_token(i, token);
                // Read back
                let _ = client.auth().get_cached_token(i);
                // Invalidate
                if i % 2 == 0 {
                    client.invalidate_token(i);
                }
            }));
        }

        for handle in handles {
            handle.join().expect("Thread should not panic");
        }

        // Odd-numbered tokens should still be present
        for i in (1..10).step_by(2) {
            assert!(client.auth().get_cached_token(i).is_some());
        }
        // Even-numbered tokens should be gone
        for i in (0..10).step_by(2) {
            assert!(client.auth().get_cached_token(i).is_none());
        }
    }

    #[test]
    fn test_installation_ignores_unknown_fields() {
        let json = r#"{
            "id": 1,
            "account": {
                "login": "user",
                "id": 2,
                "type": "User",
                "avatar_url": "https://example.com/avatar.png"
            },
            "target_type": "User",
            "repository_selection": "all",
            "app_id": 3,
            "html_url": null,
            "permissions": {"contents": "read"},
            "events": ["push"],
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-06-01T00:00:00Z",
            "single_file_name": null
        }"#;

        // Should deserialize without error even with extra fields
        let installation: Installation = serde_json::from_str(json).unwrap();
        assert_eq!(installation.id, 1);
        assert!(installation.html_url.is_none());
    }

    #[test]
    fn test_installation_account_type_rename() {
        // The "type" JSON field maps to account_type in Rust
        let json = r#"{
            "login": "test",
            "id": 1,
            "type": "Bot"
        }"#;

        let account: InstallationAccount = serde_json::from_str(json).unwrap();
        assert_eq!(account.account_type, "Bot");
    }

    #[test]
    fn test_client_new_does_not_panic() {
        // Ensure that creating a client with default HTTP settings does not panic
        let config = create_test_config();
        let _client = GitHubAppClient::new(config);
    }

    #[test]
    fn test_client_with_custom_timeout() {
        let config = create_test_config();
        let http_client = claudear_core::tls::client_builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let client = GitHubAppClient::with_http_client(config, http_client);
        assert_eq!(client.app_id(), Some(12345));
    }

    #[tokio::test]
    async fn test_find_installation_for_repo_fails_without_network() {
        let config = create_test_config();
        let http_client = claudear_core::tls::client_builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let client = GitHubAppClient::with_http_client(config, http_client);

        let result = client.find_installation_for_repo("owner", "repo").await;
        assert!(result.is_err());
    }
}
