//! Thread manager for PR discussions.

use super::client::{DiscordClient, DiscordHttpClient, ReqwestDiscordClient};
use super::types::{CreateMessageParams, CreateThreadParams, MessageEmbed, ThreadState};
use claudear_core::error::{Error, Result};
use claudear_core::types::Issue;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Colors for different notification types.
mod colors {
    pub const SUCCESS: u32 = 0x2ecc71; // Green
    pub const ERROR: u32 = 0xe74c3c; // Red
    pub const INFO: u32 = 0x3498db; // Blue
    pub const WARNING: u32 = 0xf39c12; // Orange
    pub const PURPLE: u32 = 0x9b59b6; // Purple
    pub const REVIEW: u32 = 0x5865f2; // Discord blurple
}

/// Manages Discord threads for PR discussions.
pub struct ThreadManager<H: DiscordHttpClient = ReqwestDiscordClient> {
    client: DiscordClient<H>,
    channel_id: String,
    /// Map of PR URL -> Thread state
    threads: Arc<RwLock<HashMap<String, ThreadState>>>,
    /// User ID to mention
    user_id: Option<String>,
}

impl ThreadManager<ReqwestDiscordClient> {
    /// Create a new thread manager.
    pub fn new(
        bot_token: impl Into<String>,
        channel_id: impl Into<String>,
        user_id: Option<String>,
    ) -> Result<Self> {
        let client = DiscordClient::new(bot_token)?;
        Ok(Self {
            client,
            channel_id: channel_id.into(),
            threads: Arc::new(RwLock::new(HashMap::new())),
            user_id,
        })
    }
}

impl<H: DiscordHttpClient> ThreadManager<H> {
    /// Create a new thread manager with a custom Discord client.
    pub fn with_client(
        client: DiscordClient<H>,
        channel_id: impl Into<String>,
        user_id: Option<String>,
    ) -> Self {
        Self {
            client,
            channel_id: channel_id.into(),
            threads: Arc::new(RwLock::new(HashMap::new())),
            user_id,
        }
    }

    /// Get user mention string if configured.
    fn user_mention(&self) -> Option<String> {
        self.user_id.as_ref().map(|id| format!("<@{}>", id))
    }

    /// Create a thread for a new PR.
    pub async fn create_pr_thread(
        &self,
        issue: &Issue,
        pr_url: &str,
        pr_number: i64,
    ) -> Result<ThreadState> {
        // Check if thread already exists
        {
            let threads = self.threads.read().await;
            if let Some(state) = threads.get(pr_url) {
                return Ok(state.clone());
            }
        }

        // Create thread name
        let thread_name = format!("PR #{}: {} ({})", pr_number, issue.short_id, issue.source);

        // Create thread
        let thread = self
            .client
            .create_thread(&self.channel_id, CreateThreadParams::public(&thread_name))
            .await?;

        // Send initial message to thread
        let mention = self.user_mention();
        let content = match mention {
            Some(m) => format!("{} New PR created for issue {}", m, issue.short_id),
            None => format!("New PR created for issue {}", issue.short_id),
        };

        let embed = MessageEmbed::new()
            .title(format!("PR Created: {}", issue.short_id))
            .description(&issue.title)
            .url(pr_url)
            .color(colors::SUCCESS)
            .field(
                "Issue",
                format!("[{}]({})", issue.short_id, issue.url),
                true,
            )
            .field("Source", &issue.source, true)
            .field("Priority", issue.priority.to_string(), true)
            .footer("Claudear")
            .timestamp(chrono::Utc::now().to_rfc3339());

        let message = self
            .client
            .send_message(&thread.id, CreateMessageParams::with_embed(content, embed))
            .await?;

        // Create and store thread state
        let state = ThreadState {
            thread_id: thread.id.clone(),
            thread_name: thread.name,
            channel_id: self.channel_id.clone(),
            pr_url: pr_url.to_string(),
            issue_id: issue.id.clone(),
            source: issue.source.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            is_active: true,
            last_message_id: Some(message.id),
        };

        {
            let mut threads = self.threads.write().await;
            threads.insert(pr_url.to_string(), state.clone());
        }

        Ok(state)
    }

    /// Post a review notification to the PR thread.
    pub async fn notify_review_submitted(
        &self,
        pr_url: &str,
        reviewer: &str,
        review_state: &str,
        body: Option<&str>,
    ) -> Result<()> {
        let thread_id = {
            let threads = self.threads.read().await;
            threads.get(pr_url).map(|s| s.thread_id.clone())
        };

        let thread_id = match thread_id {
            Some(id) => id,
            None => return Ok(()), // No thread for this PR
        };

        let (title, color) = match review_state.to_lowercase().as_str() {
            "approved" => ("Review Approved", colors::SUCCESS),
            "changes_requested" => ("Changes Requested", colors::WARNING),
            "commented" => ("Review Comment", colors::INFO),
            "dismissed" => ("Review Dismissed", colors::PURPLE),
            _ => ("Review Submitted", colors::REVIEW),
        };

        let mention = self.user_mention();
        let content = match mention {
            Some(m) => format!("{} {} by {}", m, title, reviewer),
            None => format!("{} by {}", title, reviewer),
        };

        let mut embed = MessageEmbed::new()
            .title(title)
            .color(color)
            .field("Reviewer", reviewer, true)
            .field("State", review_state, true)
            .footer("Claudear")
            .timestamp(chrono::Utc::now().to_rfc3339());

        if let Some(review_body) = body {
            if !review_body.is_empty() {
                let truncated = if review_body.len() > 1000 {
                    format!(
                        "{}...",
                        &review_body[..review_body.floor_char_boundary(997)]
                    )
                } else {
                    review_body.to_string()
                };
                embed = embed.description(truncated);
            }
        }

        self.client
            .send_message(&thread_id, CreateMessageParams::with_embed(content, embed))
            .await?;

        Ok(())
    }

    /// Post a review comment to the PR thread.
    pub async fn notify_review_comment(
        &self,
        pr_url: &str,
        commenter: &str,
        file_path: Option<&str>,
        comment: &str,
    ) -> Result<()> {
        let thread_id = {
            let threads = self.threads.read().await;
            threads.get(pr_url).map(|s| s.thread_id.clone())
        };

        let thread_id = match thread_id {
            Some(id) => id,
            None => return Ok(()), // No thread for this PR
        };

        let truncated_comment = if comment.len() > 1000 {
            format!("{}...", &comment[..comment.floor_char_boundary(997)])
        } else {
            comment.to_string()
        };

        let mut embed = MessageEmbed::new()
            .title("Review Comment")
            .description(&truncated_comment)
            .color(colors::INFO)
            .field("By", commenter, true)
            .footer("Claudear")
            .timestamp(chrono::Utc::now().to_rfc3339());

        if let Some(path) = file_path {
            embed = embed.field("File", format!("`{}`", path), true);
        }

        let content = format!("Comment from {}", commenter);

        self.client
            .send_message(&thread_id, CreateMessageParams::with_embed(content, embed))
            .await?;

        Ok(())
    }

    /// Notify that an agent is working on review comments.
    pub async fn notify_agent_started(&self, pr_url: &str, task_description: &str) -> Result<()> {
        let thread_id = {
            let threads = self.threads.read().await;
            threads.get(pr_url).map(|s| s.thread_id.clone())
        };

        let thread_id = match thread_id {
            Some(id) => id,
            None => return Ok(()), // No thread for this PR
        };

        let mention = self.user_mention();
        let content = match mention {
            Some(m) => format!("{} Agent started working on review feedback", m),
            None => "Agent started working on review feedback".to_string(),
        };

        let embed = MessageEmbed::new()
            .title("Agent Working")
            .description(task_description)
            .color(colors::INFO)
            .footer("Claudear")
            .timestamp(chrono::Utc::now().to_rfc3339());

        self.client
            .send_message(&thread_id, CreateMessageParams::with_embed(content, embed))
            .await?;

        Ok(())
    }

