//! Console notifier for local development/debugging.

use super::Notifier;
use async_trait::async_trait;
use claudear_core::error::Result;
use claudear_core::types::Issue;

/// Console notifier that prints to stdout.
pub struct ConsoleNotifier;

impl ConsoleNotifier {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ConsoleNotifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Notifier for ConsoleNotifier {
    fn name(&self) -> &str {
        "console"
    }

    fn is_enabled(&self) -> bool {
        true
    }

    fn is_local(&self) -> bool {
        true
    }

    async fn notify_start(&self, issue: &Issue) -> Result<()> {
        println!(
            "\n[{}] Processing: {} - {}",
            issue.source, issue.short_id, issue.title
        );
        if let Some(reason) = issue.get_metadata::<String>("trigger_reason") {
            println!("  Trigger: {}", reason);
        }
        Ok(())
    }

    async fn notify_success(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        if issue
            .get_metadata::<String>("cascade_downstream_repo")
            .is_some()
        {
            let upstream = issue
                .get_metadata::<String>("cascade_upstream_repo")
                .unwrap_or_default();
            let downstream = issue
                .get_metadata::<String>("cascade_downstream_repo")
                .unwrap_or_default();
            println!(
                "[{}] Cascade PR: {} ({} -> {}) - PR: {}",
                issue.source, issue.short_id, upstream, downstream, pr_url
            );
        } else if issue.get_metadata::<bool>("is_pr_update").unwrap_or(false) {
            println!(
                "[{}] PR Updated: {} - PR: {}",
                issue.source, issue.short_id, pr_url
            );
        } else {
            println!(
                "[{}] Success: {} - PR: {}",
                issue.source, issue.short_id, pr_url
            );
        }
        if let Some(reason) = issue.get_metadata::<String>("trigger_reason") {
            println!("  Trigger: {}", reason);
        }
        Ok(())
    }

    async fn notify_completed(&self, issue: &Issue) -> Result<()> {
        if issue
            .get_metadata::<bool>("regression_resolved")
            .unwrap_or(false)
        {
            println!(
                "[{}] Regression Resolved: {} (no regression after monitoring)",
                issue.source, issue.short_id
            );
        } else {
            let reason = issue
                .get_metadata::<String>("completion_reason")
                .unwrap_or_else(|| "no PR URL found".to_string());
            println!(
                "[{}] Completed: {} - {}",
                issue.source, issue.short_id, reason
            );
        }
        Ok(())
    }

    async fn notify_failed(&self, issue: &Issue, error: &str) -> Result<()> {
        if issue
            .get_metadata::<bool>("regression_detected")
            .unwrap_or(false)
        {
            eprintln!(
                "[{}] Regression Detected: {} - {}",
                issue.source, issue.short_id, error
            );
        } else if issue
            .get_metadata::<String>("cascade_downstream_repo")
            .is_some()
        {
            let downstream = issue
                .get_metadata::<String>("cascade_downstream_repo")
                .unwrap_or_default();
            eprintln!(
                "[{}] Cascade Failed: {} ({}) - {}",
                issue.source, issue.short_id, downstream, error
            );
        } else {
            eprintln!("[{}] Failed: {} - {}", issue.source, issue.short_id, error);
        }
        if let Some(reason) = issue.get_metadata::<String>("trigger_reason") {
            eprintln!("  Trigger: {}", reason);
        }
        Ok(())
    }

    async fn notify_merged(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        println!(
            "[{}] PR Merged: {} - PR: {}",
            issue.source, issue.short_id, pr_url
        );
        Ok(())
    }

    async fn notify_closed(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        println!(
            "[{}] PR Closed: {} - PR: {}",
            issue.source, issue.short_id, pr_url
        );
        Ok(())
    }

    async fn notify_status(&self, message: &str) -> Result<()> {
        println!("{}", message);
        Ok(())
    }

