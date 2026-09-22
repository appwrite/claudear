//! Discord API client for thread management.

use super::types::{
    CreateMessageParams, CreateThreadParams, DiscordChannel, DiscordMessage, DiscordThread,
};
use async_trait::async_trait;
use claudear_core::error::{Error, Result};
use claudear_core::http::HttpResponse;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// Default HTTP request timeout for Discord API calls (30 seconds).
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 30;

/// Trait for HTTP client operations to enable testing.
#[async_trait]
pub trait DiscordHttpClient: Send + Sync {
    async fn get(&self, url: &str) -> Result<HttpResponse>;
    async fn post(&self, url: &str, body: serde_json::Value) -> Result<HttpResponse>;
    async fn patch(&self, url: &str, body: serde_json::Value) -> Result<HttpResponse>;
    async fn put_empty(&self, url: &str) -> Result<HttpResponse>;
}

/// Default HTTP client using reqwest.
pub struct ReqwestDiscordClient {
    client: reqwest::Client,
}

impl ReqwestDiscordClient {
    pub fn new(bot_token: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bot {}", bot_token))
                .map_err(|_| Error::config("Invalid bot token format"))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| Error::network(format!("Failed to create HTTP client: {}", e)))?;

        Ok(Self { client })
    }
}

#[async_trait]
impl DiscordHttpClient for ReqwestDiscordClient {
    async fn get(&self, url: &str) -> Result<HttpResponse> {
        let response = self.client.get(url).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }

    async fn post(&self, url: &str, body: serde_json::Value) -> Result<HttpResponse> {
        let response = self.client.post(url).json(&body).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }

    async fn patch(&self, url: &str, body: serde_json::Value) -> Result<HttpResponse> {
        let response = self.client.patch(url).json(&body).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }

    async fn put_empty(&self, url: &str) -> Result<HttpResponse> {
        let response = self.client.put(url).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }
}

/// Discord API client for managing threads and messages.
pub struct DiscordClient<H: DiscordHttpClient = ReqwestDiscordClient> {
    http: H,
    bot_token: String,
}

impl DiscordClient<ReqwestDiscordClient> {
    /// Create a new Discord client with a bot token.
    pub fn new(bot_token: impl Into<String>) -> Result<Self> {
        let bot_token = bot_token.into();
        if bot_token.is_empty() {
            return Err(Error::config("DISCORD_BOT_TOKEN is required"));
        }

        let http = ReqwestDiscordClient::new(&bot_token)?;
        Ok(Self { http, bot_token })
    }
}

impl<H: DiscordHttpClient> DiscordClient<H> {
    /// Create a new Discord client with a custom HTTP client.
    pub fn with_http_client(bot_token: impl Into<String>, http: H) -> Result<Self> {
        let bot_token = bot_token.into();
        if bot_token.is_empty() {
            return Err(Error::config("DISCORD_BOT_TOKEN is required"));
        }
        Ok(Self { http, bot_token })
    }

    /// Get the bot token (for verification purposes).
    pub fn bot_token(&self) -> &str {
        &self.bot_token
    }

    /// Get a channel by ID.
    pub async fn get_channel(&self, channel_id: &str) -> Result<DiscordChannel> {
        let url = format!("{}/channels/{}", DISCORD_API_BASE, channel_id);
        let response = self.http.get(&url).await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to get channel ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        response.json()
    }

    /// Create a thread in a channel (without a starting message).
    pub async fn create_thread(
        &self,
        channel_id: &str,
        params: CreateThreadParams,
    ) -> Result<DiscordThread> {
        let url = format!("{}/channels/{}/threads", DISCORD_API_BASE, channel_id);
        let body = serde_json::to_value(&params).map_err(|e| {
            Error::notifier("discord", format!("Failed to serialize params: {}", e))
        })?;
        let response = self.http.post(&url, body).await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to create thread ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        response.json()
    }

    /// Create a thread from an existing message.
    pub async fn create_thread_from_message(
        &self,
        channel_id: &str,
        message_id: &str,
        params: CreateThreadParams,
    ) -> Result<DiscordThread> {
        let url = format!(
            "{}/channels/{}/messages/{}/threads",
            DISCORD_API_BASE, channel_id, message_id
        );
        let body = serde_json::to_value(&params).map_err(|e| {
            Error::notifier("discord", format!("Failed to serialize params: {}", e))
        })?;
        let response = self.http.post(&url, body).await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to create thread from message ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        response.json()
    }