    /// Notify that an agent completed its work.
    pub async fn notify_agent_completed(
        &self,
        pr_url: &str,
        commit_url: Option<&str>,
    ) -> Result<()> {
        let thread_id = {
            let threads = self.threads.read().await;
            threads.get(pr_url).map(|s| s.thread_id.clone())
        };

        let thread_id = match thread_id {
            Some(id) => id,
            None => return Ok(()), // No thread for this PR
        };

        let mention = self.user_mention();
        let content = match mention {
            Some(m) => format!("{} Agent completed review feedback", m),
            None => "Agent completed review feedback".to_string(),
        };

        let mut embed = MessageEmbed::new()
            .title("Agent Completed")
            .description("Review feedback has been addressed")
            .color(colors::SUCCESS)
            .footer("Claudear")
            .timestamp(chrono::Utc::now().to_rfc3339());

        if let Some(url) = commit_url {
            embed = embed.field("Commit", format!("[View changes]({})", url), false);
        }

        self.client
            .send_message(&thread_id, CreateMessageParams::with_embed(content, embed))
            .await?;

        Ok(())
    }

    /// Notify that an agent failed.
    pub async fn notify_agent_failed(&self, pr_url: &str, error: &str) -> Result<()> {
        let thread_id = {
            let threads = self.threads.read().await;
            threads.get(pr_url).map(|s| s.thread_id.clone())
        };

        let thread_id = match thread_id {
            Some(id) => id,
            None => return Ok(()), // No thread for this PR
        };

        let mention = self.user_mention();
        let content = match mention {
            Some(m) => format!("{} Agent failed to address review feedback", m),
            None => "Agent failed to address review feedback".to_string(),
        };

        let truncated_error = if error.len() > 1000 {
            format!("{}...", &error[..error.floor_char_boundary(997)])
        } else {
            error.to_string()
        };

        let embed = MessageEmbed::new()
            .title("Agent Failed")
            .description(&truncated_error)
            .color(colors::ERROR)
            .footer("Claudear")
            .timestamp(chrono::Utc::now().to_rfc3339());

        self.client
            .send_message(&thread_id, CreateMessageParams::with_embed(content, embed))
            .await?;

        Ok(())
    }

    /// Notify that the PR was merged.
    pub async fn notify_pr_merged(&self, pr_url: &str) -> Result<()> {
        let thread_id = {
            let threads = self.threads.read().await;
            threads.get(pr_url).map(|s| s.thread_id.clone())
        };

        let thread_id = match thread_id {
            Some(id) => id,
            None => return Ok(()), // No thread for this PR
        };

        let mention = self.user_mention();
        let content = match mention {
            Some(m) => format!("{} PR merged!", m),
            None => "PR merged!".to_string(),
        };

        let embed = MessageEmbed::new()
            .title("PR Merged")
            .description("The pull request has been merged. Issue resolved.")
            .color(colors::SUCCESS)
            .footer("Claudear")
            .timestamp(chrono::Utc::now().to_rfc3339());

        self.client
            .send_message(&thread_id, CreateMessageParams::with_embed(content, embed))
            .await?;

        // Archive the thread
        self.client.archive_thread(&thread_id).await?;

        // Update thread state
        {
            let mut threads = self.threads.write().await;
            if let Some(state) = threads.get_mut(pr_url) {
                state.is_active = false;
            }
        }

        Ok(())
    }

    /// Notify that the PR was closed without merging.
    pub async fn notify_pr_closed(&self, pr_url: &str) -> Result<()> {
        let thread_id = {
            let threads = self.threads.read().await;
            threads.get(pr_url).map(|s| s.thread_id.clone())
        };

        let thread_id = match thread_id {
            Some(id) => id,
            None => return Ok(()), // No thread for this PR
        };

        let mention = self.user_mention();
        let content = match mention {
            Some(m) => format!("{} PR closed without merging", m),
            None => "PR closed without merging".to_string(),
        };

        let embed = MessageEmbed::new()
            .title("PR Closed")
            .description("The pull request was closed without merging.")
            .color(colors::WARNING)
            .footer("Claudear")
            .timestamp(chrono::Utc::now().to_rfc3339());

        self.client
            .send_message(&thread_id, CreateMessageParams::with_embed(content, embed))
            .await?;

        // Archive the thread
        self.client.archive_thread(&thread_id).await?;

        // Update thread state
        {
            let mut threads = self.threads.write().await;
            if let Some(state) = threads.get_mut(pr_url) {
                state.is_active = false;
            }
        }

        Ok(())
    }

    /// Send a custom message to a PR thread.
    pub async fn send_to_thread(&self, pr_url: &str, message: &str) -> Result<()> {
        let thread_id = {
            let threads = self.threads.read().await;
            threads.get(pr_url).map(|s| s.thread_id.clone())
        };

        let thread_id = match thread_id {
            Some(id) => id,
            None => {
                return Err(Error::notifier(
                    "discord",
                    format!("No thread found for PR: {}", pr_url),
                ))
            }
        };

        self.client
            .send_message(&thread_id, CreateMessageParams::text(message))
            .await?;

        Ok(())
    }

    /// Get thread state for a PR.
    pub async fn get_thread_state(&self, pr_url: &str) -> Option<ThreadState> {
        let threads = self.threads.read().await;
        threads.get(pr_url).cloned()
    }

    /// Load thread states from storage.
    pub async fn load_threads(&self, states: Vec<ThreadState>) {
        let mut threads = self.threads.write().await;
        for state in states {
            if state.is_active {
                threads.insert(state.pr_url.clone(), state);
            }
        }
    }

    /// Get all active thread states.
    pub async fn get_active_threads(&self) -> Vec<ThreadState> {
        let threads = self.threads.read().await;
        threads.values().filter(|s| s.is_active).cloned().collect()
    }

    /// Get all thread states (for persistence).
    pub async fn get_all_threads(&self) -> Vec<ThreadState> {
        let threads = self.threads.read().await;
        threads.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::client::mock::MockDiscordClient;
    use claudear_core::types::{IssuePriority, IssueStatus};

    fn create_test_issue() -> Issue {
        Issue {
            id: "issue-123".to_string(),
            short_id: "TEST-123".to_string(),
            title: "Fix the bug".to_string(),
            description: Some("Bug description".to_string()),
            url: "https://linear.app/test/issue/123".to_string(),
            source: "linear".to_string(),
            priority: IssuePriority::Medium,
            status: IssueStatus::Open,
            metadata: Default::default(),
            created_at: None,
            updated_at: None,
        }
    }

    fn mock_thread_json() -> &'static str {
        r#"{"id": "thread-456", "type": 11, "name": "test-thread", "parent_id": "channel123", "owner_id": "bot123"}"#
    }