    async fn notify_urgent_issues(&self, issues: &[Issue]) -> Result<()> {
        println!("\n{} urgent issue(s) detected:", issues.len());
        for issue in issues.iter().take(10) {
            println!(
                "   - [{}] {}: {}",
                issue.source, issue.short_id, issue.title
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let notifier = ConsoleNotifier::new();
        assert_eq!(notifier.name(), "console");
    }

    #[test]
    fn test_default() {
        let notifier = ConsoleNotifier;
        assert_eq!(notifier.name(), "console");
    }

    #[test]
    fn test_name() {
        let notifier = ConsoleNotifier::new();
        assert_eq!(notifier.name(), "console");
    }

    #[test]
    fn test_is_enabled_always_true() {
        let notifier = ConsoleNotifier::new();
        assert!(notifier.is_enabled());
    }

    #[tokio::test]
    async fn test_notify_start() {
        let notifier = ConsoleNotifier::new();
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        // Should not panic and return Ok
        let result = notifier.notify_start(&issue).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_success() {
        let notifier = ConsoleNotifier::new();
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        let result = notifier
            .notify_success(&issue, "https://github.com/org/repo/pull/1")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_completed() {
        let notifier = ConsoleNotifier::new();
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        let result = notifier.notify_completed(&issue).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_failed() {
        let notifier = ConsoleNotifier::new();
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        let result = notifier.notify_failed(&issue, "Test error message").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_status() {
        let notifier = ConsoleNotifier::new();

        let result = notifier.notify_status("Test status message").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_empty() {
        let notifier = ConsoleNotifier::new();

        let result = notifier.notify_urgent_issues(&[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_single() {
        let notifier = ConsoleNotifier::new();
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://example.com",
            "linear",
        );

        let result = notifier.notify_urgent_issues(&[issue]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_urgent_issues_multiple() {
        let notifier = ConsoleNotifier::new();
        let issues: Vec<Issue> = (0..5)
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

    #[tokio::test]
    async fn test_notify_urgent_issues_truncated() {
        let notifier = ConsoleNotifier::new();
        // More than 10 issues
        let issues: Vec<Issue> = (0..15)
            .map(|i| {
                Issue::new(
                    format!("{}", i),
                    format!("PROJ-{}", i),
                    format!("Issue {}", i),
                    "https://example.com",
                    "sentry",
                )
            })
            .collect();

        let result = notifier.notify_urgent_issues(&issues).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_start_different_sources() {
        let notifier = ConsoleNotifier::new();

        for source in ["linear", "sentry", "github", "jira"] {
            let issue = Issue::new("123", "PROJ-123", "Test", "https://example.com", source);
            let result = notifier.notify_start(&issue).await;
            assert!(result.is_ok());
        }
    }

    #[tokio::test]
    async fn test_notify_start_empty_fields() {
        let notifier = ConsoleNotifier::new();
        let issue = Issue::new("", "", "", "", "");
        assert!(notifier.notify_start(&issue).await.is_ok());
    }

    #[tokio::test]
    async fn test_notify_success_empty_pr_url() {
        let notifier = ConsoleNotifier::new();
        let issue = Issue::new("1", "T-1", "test", "url", "linear");
        assert!(notifier.notify_success(&issue, "").await.is_ok());
    }

    #[tokio::test]
    async fn test_notify_failed_empty_error() {
        let notifier = ConsoleNotifier::new();
        let issue = Issue::new("1", "T-1", "test", "url", "linear");
        assert!(notifier.notify_failed(&issue, "").await.is_ok());
    }

    #[tokio::test]
    async fn test_notify_status_empty_message() {
        let notifier = ConsoleNotifier::new();
        assert!(notifier.notify_status("").await.is_ok());
    }

    #[tokio::test]
    async fn test_notify_start_unicode_title() {
        let notifier = ConsoleNotifier::new();
        let issue = Issue::new("1", "T-1", "Ошибка: 数据库超时 🔥", "url", "sentry");
        assert!(notifier.notify_start(&issue).await.is_ok());
    }

    #[tokio::test]
    async fn test_notify_failed_multiline_error() {
        let notifier = ConsoleNotifier::new();
        let issue = Issue::new("1", "T-1", "test", "url", "linear");
        assert!(notifier
            .notify_failed(&issue, "line1\nline2\nline3")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_notify_status_very_long_message() {
        let notifier = ConsoleNotifier::new();
        let long_msg = "x".repeat(10_000);
        assert!(notifier.notify_status(&long_msg).await.is_ok());
    }

    #[test]
    fn test_supports_replies_false() {
        let notifier = ConsoleNotifier::new();
        assert!(!notifier.supports_replies());
    }
}