    /// Get a thread by ID.
    pub async fn get_thread(&self, thread_id: &str) -> Result<DiscordThread> {
        let url = format!("{}/channels/{}", DISCORD_API_BASE, thread_id);
        let response = self.http.get(&url).await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to get thread ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        response.json()
    }

    /// Send a message to a channel or thread.
    pub async fn send_message(
        &self,
        channel_id: &str,
        params: CreateMessageParams,
    ) -> Result<DiscordMessage> {
        let url = format!("{}/channels/{}/messages", DISCORD_API_BASE, channel_id);
        let body = serde_json::to_value(&params).map_err(|e| {
            Error::notifier("discord", format!("Failed to serialize params: {}", e))
        })?;
        let response = self.http.post(&url, body).await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to send message ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        response.json()
    }

    /// Fetch a single message by ID from a channel.
    pub async fn get_message(&self, channel_id: &str, message_id: &str) -> Result<DiscordMessage> {
        let url = format!(
            "{}/channels/{}/messages/{}",
            DISCORD_API_BASE, channel_id, message_id
        );
        let response = self.http.get(&url).await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to get message {} ({}): {}",
                    message_id, response.status, response.body
                ),
            ));
        }

        response.json()
    }

    /// List recent messages from a channel.
    pub async fn list_channel_messages(
        &self,
        channel_id: &str,
        limit: usize,
    ) -> Result<Vec<DiscordMessage>> {
        let clamped_limit = limit.clamp(1, 100);
        let url = format!(
            "{}/channels/{}/messages?limit={}",
            DISCORD_API_BASE, channel_id, clamped_limit
        );
        let response = self.http.get(&url).await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to list channel messages ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        response.json()
    }

    /// List messages from a channel after a given message ID (for incremental polling).
    /// Messages are returned in ascending order (oldest first).
    pub async fn list_channel_messages_after(
        &self,
        channel_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<DiscordMessage>> {
        let clamped_limit = limit.clamp(1, 100);
        let url = format!(
            "{}/channels/{}/messages?after={}&limit={}",
            DISCORD_API_BASE, channel_id, after, clamped_limit
        );
        let response = self.http.get(&url).await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to list channel messages ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        // Discord returns messages newest-first; reverse to get chronological order
        let mut messages: Vec<DiscordMessage> = response.json()?;
        messages.reverse();
        Ok(messages)
    }

    /// List messages from a channel before a given message ID (for incremental polling during indexing).
    /// Messages are returned in ascending order (oldest first).
    pub async fn list_channel_messages_before(
        &self,
        channel_id: &str,
        before: &str,
        limit: usize,
    ) -> Result<Vec<DiscordMessage>> {
        let clamped_limit = limit.clamp(1, 100);
        let url = format!(
            "{}/channels/{}/messages?before={}&limit={}",
            DISCORD_API_BASE, channel_id, before, clamped_limit
        );
        let response = self.http.get(&url).await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to list channel messages ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        // Discord returns messages newest-first; reverse to get chronological order
        let mut messages: Vec<DiscordMessage> = response.json()?;
        messages.reverse();
        Ok(messages)
    }

    /// Add a reaction emoji to a message.
    pub async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<()> {
        let encoded_emoji = urlencoding::encode(emoji);
        let url = format!(
            "{}/channels/{}/messages/{}/reactions/{}/@me",
            DISCORD_API_BASE, channel_id, message_id, encoded_emoji
        );
        let response = self.http.put_empty(&url).await?;
        if !response.is_success() {
            tracing::warn!(
                channel_id,
                message_id,
                emoji,
                status = response.status,
                "Failed to add reaction to message"
            );
        }
        Ok(())
    }

    /// Archive a thread.
    pub async fn archive_thread(&self, thread_id: &str) -> Result<DiscordThread> {
        let url = format!("{}/channels/{}", DISCORD_API_BASE, thread_id);
        let response = self
            .http
            .patch(&url, serde_json::json!({ "archived": true }))
            .await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to archive thread ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        response.json()
    }

    /// Unarchive a thread.
    pub async fn unarchive_thread(&self, thread_id: &str) -> Result<DiscordThread> {
        let url = format!("{}/channels/{}", DISCORD_API_BASE, thread_id);
        let response = self
            .http
            .patch(&url, serde_json::json!({ "archived": false }))
            .await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to unarchive thread ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        response.json()
    }

    /// List active threads in a channel.
    pub async fn list_active_threads(
        &self,
        guild_id: &str,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<Vec<DiscordThread>> {
        let mut url = format!("{}/guilds/{}/threads/active", DISCORD_API_BASE, guild_id);

        let mut query = Vec::new();

        if let Some(before) = before {
            query.push(format!("before={}", before));
        }

        if let Some(after) = after {
            query.push(format!("after={}", after));
        }

        if !query.is_empty() {
            url.push('?');
            url.push_str(&query.join("&"));
        }

        let response = self.http.get(&url).await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to list threads ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        #[derive(serde::Deserialize)]
        struct ThreadsResponse {
            threads: Vec<DiscordThread>,
        }

        let threads_response: ThreadsResponse = response.json()?;
        Ok(threads_response.threads)
    }

    pub async fn list_guild_channels(&self, guild_id: &str) -> Result<Vec<DiscordChannel>> {
        let url = format!("{}/guilds/{}/channels", DISCORD_API_BASE, guild_id);
        let response = self.http.get(&url).await?;

        if !response.is_success() {
            return Err(Error::notifier(
                "discord",
                format!(
                    "Failed to list channels ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        // TODO: store these in the db as well so that we dont have to fetch everything
        let channels_response: Vec<DiscordChannel> = response.json()?;
        Ok(channels_response)
    }

    /// List **all** public archived threads under a channel (for knowledge
    /// indexing of past conversations), following Discord's `has_more` pagination
    /// to completion. Mirrors `list_active_threads`'s `{ "threads": [...] }` shape
    /// plus a `has_more` flag; pages with `before` = the last thread's
    /// `archive_timestamp`. `before` seeds the first page; `limit` is the page size.
    pub async fn list_public_archived_threads(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<DiscordThread>> {
        #[derive(serde::Deserialize)]
        struct ThreadsResponse {
            threads: Vec<DiscordThread>,
            #[serde(default)]
            has_more: bool,
        }

        let mut all = Vec::new();
        let mut before = before.map(|s| s.to_string());

        loop {
            let mut url = format!(
                "{}/channels/{}/threads/archived/public",
                DISCORD_API_BASE, channel_id
            );
            let mut query = Vec::new();
            if let Some(before) = &before {
                // `before` is an ISO8601 archive_timestamp that can contain a `+`
                // offset (e.g. `+00:00`); URL-encode it so `+` isn't decoded as a
                // space and the cursor stays valid across pages.
                query.push(format!("before={}", urlencoding::encode(before)));
            }
            if let Some(limit) = limit {
                query.push(format!("limit={}", limit.clamp(1, 100)));
            }
            if !query.is_empty() {
                url.push('?');
                url.push_str(&query.join("&"));
            }

            let response = self.http.get(&url).await?;
            if !response.is_success() {
                return Err(Error::notifier(
                    "discord",
                    format!(
                        "Failed to list archived threads ({}): {}",
                        response.status, response.body
                    ),
                ));
            }

            let page: ThreadsResponse = response.json()?;
            // Next page starts before the oldest thread's archive timestamp.
            let next_before = page.threads.last().and_then(|t| {
                t.archive_timestamp()
                    .map(|s| s.to_string())
                    .or_else(|| t.thread_metadata.as_ref().map(|_| String::new()))
            });
            let empty = page.threads.is_empty();
            all.extend(page.threads);

            // Stop when the API says there's no more, the page was empty, or we
            // have no cursor to advance with (avoids an infinite loop).
            match next_before {
                Some(ts) if page.has_more && !empty && !ts.is_empty() => before = Some(ts),
                _ => break,
            }
        }

        Ok(all)
    }
}

/// Mock HTTP client for testing (only available in tests).
#[cfg(test)]
pub mod mock {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Mock HTTP client for testing.
    pub struct MockDiscordClient {
        get_responses: Mutex<HashMap<String, HttpResponse>>,
        post_responses: Mutex<HashMap<String, HttpResponse>>,
        patch_responses: Mutex<HashMap<String, HttpResponse>>,
    }

    impl MockDiscordClient {
        pub fn new() -> Self {
            Self {
                get_responses: Mutex::new(HashMap::new()),
                post_responses: Mutex::new(HashMap::new()),
                patch_responses: Mutex::new(HashMap::new()),
            }
        }

        pub fn mock_get(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
            self.get_responses.lock().unwrap().insert(
                url.into(),
                HttpResponse {
                    status,
                    body: body.into(),
                },
            );
        }

        pub fn mock_post(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
            self.post_responses.lock().unwrap().insert(
                url.into(),
                HttpResponse {
                    status,
                    body: body.into(),
                },
            );
        }

        pub fn mock_patch(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
            self.patch_responses.lock().unwrap().insert(
                url.into(),
                HttpResponse {
                    status,
                    body: body.into(),
                },
            );
        }
    }

    #[async_trait]
    impl DiscordHttpClient for MockDiscordClient {
        async fn get(&self, url: &str) -> Result<HttpResponse> {
            let responses = self.get_responses.lock().unwrap();
            if let Some(r) = responses.get(url) {
                Ok(HttpResponse {
                    status: r.status,
                    body: r.body.clone(),
                })
            } else {
                Ok(HttpResponse {
                    status: 404,
                    body: "Not found".to_string(),
                })
            }
        }

        async fn post(&self, url: &str, _body: serde_json::Value) -> Result<HttpResponse> {
            let responses = self.post_responses.lock().unwrap();
            if let Some(r) = responses.get(url) {
                Ok(HttpResponse {
                    status: r.status,
                    body: r.body.clone(),
                })
            } else {
                Ok(HttpResponse {
                    status: 404,
                    body: "Not found".to_string(),
                })
            }
        }

        async fn patch(&self, url: &str, _body: serde_json::Value) -> Result<HttpResponse> {
            let responses = self.patch_responses.lock().unwrap();
            if let Some(r) = responses.get(url) {
                Ok(HttpResponse {
                    status: r.status,
                    body: r.body.clone(),
                })
            } else {
                Ok(HttpResponse {
                    status: 404,
                    body: "Not found".to_string(),
                })
            }
        }

        async fn put_empty(&self, _url: &str) -> Result<HttpResponse> {
            Ok(HttpResponse {
                status: 204,
                body: String::new(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockDiscordClient;
    use super::*;

    fn mock_channel_json() -> &'static str {
        r#"{"id": "123", "type": 0, "name": "test-channel"}"#
    }

    fn mock_thread_json() -> &'static str {
        r#"{"id": "456", "type": 11, "name": "test-thread", "parent_id": "123", "owner_id": "789"}"#
    }

    fn mock_message_json() -> &'static str {
        r#"{"id": "999", "channel_id": "123", "content": "Hello", "timestamp": "2024-01-01T00:00:00Z", "author": {"id": "111", "username": "bot"}}"#
    }

    #[test]
    fn test_http_response_is_success() {
        assert!(HttpResponse {
            status: 200,
            body: "".to_string()
        }
        .is_success());
        assert!(HttpResponse {
            status: 201,
            body: "".to_string()
        }
        .is_success());
        assert!(!HttpResponse {
            status: 400,
            body: "".to_string()
        }
        .is_success());
        assert!(!HttpResponse {
            status: 500,
            body: "".to_string()
        }
        .is_success());
    }

    #[test]
    fn test_http_response_json() {
        let response = HttpResponse {
            status: 200,
            body: r#"{"id": "123"}"#.to_string(),
        };
        let parsed: serde_json::Value = response.json().unwrap();
        assert_eq!(parsed["id"], "123");
    }

    #[test]
    fn test_http_response_json_error() {
        let response = HttpResponse {
            status: 200,
            body: "invalid".to_string(),
        };
        let result: Result<serde_json::Value> = response.json();
        assert!(result.is_err());
    }

    #[test]
    fn test_client_requires_token() {
        let result = DiscordClient::new("");
        assert!(result.is_err());
    }

    #[test]
    fn test_client_creation() {
        let result = DiscordClient::new("test_token");
        assert!(result.is_ok());
        let client = result.unwrap();
        assert_eq!(client.bot_token(), "test_token");
    }

    #[test]
    fn test_with_http_client_requires_token() {
        let mock = MockDiscordClient::new();
        let result = DiscordClient::with_http_client("", mock);
        assert!(result.is_err());
    }

    #[test]
    fn test_with_http_client_success() {
        let mock = MockDiscordClient::new();
        let result = DiscordClient::with_http_client("token", mock);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_channel_success() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/channels/123",
            200,
            mock_channel_json(),
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let channel = client.get_channel("123").await.unwrap();
        assert_eq!(channel.id, "123");
    }

    #[tokio::test]
    async fn test_get_channel_error() {
        let mock = MockDiscordClient::new();
        mock.mock_get("https://discord.com/api/v10/channels/123", 404, "Not found");

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let result = client.get_channel("123").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to get channel"));
    }

    #[tokio::test]
    async fn test_create_thread_success() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/123/threads",
            200,
            mock_thread_json(),
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let params = CreateThreadParams::public("Test Thread");
        let thread = client.create_thread("123", params).await.unwrap();
        assert_eq!(thread.id, "456");
    }

    #[tokio::test]
    async fn test_create_thread_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/123/threads",
            403,
            "Forbidden",
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let params = CreateThreadParams::public("Test");
        let result = client.create_thread("123", params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_thread_from_message_success() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/123/messages/999/threads",
            200,
            mock_thread_json(),
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let params = CreateThreadParams::public("Test");
        let thread = client
            .create_thread_from_message("123", "999", params)
            .await
            .unwrap();
        assert_eq!(thread.id, "456");
    }

    #[tokio::test]
    async fn test_create_thread_from_message_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/123/messages/999/threads",
            400,
            "Bad request",
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let params = CreateThreadParams::public("Test");
        let result = client
            .create_thread_from_message("123", "999", params)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_thread_success() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/channels/456",
            200,
            mock_thread_json(),
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let thread = client.get_thread("456").await.unwrap();
        assert_eq!(thread.id, "456");
    }

    #[tokio::test]
    async fn test_get_thread_error() {
        let mock = MockDiscordClient::new();
        mock.mock_get("https://discord.com/api/v10/channels/456", 404, "Not found");

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let result = client.get_thread("456").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_message_success() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/123/messages",
            200,
            mock_message_json(),
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let params = CreateMessageParams::text("Hello");
        let message = client.send_message("123", params).await.unwrap();
        assert_eq!(message.id, "999");
    }

    #[tokio::test]
    async fn test_send_message_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/123/messages",
            403,
            "Forbidden",
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let params = CreateMessageParams::text("Hello");
        let result = client.send_message("123", params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_channel_messages_success() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/messages?limit=10",
            200,
            format!("[{}]", mock_message_json()),
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let messages = client.list_channel_messages("123", 10).await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "999");
    }

    #[tokio::test]
    async fn test_list_channel_messages_after_success() {
        let mock = MockDiscordClient::new();
        let msg1 = r#"{"id": "1001", "channel_id": "123", "content": "First", "timestamp": "2024-01-01T00:01:00Z", "author": {"id": "222", "username": "user1"}}"#;
        let msg2 = r#"{"id": "1002", "channel_id": "123", "content": "Second", "timestamp": "2024-01-01T00:02:00Z", "author": {"id": "222", "username": "user1"}}"#;
        // Discord API returns newest first
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/messages?after=1000&limit=50",
            200,
            format!("[{}, {}]", msg2, msg1),
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let messages = client
            .list_channel_messages_after("123", "1000", 50)
            .await
            .unwrap();
        assert_eq!(messages.len(), 2);
        // Should be reversed to chronological order
        assert_eq!(messages[0].id, "1001");
        assert_eq!(messages[1].id, "1002");
    }

    #[tokio::test]
    async fn test_list_channel_messages_after_error() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/messages?after=1000&limit=50",
            403,
            "Forbidden",
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let result = client.list_channel_messages_after("123", "1000", 50).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_archive_thread_success() {
        let mock = MockDiscordClient::new();
        mock.mock_patch(
            "https://discord.com/api/v10/channels/456",
            200,
            mock_thread_json(),
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let thread = client.archive_thread("456").await.unwrap();
        assert_eq!(thread.id, "456");
    }

    #[tokio::test]
    async fn test_archive_thread_error() {
        let mock = MockDiscordClient::new();
        mock.mock_patch("https://discord.com/api/v10/channels/456", 403, "Forbidden");

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let result = client.archive_thread("456").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_unarchive_thread_success() {
        let mock = MockDiscordClient::new();
        mock.mock_patch(
            "https://discord.com/api/v10/channels/456",
            200,
            mock_thread_json(),
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let thread = client.unarchive_thread("456").await.unwrap();
        assert_eq!(thread.id, "456");
    }

    #[tokio::test]
    async fn test_unarchive_thread_error() {
        let mock = MockDiscordClient::new();
        mock.mock_patch(
            "https://discord.com/api/v10/channels/456",
            500,
            "Server error",
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let result = client.unarchive_thread("456").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_active_threads_success() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/guilds/guild1/threads/active",
            200,
            r#"{"threads": [{"id": "456", "type": 11, "name": "thread1", "parent_id": "123", "owner_id": "789"}]}"#,
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let threads = client
            .list_active_threads("guild1", None, None)
            .await
            .unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "456");
    }

    #[tokio::test]
    async fn test_list_active_threads_error() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/guilds/guild1/threads/active",
            403,
            "Forbidden",
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let result = client.list_active_threads("guild1", None, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_active_threads_empty() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/guilds/guild1/threads/active",
            200,
            r#"{"threads": []}"#,
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let threads = client
            .list_active_threads("guild1", None, None)
            .await
            .unwrap();
        assert!(threads.is_empty());
    }

    #[tokio::test]
    async fn test_list_public_archived_threads_success() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/threads/archived/public",
            200,
            r#"{"threads": [{"id": "456", "type": 11, "name": "old-thread", "parent_id": "123", "owner_id": "789", "archived": true}]}"#,
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let threads = client
            .list_public_archived_threads("123", None, None)
            .await
            .unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "456");
        assert!(threads[0].archived);
    }

    #[tokio::test]
    async fn test_list_public_archived_threads_paged_query() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/threads/archived/public?before=2024-01-01T00%3A00%3A00Z&limit=50",
            200,
            r#"{"threads": []}"#,
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let threads = client
            .list_public_archived_threads("123", Some("2024-01-01T00:00:00Z"), Some(50))
            .await
            .unwrap();
        assert!(threads.is_empty());
    }

    #[tokio::test]
    async fn test_list_public_archived_threads_follows_has_more() {
        let mock = MockDiscordClient::new();
        // Page 1: has_more=true, oldest thread carries an archive_timestamp cursor.
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/threads/archived/public?limit=2",
            200,
            r#"{"has_more": true, "threads": [
                {"id": "1", "type": 11, "name": "t1", "parent_id": "123", "thread_metadata": {"archive_timestamp": "2024-06-02T00:00:00Z"}},
                {"id": "2", "type": 11, "name": "t2", "parent_id": "123", "thread_metadata": {"archive_timestamp": "2024-06-01T00:00:00Z"}}
            ]}"#,
        );
        // Page 2: fetched with before = page-1 oldest archive_timestamp; has_more=false.
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/threads/archived/public?before=2024-06-01T00%3A00%3A00Z&limit=2",
            200,
            r#"{"has_more": false, "threads": [
                {"id": "3", "type": 11, "name": "t3", "parent_id": "123", "thread_metadata": {"archive_timestamp": "2024-05-30T00:00:00Z"}}
            ]}"#,
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let threads = client
            .list_public_archived_threads("123", None, Some(2))
            .await
            .unwrap();
        let ids: Vec<&str> = threads.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["1", "2", "3"],
            "should follow has_more across pages"
        );
    }

    #[tokio::test]
    async fn test_list_public_archived_threads_url_encodes_offset_cursor() {
        let mock = MockDiscordClient::new();
        // Page 1's oldest thread carries a +00:00 offset timestamp.
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/threads/archived/public?limit=1",
            200,
            r#"{"has_more": true, "threads": [
                {"id": "1", "type": 11, "name": "t1", "parent_id": "123", "thread_metadata": {"archive_timestamp": "2024-06-01T00:00:00+00:00"}}
            ]}"#,
        );
        // Page 2 must be requested with the cursor URL-encoded (`+` -> %2B, `:` -> %3A),
        // otherwise the API would misparse it. The mock only matches the encoded form.
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/threads/archived/public?before=2024-06-01T00%3A00%3A00%2B00%3A00&limit=1",
            200,
            r#"{"has_more": false, "threads": [
                {"id": "2", "type": 11, "name": "t2", "parent_id": "123", "thread_metadata": {"archive_timestamp": "2024-05-30T00:00:00+00:00"}}
            ]}"#,
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let threads = client
            .list_public_archived_threads("123", None, Some(1))
            .await
            .unwrap();
        let ids: Vec<&str> = threads.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["1", "2"],
            "offset cursor must be URL-encoded so page 2 resolves"
        );
    }

    #[tokio::test]
    async fn test_list_public_archived_threads_error() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/threads/archived/public",
            403,
            "Forbidden",
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let result = client.list_public_archived_threads("123", None, None).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_client_with_string() {
        let token = String::from("my_token");
        let client = DiscordClient::new(token).unwrap();
        assert_eq!(client.bot_token(), "my_token");
    }

    #[test]
    fn test_discord_api_base_url() {
        assert_eq!(DISCORD_API_BASE, "https://discord.com/api/v10");
    }

    #[test]
    fn test_create_thread_params_public() {
        let params = CreateThreadParams::public("Test Thread");
        assert_eq!(params.name, "Test Thread");
        assert_eq!(params.thread_type, Some(11));
        assert_eq!(params.auto_archive_duration, Some(10080));
    }

    #[test]
    fn test_create_thread_params_private() {
        let params = CreateThreadParams::private("Private Thread");
        assert_eq!(params.name, "Private Thread");
        assert_eq!(params.thread_type, Some(12));
    }

    #[test]
    fn test_create_message_params_text() {
        let params = CreateMessageParams::text("Hello");
        assert_eq!(params.content, "Hello".to_string());
        assert!(params.embeds.is_none());
    }

    #[tokio::test]
    async fn test_list_channel_messages_before_success() {
        let mock = MockDiscordClient::new();
        let msg1 = r#"{"id": "1001", "channel_id": "123", "content": "First", "timestamp": "2024-01-01T00:01:00Z", "author": {"id": "222", "username": "user1"}}"#;
        let msg2 = r#"{"id": "1002", "channel_id": "123", "content": "Second", "timestamp": "2024-01-01T00:02:00Z", "author": {"id": "222", "username": "user1"}}"#;
        // Discord API returns newest first
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/messages?before=2000&limit=50",
            200,
            format!("[{}, {}]", msg2, msg1),
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let messages = client
            .list_channel_messages_before("123", "2000", 50)
            .await
            .unwrap();
        assert_eq!(messages.len(), 2);
        // Should be reversed to chronological order
        assert_eq!(messages[0].id, "1001");
        assert_eq!(messages[1].id, "1002");
    }

    #[tokio::test]
    async fn test_list_channel_messages_before_error() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/channels/123/messages?before=2000&limit=50",
            403,
            "Forbidden",
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let result = client.list_channel_messages_before("123", "2000", 50).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_guild_channels_success() {
        let mock = MockDiscordClient::new();
        let chan = r#"{"id": "100", "type": 0, "guild_id": "guild1", "name": "general", "parent_id": "900"}"#;
        let category = r#"{"id": "900", "type": 4, "guild_id": "guild1", "name": "Text Channels", "parent_id": null}"#;
        mock.mock_get(
            "https://discord.com/api/v10/guilds/guild1/channels",
            200,
            format!("[{}, {}]", chan, category),
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let channels = client.list_guild_channels("guild1").await.unwrap();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].id, "100");
        assert_eq!(channels[0].channel_type, 0);
        assert_eq!(channels[0].parent_id.as_deref(), Some("900"));
        assert_eq!(channels[1].id, "900");
        assert_eq!(channels[1].channel_type, 4);
        assert_eq!(channels[1].parent_id, None);
    }

    #[tokio::test]
    async fn test_list_guild_channels_empty() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/guilds/guild1/channels",
            200,
            "[]",
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let channels = client.list_guild_channels("guild1").await.unwrap();
        assert!(channels.is_empty());
    }

    #[tokio::test]
    async fn test_list_guild_channels_error() {
        let mock = MockDiscordClient::new();
        mock.mock_get(
            "https://discord.com/api/v10/guilds/guild1/channels",
            403,
            "Forbidden",
        );

        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let result = client.list_guild_channels("guild1").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to list channels"));
    }

    #[test]
    fn test_create_message_params_with_embed() {
        let embed = super::super::types::MessageEmbed::new()
            .title("Test Title")
            .description("Test description");
        let params = CreateMessageParams::with_embed("Content", embed);
        assert_eq!(params.content, "Content".to_string());
        assert!(params.embeds.is_some());
    }
}