    fn mock_message_json() -> &'static str {
        r#"{"id": "msg-789", "channel_id": "thread-456", "content": "Hello", "timestamp": "2024-01-01T00:00:00Z", "author": {"id": "bot123", "username": "bot"}}"#
    }

    fn create_manager_with_mock(mock: MockDiscordClient) -> ThreadManager<MockDiscordClient> {
        let client = DiscordClient::with_http_client("token", mock).unwrap();
        ThreadManager::with_client(client, "channel123", None)
    }

    fn create_manager_with_mock_and_user(
        mock: MockDiscordClient,
    ) -> ThreadManager<MockDiscordClient> {
        let client = DiscordClient::with_http_client("token", mock).unwrap();
        ThreadManager::with_client(client, "channel123", Some("user456".to_string()))
    }

    #[test]
    fn test_thread_manager_requires_token() {
        let result = ThreadManager::new("", "channel123", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_thread_manager_creation() {
        let result = ThreadManager::new("test_token", "channel123", Some("user123".to_string()));
        assert!(result.is_ok());
    }

    #[test]
    fn test_with_client() {
        let mock = MockDiscordClient::new();
        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let manager = ThreadManager::with_client(client, "channel123", Some("user".to_string()));
        assert_eq!(manager.channel_id, "channel123");
        assert_eq!(manager.user_id, Some("user".to_string()));
    }

    #[tokio::test]
    async fn test_thread_state_storage() {
        let manager = ThreadManager::new("test_token", "channel123", None).unwrap();

        let state = ThreadState::new(
            "thread-1",
            "PR: Test",
            "channel123",
            "https://github.com/test/repo/pull/1",
            "issue-1",
            "linear",
        );

        manager.load_threads(vec![state.clone()]).await;

        let retrieved = manager
            .get_thread_state("https://github.com/test/repo/pull/1")
            .await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().thread_id, "thread-1");
    }

    #[tokio::test]
    async fn test_get_active_threads() {
        let manager = ThreadManager::new("test_token", "channel123", None).unwrap();

        let active_state = ThreadState::new(
            "thread-1",
            "PR: Active",
            "channel123",
            "https://github.com/test/repo/pull/1",
            "issue-1",
            "linear",
        );

        let mut inactive_state = ThreadState::new(
            "thread-2",
            "PR: Inactive",
            "channel123",
            "https://github.com/test/repo/pull/2",
            "issue-2",
            "linear",
        );
        inactive_state.is_active = false;

        manager
            .load_threads(vec![active_state, inactive_state])
            .await;

        let active = manager.get_active_threads().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].thread_id, "thread-1");
    }

    #[tokio::test]
    async fn test_get_all_threads() {
        let manager = ThreadManager::new("test_token", "channel123", None).unwrap();

        let state1 = ThreadState::new(
            "thread-1",
            "PR: 1",
            "channel123",
            "https://pr/1",
            "issue-1",
            "linear",
        );
        let state2 = ThreadState::new(
            "thread-2",
            "PR: 2",
            "channel123",
            "https://pr/2",
            "issue-2",
            "sentry",
        );
        let mut state3 = ThreadState::new(
            "thread-3",
            "PR: 3",
            "channel123",
            "https://pr/3",
            "issue-3",
            "github",
        );
        state3.is_active = false;

        manager.load_threads(vec![state1, state2, state3]).await;

        // get_all_threads returns all (2 active only since load_threads filters inactive)
        let all = manager.get_all_threads().await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_get_thread_state_not_found() {
        let manager = ThreadManager::new("test_token", "channel123", None).unwrap();

        let result = manager.get_thread_state("https://unknown/pr/999").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_send_to_thread_not_found() {
        let manager = ThreadManager::new("test_token", "channel123", None).unwrap();

        let result = manager
            .send_to_thread("https://unknown/pr/999", "Hello")
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No thread found"));
    }

    #[test]
    fn test_user_mention_with_id() {
        let manager = ThreadManager::new("token", "channel", Some("user123".to_string())).unwrap();
        let mention = manager.user_mention();
        assert_eq!(mention, Some("<@user123>".to_string()));
    }

    #[test]
    fn test_user_mention_without_id() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();
        let mention = manager.user_mention();
        assert!(mention.is_none());
    }

    #[test]
    fn test_colors() {
        assert_eq!(colors::SUCCESS, 0x2ecc71);
        assert_eq!(colors::ERROR, 0xe74c3c);
        assert_eq!(colors::INFO, 0x3498db);
        assert_eq!(colors::WARNING, 0xf39c12);
        assert_eq!(colors::PURPLE, 0x9b59b6);
        assert_eq!(colors::REVIEW, 0x5865f2);
    }

    #[tokio::test]
    async fn test_load_threads_only_loads_active() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();

        let mut inactive = ThreadState::new("t1", "Test", "ch", "https://pr/1", "i1", "linear");
        inactive.is_active = false;

        let active = ThreadState::new("t2", "Test 2", "ch", "https://pr/2", "i2", "sentry");

        manager.load_threads(vec![inactive, active]).await;

        // Only active should be loaded
        let threads = manager.get_all_threads().await;
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, "t2");
    }

    #[tokio::test]
    async fn test_notify_review_submitted_no_thread() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();

        // Should return Ok when no thread exists
        let result = manager
            .notify_review_submitted("https://unknown", "reviewer", "approved", None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_review_comment_no_thread() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();

        let result = manager
            .notify_review_comment("https://unknown", "commenter", None, "Comment")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_agent_started_no_thread() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();

        let result = manager
            .notify_agent_started("https://unknown", "Working on it")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_agent_completed_no_thread() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();

        let result = manager
            .notify_agent_completed("https://unknown", Some("https://commit"))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_agent_failed_no_thread() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();

        let result = manager
            .notify_agent_failed("https://unknown", "Error message")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_pr_merged_no_thread() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();

        let result = manager.notify_pr_merged("https://unknown").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_pr_closed_no_thread() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();

        let result = manager.notify_pr_closed("https://unknown").await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_test_issue() {
        let issue = create_test_issue();
        assert_eq!(issue.id, "issue-123");
        assert_eq!(issue.short_id, "TEST-123");
        assert_eq!(issue.source, "linear");
        assert_eq!(issue.priority, IssuePriority::Medium);
    }

    #[tokio::test]
    async fn test_load_empty_threads() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();
        manager.load_threads(vec![]).await;

        let threads = manager.get_all_threads().await;
        assert!(threads.is_empty());
    }

    // Mock-based tests for HTTP-dependent functionality

    #[tokio::test]
    async fn test_create_pr_thread_success() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let issue = create_test_issue();
        let result = manager
            .create_pr_thread(&issue, "https://github.com/test/pr/1", 1)
            .await;

        assert!(result.is_ok());
        let state = result.unwrap();
        assert_eq!(state.thread_id, "thread-456");
        assert_eq!(state.pr_url, "https://github.com/test/pr/1");
        assert_eq!(state.issue_id, "issue-123");
        assert!(state.is_active);
    }

    #[tokio::test]
    async fn test_create_pr_thread_with_user_mention() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock_and_user(mock);
        let issue = create_test_issue();
        let result = manager
            .create_pr_thread(&issue, "https://github.com/test/pr/1", 1)
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_create_pr_thread_returns_existing() {
        let mock = MockDiscordClient::new();
        let manager = create_manager_with_mock(mock);

        // Pre-load a thread state
        let state = ThreadState::new(
            "existing-thread",
            "PR: Existing",
            "channel123",
            "https://github.com/test/pr/1",
            "issue-123",
            "linear",
        );
        manager.load_threads(vec![state]).await;

        // Calling create_pr_thread for same PR should return existing
        let issue = create_test_issue();
        let result = manager
            .create_pr_thread(&issue, "https://github.com/test/pr/1", 1)
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().thread_id, "existing-thread");
    }

    #[tokio::test]
    async fn test_create_pr_thread_api_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            403,
            "Forbidden",
        );

        let manager = create_manager_with_mock(mock);
        let issue = create_test_issue();
        let result = manager
            .create_pr_thread(&issue, "https://github.com/test/pr/1", 1)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_review_submitted_approved() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_review_submitted("https://pr/1", "reviewer1", "approved", None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_review_submitted_changes_requested() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_review_submitted(
                "https://pr/1",
                "reviewer1",
                "changes_requested",
                Some("Please fix this"),
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_review_submitted_commented() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_review_submitted("https://pr/1", "reviewer1", "commented", Some("Looks good"))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_review_submitted_dismissed() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_review_submitted("https://pr/1", "reviewer1", "dismissed", None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_review_submitted_unknown_state() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_review_submitted("https://pr/1", "reviewer1", "unknown_state", None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_review_submitted_with_long_body() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        // Body longer than 1000 chars should be truncated
        let long_body = "x".repeat(1500);
        let result = manager
            .notify_review_submitted("https://pr/1", "reviewer1", "commented", Some(&long_body))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_review_submitted_with_user_mention() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock_and_user(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_review_submitted("https://pr/1", "reviewer1", "approved", None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_review_comment_with_file() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_review_comment(
                "https://pr/1",
                "commenter",
                Some("src/main.rs"),
                "Fix this line",
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_review_comment_without_file() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_review_comment("https://pr/1", "commenter", None, "General comment")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_review_comment_long_comment() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let long_comment = "x".repeat(1500);
        let result = manager
            .notify_review_comment("https://pr/1", "commenter", None, &long_comment)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_agent_started_with_thread() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_agent_started("https://pr/1", "Working on fixes")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_agent_started_with_user_mention() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock_and_user(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_agent_started("https://pr/1", "Working on fixes")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_agent_completed_with_commit() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_agent_completed("https://pr/1", Some("https://github.com/commit/abc123"))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_agent_completed_without_commit() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_agent_completed("https://pr/1", None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_agent_completed_with_user_mention() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock_and_user(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_agent_completed("https://pr/1", None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_agent_failed_with_thread() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_agent_failed("https://pr/1", "Error occurred")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_agent_failed_long_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let long_error = "x".repeat(1500);
        let result = manager
            .notify_agent_failed("https://pr/1", &long_error)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_agent_failed_with_user_mention() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock_and_user(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_agent_failed("https://pr/1", "Error").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_pr_merged_with_thread() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        mock.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_pr_merged("https://pr/1").await;
        assert!(result.is_ok());

        // Thread should be marked inactive
        let thread_state = manager.get_thread_state("https://pr/1").await.unwrap();
        assert!(!thread_state.is_active);
    }

    #[tokio::test]
    async fn test_notify_pr_merged_with_user_mention() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        mock.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_manager_with_mock_and_user(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_pr_merged("https://pr/1").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_pr_closed_with_thread() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        mock.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_pr_closed("https://pr/1").await;
        assert!(result.is_ok());

        // Thread should be marked inactive
        let thread_state = manager.get_thread_state("https://pr/1").await.unwrap();
        assert!(!thread_state.is_active);
    }

    #[tokio::test]
    async fn test_notify_pr_closed_with_user_mention() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        mock.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_manager_with_mock_and_user(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_pr_closed("https://pr/1").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_to_thread_success() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .send_to_thread("https://pr/1", "Hello thread!")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_to_thread_api_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            403,
            "Forbidden",
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.send_to_thread("https://pr/1", "Hello").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_review_submitted_api_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            500,
            "Internal error",
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_review_submitted("https://pr/1", "reviewer1", "approved", None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_review_comment_api_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            500,
            "Internal error",
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_review_comment("https://pr/1", "commenter", None, "Comment")
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_agent_started_api_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            500,
            "Internal error",
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_agent_started("https://pr/1", "Working")
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_agent_completed_api_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            500,
            "Internal error",
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_agent_completed("https://pr/1", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_agent_failed_api_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            500,
            "Internal error",
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_agent_failed("https://pr/1", "Error").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_pr_merged_send_message_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            500,
            "Internal error",
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_pr_merged("https://pr/1").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_pr_merged_archive_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        mock.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            500,
            "Archive failed",
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_pr_merged("https://pr/1").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_pr_closed_send_message_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            500,
            "Internal error",
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_pr_closed("https://pr/1").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_pr_closed_archive_error() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        mock.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            500,
            "Archive failed",
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_pr_closed("https://pr/1").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_notify_review_submitted_empty_body() {
        let mock = MockDiscordClient::new();
        mock.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_manager_with_mock(mock);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        // Empty body should not add description to embed
        let result = manager
            .notify_review_submitted("https://pr/1", "reviewer1", "commented", Some(""))
            .await;
        assert!(result.is_ok());
    }

    /// Shared state for the capturing mock, held via Arc so tests can
    /// inspect captured requests after passing the mock to the manager.
    struct CapturedRequests {
        post_responses: std::sync::Mutex<HashMap<String, abnegate_http::HttpResponse>>,
        patch_responses: std::sync::Mutex<HashMap<String, abnegate_http::HttpResponse>>,
        captured_posts: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
        captured_patches: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl CapturedRequests {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                post_responses: std::sync::Mutex::new(HashMap::new()),
                patch_responses: std::sync::Mutex::new(HashMap::new()),
                captured_posts: std::sync::Mutex::new(Vec::new()),
                captured_patches: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn mock_post(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
            self.post_responses
                .lock()
                .unwrap()
                .insert(url.into(), abnegate_http::HttpResponse::new(status, body));
        }

        fn mock_patch(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
            self.patch_responses
                .lock()
                .unwrap()
                .insert(url.into(), abnegate_http::HttpResponse::new(status, body));
        }

        fn get_captured_posts(&self) -> Vec<(String, serde_json::Value)> {
            self.captured_posts.lock().unwrap().clone()
        }

        fn get_captured_patches(&self) -> Vec<(String, serde_json::Value)> {
            self.captured_patches.lock().unwrap().clone()
        }
    }

    /// A mock HTTP client that captures request bodies for assertions.
    /// Uses Arc<CapturedRequests> so the test can retain a handle.
    struct CapturingMockClient {
        inner: Arc<CapturedRequests>,
    }

    impl CapturingMockClient {
        fn new(inner: Arc<CapturedRequests>) -> Self {
            Self { inner }
        }
    }

    #[async_trait::async_trait]
    impl DiscordHttpClient for CapturingMockClient {
        async fn get(
            &self,
            _url: &str,
        ) -> claudear_core::error::Result<abnegate_http::HttpResponse> {
            Ok(abnegate_http::HttpResponse::new(404, "Not found"))
        }

        async fn post(
            &self,
            url: &str,
            body: serde_json::Value,
        ) -> claudear_core::error::Result<abnegate_http::HttpResponse> {
            self.inner
                .captured_posts
                .lock()
                .unwrap()
                .push((url.to_string(), body));
            let responses = self.inner.post_responses.lock().unwrap();
            if let Some(response) = responses.get(url) {
                Ok(response.clone())
            } else {
                Ok(abnegate_http::HttpResponse::new(404, "Not found"))
            }
        }

        async fn put_empty(
            &self,
            _url: &str,
        ) -> claudear_core::error::Result<abnegate_http::HttpResponse> {
            Ok(abnegate_http::HttpResponse::new(204, ""))
        }

        async fn patch(
            &self,
            url: &str,
            body: serde_json::Value,
        ) -> claudear_core::error::Result<abnegate_http::HttpResponse> {
            self.inner
                .captured_patches
                .lock()
                .unwrap()
                .push((url.to_string(), body));
            let responses = self.inner.patch_responses.lock().unwrap();
            if let Some(response) = responses.get(url) {
                Ok(response.clone())
            } else {
                Ok(abnegate_http::HttpResponse::new(404, "Not found"))
            }
        }
    }

    fn create_capturing_manager(
        captured: &Arc<CapturedRequests>,
        user_id: Option<String>,
    ) -> ThreadManager<CapturingMockClient> {
        let mock = CapturingMockClient::new(Arc::clone(captured));
        let client = DiscordClient::with_http_client("token", mock).unwrap();
        ThreadManager::with_client(client, "channel123", user_id)
    }

    #[tokio::test]
    async fn test_create_pr_thread_name_format() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let issue = create_test_issue();
        let _result = manager
            .create_pr_thread(&issue, "https://github.com/test/pr/42", 42)
            .await
            .unwrap();

        // Verify the thread creation payload includes correct name format
        let posts = captured.get_captured_posts();
        let (url, body) = &posts[0];
        assert!(url.contains("/threads"));
        assert_eq!(body["name"].as_str().unwrap(), "PR #42: TEST-123 (linear)");
    }

    #[tokio::test]
    async fn test_create_pr_thread_initial_embed_content() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let issue = create_test_issue();
        let _result = manager
            .create_pr_thread(&issue, "https://github.com/test/pr/1", 1)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        // Second POST is the message to the thread
        let (url, body) = &posts[1];
        assert!(url.contains("/messages"));

        // Verify content
        let content = body["content"].as_str().unwrap();
        assert!(content.contains("New PR created for issue TEST-123"));
        assert!(!content.contains("<@")); // No mention without user_id

        // Verify embed
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "PR Created: TEST-123");
        assert_eq!(embed["description"].as_str().unwrap(), "Fix the bug");
        assert_eq!(
            embed["url"].as_str().unwrap(),
            "https://github.com/test/pr/1"
        );
        assert_eq!(embed["color"].as_u64().unwrap(), colors::SUCCESS as u64);

        // Verify fields
        let fields = embed["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0]["name"].as_str().unwrap(), "Issue");
        assert!(fields[0]["value"].as_str().unwrap().contains("TEST-123"));
        assert_eq!(fields[1]["name"].as_str().unwrap(), "Source");
        assert_eq!(fields[1]["value"].as_str().unwrap(), "linear");
        assert_eq!(fields[2]["name"].as_str().unwrap(), "Priority");

        // Verify footer
        assert_eq!(embed["footer"]["text"].as_str().unwrap(), "Claudear");
    }

    #[tokio::test]
    async fn test_create_pr_thread_with_user_mention_content() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, Some("user456".to_string()));
        let issue = create_test_issue();
        let _result = manager
            .create_pr_thread(&issue, "https://github.com/test/pr/1", 1)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[1];
        let content = body["content"].as_str().unwrap();
        assert!(content.starts_with("<@user456>"));
        assert!(content.contains("New PR created for issue TEST-123"));
    }

    #[tokio::test]
    async fn test_create_pr_thread_stores_state_correctly() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let issue = create_test_issue();
        let state = manager
            .create_pr_thread(&issue, "https://github.com/test/pr/1", 1)
            .await
            .unwrap();

        assert_eq!(state.thread_id, "thread-456");
        assert_eq!(state.thread_name, "test-thread");
        assert_eq!(state.channel_id, "channel123");
        assert_eq!(state.pr_url, "https://github.com/test/pr/1");
        assert_eq!(state.issue_id, "issue-123");
        assert_eq!(state.source, "linear");
        assert!(state.is_active);
        assert_eq!(state.last_message_id, Some("msg-789".to_string()));

        // Verify it's stored and retrievable
        let retrieved = manager
            .get_thread_state("https://github.com/test/pr/1")
            .await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().thread_id, "thread-456");
    }

    #[tokio::test]
    async fn test_create_pr_thread_duplicate_returns_existing_without_api_call() {
        let captured = CapturedRequests::new();
        // No mock responses needed -- should not make API calls for duplicate
        let manager = create_capturing_manager(&captured, None);

        let state = ThreadState::new(
            "existing-thread",
            "PR: Existing",
            "channel123",
            "https://github.com/test/pr/1",
            "issue-123",
            "linear",
        );
        manager.load_threads(vec![state]).await;

        let issue = create_test_issue();
        let result = manager
            .create_pr_thread(&issue, "https://github.com/test/pr/1", 1)
            .await
            .unwrap();

        assert_eq!(result.thread_id, "existing-thread");

        // Verify no API calls were made
        let posts = captured.get_captured_posts();
        assert!(posts.is_empty());
    }

    #[tokio::test]
    async fn test_create_pr_thread_message_send_failure_propagates() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            500,
            "Internal Server Error",
        );

        let manager = create_capturing_manager(&captured, None);
        let issue = create_test_issue();
        let result = manager
            .create_pr_thread(&issue, "https://github.com/test/pr/1", 1)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_review_submitted_approved_embed_color() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_review_submitted("https://pr/1", "alice", "approved", None)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "Review Approved");
        assert_eq!(embed["color"].as_u64().unwrap(), colors::SUCCESS as u64);
        assert_eq!(embed["fields"][0]["value"].as_str().unwrap(), "alice");
        assert_eq!(embed["fields"][1]["value"].as_str().unwrap(), "approved");

        let content = body["content"].as_str().unwrap();
        assert_eq!(content, "Review Approved by alice");
    }

    #[tokio::test]
    async fn test_review_submitted_changes_requested_embed_color() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_review_submitted(
                "https://pr/1",
                "bob",
                "changes_requested",
                Some("Fix the tests"),
            )
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "Changes Requested");
        assert_eq!(embed["color"].as_u64().unwrap(), colors::WARNING as u64);
        assert_eq!(embed["description"].as_str().unwrap(), "Fix the tests");
    }

    #[tokio::test]
    async fn test_review_submitted_commented_embed_color() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_review_submitted("https://pr/1", "carol", "commented", Some("LGTM"))
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "Review Comment");
        assert_eq!(embed["color"].as_u64().unwrap(), colors::INFO as u64);
    }

    #[tokio::test]
    async fn test_review_submitted_dismissed_embed_color() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_review_submitted("https://pr/1", "dave", "dismissed", None)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "Review Dismissed");
        assert_eq!(embed["color"].as_u64().unwrap(), colors::PURPLE as u64);
    }

    #[tokio::test]
    async fn test_review_submitted_unknown_state_uses_review_color() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_review_submitted("https://pr/1", "eve", "pending", None)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "Review Submitted");
        assert_eq!(embed["color"].as_u64().unwrap(), colors::REVIEW as u64);
    }

    #[tokio::test]
    async fn test_review_submitted_with_mention_includes_mention_in_content() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, Some("user456".to_string()));
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_review_submitted("https://pr/1", "alice", "approved", None)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let content = body["content"].as_str().unwrap();
        assert!(content.starts_with("<@user456>"));
        assert!(content.contains("Review Approved by alice"));
    }

    #[tokio::test]
    async fn test_review_submitted_body_truncation_at_1000_chars() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let long_body = "x".repeat(1500);
        manager
            .notify_review_submitted("https://pr/1", "reviewer", "commented", Some(&long_body))
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let description = body["embeds"][0]["description"].as_str().unwrap();
        assert!(description.len() <= 1003); // 997 chars + "..."
        assert!(description.ends_with("..."));
    }

    #[tokio::test]
    async fn test_review_submitted_empty_body_no_description() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_review_submitted("https://pr/1", "reviewer", "commented", Some(""))
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        // Empty body should not produce a description field in the embed
        assert!(body["embeds"][0]["description"].is_null());
    }

    #[tokio::test]
    async fn test_review_submitted_none_body_no_description() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_review_submitted("https://pr/1", "reviewer", "approved", None)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        assert!(body["embeds"][0]["description"].is_null());
    }

    #[tokio::test]
    async fn test_review_submitted_case_insensitive_state() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        // Use uppercase -- the code lowercases before matching
        manager
            .notify_review_submitted("https://pr/1", "reviewer", "APPROVED", None)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        assert_eq!(
            body["embeds"][0]["title"].as_str().unwrap(),
            "Review Approved"
        );
        assert_eq!(
            body["embeds"][0]["color"].as_u64().unwrap(),
            colors::SUCCESS as u64
        );
    }

    #[tokio::test]
    async fn test_review_comment_with_file_path_embed() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_review_comment(
                "https://pr/1",
                "alice",
                Some("src/main.rs"),
                "Fix this line",
            )
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "Review Comment");
        assert_eq!(embed["description"].as_str().unwrap(), "Fix this line");
        assert_eq!(embed["color"].as_u64().unwrap(), colors::INFO as u64);

        let fields = embed["fields"].as_array().unwrap();
        // "By" field + "File" field
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0]["name"].as_str().unwrap(), "By");
        assert_eq!(fields[0]["value"].as_str().unwrap(), "alice");
        assert_eq!(fields[1]["name"].as_str().unwrap(), "File");
        assert_eq!(fields[1]["value"].as_str().unwrap(), "`src/main.rs`");

        let content = body["content"].as_str().unwrap();
        assert_eq!(content, "Comment from alice");
    }

    #[tokio::test]
    async fn test_review_comment_without_file_has_no_file_field() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_review_comment("https://pr/1", "bob", None, "General comment")
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let fields = body["embeds"][0]["fields"].as_array().unwrap();
        // Only "By" field, no "File" field
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0]["name"].as_str().unwrap(), "By");
    }

    #[tokio::test]
    async fn test_review_comment_truncates_long_comment() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let long_comment = "a".repeat(2000);
        manager
            .notify_review_comment("https://pr/1", "reviewer", None, &long_comment)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let description = body["embeds"][0]["description"].as_str().unwrap();
        assert!(description.len() <= 1003);
        assert!(description.ends_with("..."));
    }

    #[tokio::test]
    async fn test_agent_started_embed_content() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_agent_started("https://pr/1", "Addressing review feedback on auth module")
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "Agent Working");
        assert_eq!(
            embed["description"].as_str().unwrap(),
            "Addressing review feedback on auth module"
        );
        assert_eq!(embed["color"].as_u64().unwrap(), colors::INFO as u64);

        let content = body["content"].as_str().unwrap();
        assert_eq!(content, "Agent started working on review feedback");
    }

    #[tokio::test]
    async fn test_agent_started_with_mention() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, Some("user789".to_string()));
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_agent_started("https://pr/1", "Working on it")
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let content = body["content"].as_str().unwrap();
        assert!(content.starts_with("<@user789>"));
    }

    #[tokio::test]
    async fn test_agent_completed_with_commit_url_embed() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_agent_completed(
                "https://pr/1",
                Some("https://github.com/test/repo/commit/abc123"),
            )
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "Agent Completed");
        assert_eq!(
            embed["description"].as_str().unwrap(),
            "Review feedback has been addressed"
        );
        assert_eq!(embed["color"].as_u64().unwrap(), colors::SUCCESS as u64);

        let fields = embed["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0]["name"].as_str().unwrap(), "Commit");
        assert!(fields[0]["value"]
            .as_str()
            .unwrap()
            .contains("View changes"));
        assert!(fields[0]["value"].as_str().unwrap().contains("abc123"));
    }

    #[tokio::test]
    async fn test_agent_completed_without_commit_url_no_field() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_agent_completed("https://pr/1", None)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        // No commit field
        assert!(embed["fields"].is_null());
    }

    #[tokio::test]
    async fn test_agent_completed_with_mention() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, Some("user999".to_string()));
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_agent_completed("https://pr/1", None)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let content = body["content"].as_str().unwrap();
        assert_eq!(content, "<@user999> Agent completed review feedback");
    }

    #[tokio::test]
    async fn test_agent_failed_embed_content() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_agent_failed("https://pr/1", "Compilation error in src/main.rs")
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "Agent Failed");
        assert_eq!(
            embed["description"].as_str().unwrap(),
            "Compilation error in src/main.rs"
        );
        assert_eq!(embed["color"].as_u64().unwrap(), colors::ERROR as u64);

        let content = body["content"].as_str().unwrap();
        assert_eq!(content, "Agent failed to address review feedback");
    }

    #[tokio::test]
    async fn test_agent_failed_truncates_long_error() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let long_error = "E".repeat(2000);
        manager
            .notify_agent_failed("https://pr/1", &long_error)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let description = body["embeds"][0]["description"].as_str().unwrap();
        assert!(description.len() <= 1003);
        assert!(description.ends_with("..."));
    }

    #[tokio::test]
    async fn test_agent_failed_with_mention() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, Some("user111".to_string()));
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .notify_agent_failed("https://pr/1", "Oops")
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let content = body["content"].as_str().unwrap();
        assert!(content.starts_with("<@user111>"));
        assert!(content.contains("Agent failed"));
    }

    #[tokio::test]
    async fn test_pr_merged_embed_content() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager.notify_pr_merged("https://pr/1").await.unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "PR Merged");
        assert_eq!(
            embed["description"].as_str().unwrap(),
            "The pull request has been merged. Issue resolved."
        );
        assert_eq!(embed["color"].as_u64().unwrap(), colors::SUCCESS as u64);

        let content = body["content"].as_str().unwrap();
        assert_eq!(content, "PR merged!");
    }

    #[tokio::test]
    async fn test_pr_merged_archives_thread() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager.notify_pr_merged("https://pr/1").await.unwrap();

        // Verify archive PATCH was sent
        let patches = captured.get_captured_patches();
        assert_eq!(patches.len(), 1);
        let (url, body) = &patches[0];
        assert!(url.contains("thread-1"));
        assert_eq!(body["archived"], true);
    }

    #[tokio::test]
    async fn test_pr_merged_marks_thread_inactive() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        // Thread should be active before merge
        let before = manager.get_thread_state("https://pr/1").await.unwrap();
        assert!(before.is_active);

        manager.notify_pr_merged("https://pr/1").await.unwrap();

        // Thread should be inactive after merge
        let after = manager.get_thread_state("https://pr/1").await.unwrap();
        assert!(!after.is_active);
    }

    #[tokio::test]
    async fn test_pr_merged_with_mention() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, Some("user456".to_string()));
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager.notify_pr_merged("https://pr/1").await.unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let content = body["content"].as_str().unwrap();
        assert_eq!(content, "<@user456> PR merged!");
    }

    #[tokio::test]
    async fn test_pr_closed_embed_content() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager.notify_pr_closed("https://pr/1").await.unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"].as_str().unwrap(), "PR Closed");
        assert_eq!(
            embed["description"].as_str().unwrap(),
            "The pull request was closed without merging."
        );
        assert_eq!(embed["color"].as_u64().unwrap(), colors::WARNING as u64);

        let content = body["content"].as_str().unwrap();
        assert_eq!(content, "PR closed without merging");
    }

    #[tokio::test]
    async fn test_pr_closed_archives_thread() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager.notify_pr_closed("https://pr/1").await.unwrap();

        let patches = captured.get_captured_patches();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].1["archived"], true);
    }

    #[tokio::test]
    async fn test_pr_closed_marks_thread_inactive() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager.notify_pr_closed("https://pr/1").await.unwrap();

        let after = manager.get_thread_state("https://pr/1").await.unwrap();
        assert!(!after.is_active);
    }

    #[tokio::test]
    async fn test_pr_closed_with_mention() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, Some("user123".to_string()));
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager.notify_pr_closed("https://pr/1").await.unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let content = body["content"].as_str().unwrap();
        assert_eq!(content, "<@user123> PR closed without merging");
    }

    #[tokio::test]
    async fn test_send_to_thread_sends_correct_content() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        manager
            .send_to_thread("https://pr/1", "Custom message here")
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (url, body) = &posts[0];
        assert!(url.contains("thread-1/messages"));
        assert_eq!(body["content"].as_str().unwrap(), "Custom message here");
        // Text-only message, no embeds
        assert!(body["embeds"].is_null());
    }

    #[tokio::test]
    async fn test_send_to_thread_not_found_error_message() {
        let captured = CapturedRequests::new();
        let manager = create_capturing_manager(&captured, None);

        let result = manager
            .send_to_thread("https://unknown/pr/999", "Hello")
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("No thread found"));
        assert!(err.contains("https://unknown/pr/999"));
    }

    #[test]
    fn test_user_mention_with_numeric_id() {
        let mock = MockDiscordClient::new();
        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let manager = ThreadManager::with_client(client, "channel", Some("123456789".to_string()));
        assert_eq!(manager.user_mention(), Some("<@123456789>".to_string()));
    }

    #[test]
    fn test_user_mention_with_empty_string_id() {
        let mock = MockDiscordClient::new();
        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let manager = ThreadManager::with_client(client, "channel", Some("".to_string()));
        // Even an empty string produces a mention format
        assert_eq!(manager.user_mention(), Some("<@>".to_string()));
    }

    #[test]
    fn test_user_mention_format_is_discord_compatible() {
        let mock = MockDiscordClient::new();
        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let manager = ThreadManager::with_client(client, "channel", Some("12345".to_string()));
        let mention = manager.user_mention().unwrap();
        assert!(mention.starts_with("<@"));
        assert!(mention.ends_with(">"));
        assert_eq!(mention, "<@12345>");
    }

    #[tokio::test]
    async fn test_load_threads_overwrites_existing() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();

        let state1 = ThreadState::new("t1", "PR: 1", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state1]).await;

        let state2 = ThreadState::new("t2", "PR: 1 Updated", "ch", "https://pr/1", "i2", "sentry");
        manager.load_threads(vec![state2]).await;

        let result = manager.get_thread_state("https://pr/1").await.unwrap();
        assert_eq!(result.thread_id, "t2");
        assert_eq!(result.issue_id, "i2");
    }

    #[tokio::test]
    async fn test_multiple_threads_for_different_prs() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();

        let state1 = ThreadState::new("t1", "PR: 1", "ch", "https://pr/1", "i1", "linear");
        let state2 = ThreadState::new("t2", "PR: 2", "ch", "https://pr/2", "i2", "sentry");
        let state3 = ThreadState::new("t3", "PR: 3", "ch", "https://pr/3", "i3", "github");

        manager.load_threads(vec![state1, state2, state3]).await;

        assert_eq!(
            manager
                .get_thread_state("https://pr/1")
                .await
                .unwrap()
                .thread_id,
            "t1"
        );
        assert_eq!(
            manager
                .get_thread_state("https://pr/2")
                .await
                .unwrap()
                .thread_id,
            "t2"
        );
        assert_eq!(
            manager
                .get_thread_state("https://pr/3")
                .await
                .unwrap()
                .thread_id,
            "t3"
        );
    }

    #[tokio::test]
    async fn test_get_active_threads_excludes_merged() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, None);

        let state1 = ThreadState::new("thread-1", "PR: 1", "ch", "https://pr/1", "i1", "linear");
        let state2 = ThreadState::new("thread-2", "PR: 2", "ch", "https://pr/2", "i2", "linear");
        manager.load_threads(vec![state1, state2]).await;

        // Merge PR 1
        manager.notify_pr_merged("https://pr/1").await.unwrap();

        let active = manager.get_active_threads().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].thread_id, "thread-2");
    }

    #[tokio::test]
    async fn test_get_all_threads_includes_inactive() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-1",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, None);

        let state1 = ThreadState::new("thread-1", "PR: 1", "ch", "https://pr/1", "i1", "linear");
        let state2 = ThreadState::new("thread-2", "PR: 2", "ch", "https://pr/2", "i2", "linear");
        manager.load_threads(vec![state1, state2]).await;

        manager.notify_pr_merged("https://pr/1").await.unwrap();

        // get_all_threads includes the inactive thread since it was active when loaded
        let all = manager.get_all_threads().await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_thread_state_after_create() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let issue = create_test_issue();
        let _state = manager
            .create_pr_thread(&issue, "https://pr/1", 1)
            .await
            .unwrap();

        // Thread should appear in active threads
        let active = manager.get_active_threads().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].pr_url, "https://pr/1");
    }

    #[tokio::test]
    async fn test_review_comment_with_unicode_content() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let unicode_comment = "This has emoji: \u{1F600} and CJK: \u{4F60}\u{597D} and accents: \u{00E9}\u{00E8}\u{00EA}";
        manager
            .notify_review_comment("https://pr/1", "reviewer", None, unicode_comment)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let description = body["embeds"][0]["description"].as_str().unwrap();
        assert!(description.contains("\u{1F600}"));
        assert!(description.contains("\u{4F60}\u{597D}"));
    }

    #[tokio::test]
    async fn test_create_pr_thread_with_unicode_issue_title() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let mut issue = create_test_issue();
        issue.title = "Fix bug: \u{1F41B} in authentication \u{6D4B}\u{8BD5}".to_string();

        let _result = manager
            .create_pr_thread(&issue, "https://pr/1", 1)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[1]; // message post
        let description = body["embeds"][0]["description"].as_str().unwrap();
        assert!(description.contains("\u{1F41B}"));
    }

    #[tokio::test]
    async fn test_unicode_truncation_safety() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        // Create a string that's > 1000 chars with multi-byte chars near the boundary
        let mut long_body = "a".repeat(995);
        long_body.push_str("\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}"); // Each is 4 bytes
        assert!(long_body.len() > 1000);

        let result = manager
            .notify_review_submitted("https://pr/1", "reviewer", "commented", Some(&long_body))
            .await;
        assert!(result.is_ok()); // Should not panic on char boundary
    }

    #[tokio::test]
    async fn test_agent_started_empty_description() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_agent_started("https://pr/1", "").await;
        assert!(result.is_ok());

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        assert_eq!(body["embeds"][0]["description"].as_str().unwrap(), "");
    }

    #[tokio::test]
    async fn test_agent_failed_empty_error() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.notify_agent_failed("https://pr/1", "").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_to_thread_empty_message() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager.send_to_thread("https://pr/1", "").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_review_comment_empty_comment() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let result = manager
            .notify_review_comment("https://pr/1", "reviewer", None, "")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_create_pr_thread_thread_creation_api_rate_limit() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            429,
            r#"{"message": "You are being rate limited.", "retry_after": 1.0}"#,
        );

        let manager = create_capturing_manager(&captured, None);
        let issue = create_test_issue();
        let result = manager.create_pr_thread(&issue, "https://pr/1", 1).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_multiple_notifications_to_same_thread() {
        let captured = CapturedRequests::new();
        // The mock returns the same response for any POST to this thread
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        // Send multiple notifications in sequence
        manager
            .notify_agent_started("https://pr/1", "Working")
            .await
            .unwrap();
        manager
            .notify_review_submitted("https://pr/1", "reviewer", "approved", None)
            .await
            .unwrap();
        manager
            .notify_agent_completed("https://pr/1", None)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        assert_eq!(posts.len(), 3);
    }

    #[tokio::test]
    async fn test_notification_to_different_threads() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-2/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state1 = ThreadState::new("thread-1", "PR: 1", "ch", "https://pr/1", "i1", "linear");
        let state2 = ThreadState::new("thread-2", "PR: 2", "ch", "https://pr/2", "i2", "linear");
        manager.load_threads(vec![state1, state2]).await;

        manager
            .notify_agent_started("https://pr/1", "Working on PR 1")
            .await
            .unwrap();
        manager
            .notify_agent_started("https://pr/2", "Working on PR 2")
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        assert_eq!(posts.len(), 2);
        assert!(posts[0].0.contains("thread-1"));
        assert!(posts[1].0.contains("thread-2"));
    }

    #[test]
    fn test_color_constants_are_valid_hex_colors() {
        // Discord colors must be in range 0x000000..=0xFFFFFF
        const { assert!(colors::SUCCESS <= 0xFFFFFF) };
        const { assert!(colors::ERROR <= 0xFFFFFF) };
        const { assert!(colors::INFO <= 0xFFFFFF) };
        const { assert!(colors::WARNING <= 0xFFFFFF) };
        const { assert!(colors::PURPLE <= 0xFFFFFF) };
        const { assert!(colors::REVIEW <= 0xFFFFFF) };
    }

    #[test]
    fn test_each_color_is_unique() {
        let all_colors = [
            colors::SUCCESS,
            colors::ERROR,
            colors::INFO,
            colors::WARNING,
            colors::PURPLE,
            colors::REVIEW,
        ];
        let unique: std::collections::HashSet<_> = all_colors.iter().collect();
        assert_eq!(
            unique.len(),
            all_colors.len(),
            "All colors should be unique"
        );
    }

    #[test]
    fn test_with_client_no_user_id() {
        let mock = MockDiscordClient::new();
        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let manager: ThreadManager<MockDiscordClient> =
            ThreadManager::with_client(client, "ch123", None);
        assert_eq!(manager.channel_id, "ch123");
        assert!(manager.user_id.is_none());
        assert!(manager.user_mention().is_none());
    }

    #[test]
    fn test_with_client_accepts_string_types() {
        let mock = MockDiscordClient::new();
        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let channel = String::from("my-channel");
        let manager = ThreadManager::with_client(client, channel, Some("uid".into()));
        assert_eq!(manager.channel_id, "my-channel");
        assert_eq!(manager.user_id, Some("uid".to_string()));
    }

    #[tokio::test]
    async fn test_full_lifecycle_create_notify_merge() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-456",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, Some("user1".to_string()));
        let issue = create_test_issue();

        // Step 1: Create thread
        let state = manager
            .create_pr_thread(&issue, "https://pr/1", 42)
            .await
            .unwrap();
        assert!(state.is_active);
        assert_eq!(state.thread_id, "thread-456");

        // Step 2: Review submitted
        manager
            .notify_review_submitted("https://pr/1", "alice", "changes_requested", Some("Fix it"))
            .await
            .unwrap();

        // Step 3: Agent works on feedback
        manager
            .notify_agent_started("https://pr/1", "Addressing review")
            .await
            .unwrap();

        // Step 4: Agent completes
        manager
            .notify_agent_completed("https://pr/1", Some("https://commit/abc"))
            .await
            .unwrap();

        // Step 5: Review approved
        manager
            .notify_review_submitted("https://pr/1", "alice", "approved", None)
            .await
            .unwrap();

        // Step 6: PR merged
        manager.notify_pr_merged("https://pr/1").await.unwrap();

        // Verify thread is now inactive
        let final_state = manager.get_thread_state("https://pr/1").await.unwrap();
        assert!(!final_state.is_active);

        // Verify all the API calls were made
        let posts = captured.get_captured_posts();
        // 1 thread creation + 1 initial message + 4 notification messages = 6
        assert_eq!(posts.len(), 7); // thread create POST + 6 message POSTs
        let patches = captured.get_captured_patches();
        assert_eq!(patches.len(), 1); // archive
    }

    #[tokio::test]
    async fn test_full_lifecycle_create_notify_close() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );
        captured.mock_patch(
            "https://discord.com/api/v10/channels/thread-456",
            200,
            mock_thread_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let issue = create_test_issue();

        // Create and then close
        manager
            .create_pr_thread(&issue, "https://pr/1", 1)
            .await
            .unwrap();

        manager
            .notify_agent_failed("https://pr/1", "Build failed")
            .await
            .unwrap();

        // Update mock to point at the thread ID used by the created thread
        manager.notify_pr_closed("https://pr/1").await.unwrap();

        let final_state = manager.get_thread_state("https://pr/1").await.unwrap();
        assert!(!final_state.is_active);
    }

    #[tokio::test]
    async fn test_concurrent_thread_state_reads() {
        let manager = ThreadManager::new("token", "channel", None).unwrap();

        let state1 = ThreadState::new("t1", "PR: 1", "ch", "https://pr/1", "i1", "linear");
        let state2 = ThreadState::new("t2", "PR: 2", "ch", "https://pr/2", "i2", "linear");
        manager.load_threads(vec![state1, state2]).await;

        // Simulate concurrent reads
        let mgr = &manager;
        let (r1, r2, r3) = tokio::join!(
            mgr.get_thread_state("https://pr/1"),
            mgr.get_thread_state("https://pr/2"),
            mgr.get_active_threads()
        );

        assert!(r1.is_some());
        assert!(r2.is_some());
        assert_eq!(r3.len(), 2);
    }

    #[tokio::test]
    async fn test_all_notification_embeds_have_claudear_footer() {
        let captured = CapturedRequests::new();
        // Allow multiple posts to the same thread
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        // Trigger several notifications
        manager
            .notify_review_submitted("https://pr/1", "r", "approved", None)
            .await
            .unwrap();
        manager
            .notify_review_comment("https://pr/1", "r", None, "Hi")
            .await
            .unwrap();
        manager
            .notify_agent_started("https://pr/1", "Task")
            .await
            .unwrap();
        manager
            .notify_agent_completed("https://pr/1", None)
            .await
            .unwrap();
        manager
            .notify_agent_failed("https://pr/1", "Error")
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        for (i, (_, body)) in posts.iter().enumerate() {
            let footer = body["embeds"][0]["footer"]["text"].as_str();
            assert_eq!(
                footer,
                Some("Claudear"),
                "Post {} should have Claudear footer",
                i
            );

            let timestamp = body["embeds"][0]["timestamp"].as_str();
            assert!(timestamp.is_some(), "Post {} should have a timestamp", i);
        }
    }

    #[tokio::test]
    async fn test_create_pr_thread_name_with_special_chars_in_issue() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let mut issue = create_test_issue();
        issue.short_id = "PROJ-999".to_string();
        issue.source = "jira".to_string();

        let _result = manager
            .create_pr_thread(&issue, "https://pr/999", 999)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let name = body["name"].as_str().unwrap();
        assert_eq!(name, "PR #999: PROJ-999 (jira)");
    }

    #[tokio::test]
    async fn test_create_pr_thread_high_pr_number() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let issue = create_test_issue();

        let _result = manager
            .create_pr_thread(&issue, "https://pr/99999", 99999)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let name = body["name"].as_str().unwrap();
        assert_eq!(name, "PR #99999: TEST-123 (linear)");
    }

    #[tokio::test]
    async fn test_create_pr_thread_propagates_issue_url_to_field() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let mut issue = create_test_issue();
        issue.url = "https://linear.app/test/issue/PROJ-42".to_string();
        issue.short_id = "PROJ-42".to_string();

        let _result = manager
            .create_pr_thread(&issue, "https://pr/42", 42)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[1]; // message post
        let issue_field = &body["embeds"][0]["fields"][0];
        let value = issue_field["value"].as_str().unwrap();
        assert!(value.contains("PROJ-42"));
        assert!(value.contains("https://linear.app/test/issue/PROJ-42"));
    }

    #[tokio::test]
    async fn test_create_pr_thread_propagates_priority() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let mut issue = create_test_issue();
        issue.priority = IssuePriority::Critical;

        let _result = manager
            .create_pr_thread(&issue, "https://pr/1", 1)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[1];
        let priority_field = &body["embeds"][0]["fields"][2];
        assert_eq!(priority_field["name"].as_str().unwrap(), "Priority");
        assert_eq!(priority_field["value"].as_str().unwrap(), "critical");
    }

    #[tokio::test]
    async fn test_thread_creation_uses_configured_channel() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/my-custom-channel/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let mock = CapturingMockClient::new(Arc::clone(&captured));
        let client = DiscordClient::with_http_client("token", mock).unwrap();
        let manager: ThreadManager<CapturingMockClient> =
            ThreadManager::with_client(client, "my-custom-channel", None);

        let issue = create_test_issue();
        let result = manager.create_pr_thread(&issue, "https://pr/1", 1).await;
        assert!(result.is_ok());

        let posts = captured.get_captured_posts();
        assert!(posts[0].0.contains("my-custom-channel/threads"));
    }

    #[tokio::test]
    async fn test_notifications_target_correct_thread_id() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/specific-thread-id/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new(
            "specific-thread-id",
            "PR: Test",
            "ch",
            "https://pr/1",
            "i1",
            "linear",
        );
        manager.load_threads(vec![state]).await;

        manager
            .notify_agent_started("https://pr/1", "Working")
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        assert!(posts[0].0.contains("specific-thread-id/messages"));
    }

    #[tokio::test]
    async fn test_review_comment_with_empty_file_path() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        // Empty string file path - still adds the field
        manager
            .notify_review_comment("https://pr/1", "reviewer", Some(""), "Comment")
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let fields = body["embeds"][0]["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 2); // "By" + "File"
        assert_eq!(fields[1]["value"].as_str().unwrap(), "``");
    }

    #[tokio::test]
    async fn test_review_submitted_body_exactly_1000_chars_no_truncation() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let body_1000 = "x".repeat(1000);
        manager
            .notify_review_submitted("https://pr/1", "reviewer", "commented", Some(&body_1000))
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let description = body["embeds"][0]["description"].as_str().unwrap();
        assert_eq!(description.len(), 1000);
        assert!(!description.ends_with("..."));
    }

    #[tokio::test]
    async fn test_review_submitted_body_1001_chars_truncated() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let body_1001 = "x".repeat(1001);
        manager
            .notify_review_submitted("https://pr/1", "reviewer", "commented", Some(&body_1001))
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let description = body["embeds"][0]["description"].as_str().unwrap();
        assert!(description.len() <= 1003);
        assert!(description.ends_with("..."));
    }

    #[tokio::test]
    async fn test_review_comment_exactly_1000_chars_no_truncation() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let comment_1000 = "y".repeat(1000);
        manager
            .notify_review_comment("https://pr/1", "reviewer", None, &comment_1000)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let description = body["embeds"][0]["description"].as_str().unwrap();
        assert_eq!(description.len(), 1000);
        assert!(!description.ends_with("..."));
    }

    #[tokio::test]
    async fn test_agent_failed_error_exactly_1000_chars_no_truncation() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-1/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let state = ThreadState::new("thread-1", "PR: Test", "ch", "https://pr/1", "i1", "linear");
        manager.load_threads(vec![state]).await;

        let error_1000 = "E".repeat(1000);
        manager
            .notify_agent_failed("https://pr/1", &error_1000)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0];
        let description = body["embeds"][0]["description"].as_str().unwrap();
        assert_eq!(description.len(), 1000);
        assert!(!description.ends_with("..."));
    }

    // DiscordClient.http is pub(crate)-accessible for CapturingMockClient
    // via ThreadManager.client field visibility
    // Verify the thread types in create_thread call
    #[tokio::test]
    async fn test_create_pr_thread_uses_public_thread_type() {
        let captured = CapturedRequests::new();
        captured.mock_post(
            "https://discord.com/api/v10/channels/channel123/threads",
            200,
            mock_thread_json(),
        );
        captured.mock_post(
            "https://discord.com/api/v10/channels/thread-456/messages",
            200,
            mock_message_json(),
        );

        let manager = create_capturing_manager(&captured, None);
        let issue = create_test_issue();
        manager
            .create_pr_thread(&issue, "https://pr/1", 1)
            .await
            .unwrap();

        let posts = captured.get_captured_posts();
        let (_, body) = &posts[0]; // thread creation
                                   // Public thread type = 11
        assert_eq!(body["type"].as_u64().unwrap(), 11);
        // Auto archive duration = 10080 (7 days)
        assert_eq!(body["auto_archive_duration"].as_u64().unwrap(), 10080);
    }
}
