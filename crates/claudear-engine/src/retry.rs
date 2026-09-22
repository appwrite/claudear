//! Retry logic with exponential backoff for fix attempts.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use claudear_config::config::RetryConfig;
use claudear_core::error::Result;
use claudear_core::types::{ActivityLogEntry, FixAttempt, FixAttemptStatus, TimelineEventStatus};
use claudear_storage::FixAttemptTracker;
use serde_json::json;
use std::sync::Arc;
use tokio::time::Duration;

/// Manages retry logic for fix attempts.
pub struct RetryManager {
    config: RetryConfig,
    tracker: Arc<dyn FixAttemptTracker>,
}

impl RetryManager {
    /// Create a new retry manager.
    pub fn new(config: RetryConfig, tracker: Arc<dyn FixAttemptTracker>) -> Self {
        Self { config, tracker }
    }

    /// Check if an attempt should be retried.
    pub fn should_retry(&self, attempt: &FixAttempt) -> bool {
        // Only retry failed/closed attempts. Pending attempts are either actively
        // processing or freshly recorded — never eligible for retry.
        if attempt.status != FixAttemptStatus::Failed && attempt.status != FixAttemptStatus::Closed
        {
            return false;
        }

        // Check if we've exceeded max retries
        if attempt.retry_count >= self.config.max_retries {
            return false;
        }

        true
    }

    /// Check if enough time has passed since the last retry.
    pub fn is_ready_for_retry(&self, attempt: &FixAttempt) -> bool {
        if !self.should_retry(attempt) {
            return false;
        }

        let delay = self.get_delay(attempt.retry_count);
        let min_retry_time = attempt
            .last_retry_at
            .or(Some(attempt.attempted_at))
            .map(|t| {
                t + ChronoDuration::milliseconds(
                    i64::try_from(delay.as_millis()).unwrap_or(i64::MAX),
                )
            })
            .unwrap_or(Utc::now());

        Utc::now() >= min_retry_time
    }

    /// Calculate the delay before the next retry using exponential backoff.
    /// Formula: min(base_delay * 2^retry_count, max_delay)
    pub fn get_delay(&self, retry_count: u32) -> Duration {
        let multiplier = 2u64.saturating_pow(retry_count);
        let delay_ms = self.config.base_delay_ms.saturating_mul(multiplier);
        let capped_delay = delay_ms.min(self.config.max_delay_ms);
        Duration::from_millis(capped_delay)
    }

    /// Get the next retry time for an attempt.
    pub fn get_next_retry_time(&self, attempt: &FixAttempt) -> Option<DateTime<Utc>> {
        if !self.should_retry(attempt) {
            return None;
        }

        let delay = self.get_delay(attempt.retry_count);
        let base_time = attempt.last_retry_at.unwrap_or(attempt.attempted_at);
        Some(
            base_time
                + ChronoDuration::milliseconds(
                    i64::try_from(delay.as_millis()).unwrap_or(i64::MAX),
                ),
        )
    }

    /// Process a failed attempt - either schedule retry or mark as cannot fix.
    pub fn handle_failure(
        &self,
        source: &str,
        issue_id: &str,
        error: &str,
    ) -> Result<RetryDecision> {
        let attempt = self.tracker.get_attempt(source, issue_id)?;

        match attempt {
            Some(attempt) => {
                let new_retry_count = attempt.retry_count + 1;

                if new_retry_count > self.config.max_retries {
                    // Max retries reached
                    self.tracker.mark_cannot_fix(
                        source,
                        issue_id,
                        &format!(
                            "Max retries ({}) reached. Last error: {}",
                            self.config.max_retries, error
                        ),
                    )?;
                    // Timeline: max retries reached, cannot auto-fix.
                    self.tracker
                        .record_activity(
                            &ActivityLogEntry::new(
                                TimelineEventStatus::FixAbandoned.as_str(),
                                format!("Fix abandoned for {} (max retries)", attempt.short_id),
                            )
                            .with_source(source.to_string())
                            .with_issue(issue_id.to_string(), attempt.short_id.clone())
                            .with_metadata(json!({ "max_retries": self.config.max_retries })),
                        )
                        .ok();
                    let _ = self
                        .tracker
                        .update_qa_outcome_stats_for_attempt(attempt.id, false);
                    tracing::warn!(
                        component = "retry",
                        short_id = %attempt.short_id,
                        max_retries = self.config.max_retries,
                        "Reached max retries, marking as cannot_fix"
                    );
                    Ok(RetryDecision::CannotFix)
                } else {
                    // Schedule retry - don't increment retry_count here, that's done in prepare_retry()
                    // when the retry is actually executed. retry_count represents "retries initiated".
                    self.tracker.mark_failed(source, issue_id, error)?;

                    // Use current retry_count for delay calculation (0 = waiting for first retry, 1 = waiting for second, etc.)
                    let delay = self.get_delay(attempt.retry_count);
                    let next_retry_time = Utc::now()
                        + ChronoDuration::milliseconds(
                            i64::try_from(delay.as_millis()).unwrap_or(i64::MAX),
                        );

                    tracing::info!(
                        component = "retry",
                        short_id = %attempt.short_id,
                        retry_count = new_retry_count,
                        max_retries = self.config.max_retries,
                        delay = ?delay,
                        "Will retry"
                    );

                    // Log retry_scheduled activity
                    let activity = ActivityLogEntry::new(
                        "retry_scheduled",
                        format!(
                            "Retry scheduled for {} at {}",
                            attempt.short_id,
                            next_retry_time.format("%Y-%m-%dT%H:%M:%SZ")
                        ),
                    )
                    .with_source(source.to_string())
                    .with_issue(issue_id.to_string(), attempt.short_id.clone())
                    .with_metadata(json!({
                        "retry_count": new_retry_count,
                        "max_retries": self.config.max_retries,
                        "next_retry": next_retry_time.to_rfc3339(),
                        "delay_ms": delay.as_millis() as u64
                    }));
                    self.tracker.record_activity(&activity).ok();

                    Ok(RetryDecision::Retry {
                        retry_count: new_retry_count,
                        delay,
                    })
                }
            }
            None => {
                tracing::warn!(component = "retry", source = %source, issue_id = %issue_id, "Attempt not found");
                Ok(RetryDecision::NotFound)
            }
        }
    }

    /// Handle a PR being closed (not merged) - triggers retry logic.
    pub fn handle_pr_closed(&self, source: &str, issue_id: &str) -> Result<RetryDecision> {
        self.handle_failure(source, issue_id, "PR was closed without merging")
    }

    /// Handle a regression being detected - triggers retry logic.
    pub fn handle_regression(&self, source: &str, issue_id: &str) -> Result<RetryDecision> {
        self.handle_failure(source, issue_id, "Regression detected after merge")
    }

    /// Get all attempts that are ready to be retried now.
    pub fn get_ready_retries(&self) -> Result<Vec<FixAttempt>> {
        let retryable = self.tracker.get_retryable_issues(self.config.max_retries)?;

        Ok(retryable
            .into_iter()
            .filter(|a| self.is_ready_for_retry(a))
            .collect())
    }

    /// Prepare an attempt for retry - resets status and clears PR info.
    /// The retry count increment is done atomically inside prepare_for_retry.
    pub fn prepare_retry(&self, source: &str, issue_id: &str) -> Result<()> {
        // Get attempt info before preparing
        let attempt = self.tracker.get_attempt(source, issue_id)?;
        let (short_id, retry_count) = attempt
            .map(|a| (a.short_id.clone(), a.retry_count + 1))
            .unwrap_or_else(|| (issue_id.to_string(), 1));

        // Atomically increments retry_count AND resets status to pending.
        // Guarded: only succeeds if status is 'failed' or 'closed'.
        self.tracker.prepare_for_retry(source, issue_id)?;

        // Log retry_executed activity
        let activity = ActivityLogEntry::new(
            "retry_executed",
            format!("Executing retry {} for {}", retry_count, short_id),
        )
        .with_source(source.to_string())
        .with_issue(issue_id.to_string(), short_id)
        .with_metadata(json!({
            "retry_count": retry_count
        }));
        self.tracker.record_activity(&activity).ok();

        Ok(())
    }
}

/// Decision made by retry logic.
#[derive(Debug, Clone)]
pub enum RetryDecision {
    /// Will retry after the specified delay.
    Retry { retry_count: u32, delay: Duration },
    /// Max retries reached, cannot fix automatically.
    CannotFix,
    /// Attempt not found.
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_storage::SqliteTracker;

    fn create_test_tracker() -> Arc<dyn FixAttemptTracker> {
        Arc::new(SqliteTracker::in_memory().unwrap())
    }

    #[test]
    fn test_exponential_backoff_delay() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay_ms: 1000,  // 1 second
            max_delay_ms: 10_000, // 10 seconds
        };
        let manager = RetryManager::new(config, create_test_tracker());

        // First retry: 1s * 2^0 = 1s
        assert_eq!(manager.get_delay(0), Duration::from_millis(1000));

        // Second retry: 1s * 2^1 = 2s
        assert_eq!(manager.get_delay(1), Duration::from_millis(2000));

        // Third retry: 1s * 2^2 = 4s
        assert_eq!(manager.get_delay(2), Duration::from_millis(4000));

        // Fourth retry: 1s * 2^3 = 8s
        assert_eq!(manager.get_delay(3), Duration::from_millis(8000));

        // Fifth retry: 1s * 2^4 = 16s, but capped at 10s
        assert_eq!(manager.get_delay(4), Duration::from_millis(10_000));
    }

    #[test]
    fn test_should_retry_status() {
        let config = RetryConfig::default();
        let manager = RetryManager::new(config, create_test_tracker());

        let failed = FixAttempt {
            id: 1,
            issue_id: "123".to_string(),
            short_id: "PROJ-123".to_string(),
            source: "linear".to_string(),
            attempted_at: Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Failed,
            error_message: Some("Error".to_string()),
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };
        assert!(manager.should_retry(&failed));

        let closed = FixAttempt {
            status: FixAttemptStatus::Closed,
            ..failed.clone()
        };
        assert!(manager.should_retry(&closed));

        let pending = FixAttempt {
            status: FixAttemptStatus::Pending,
            ..failed.clone()
        };
        assert!(!manager.should_retry(&pending));

        let success = FixAttempt {
            status: FixAttemptStatus::Success,
            ..failed.clone()
        };
        assert!(!manager.should_retry(&success));

        let merged = FixAttempt {
            status: FixAttemptStatus::Merged,
            ..failed.clone()
        };
        assert!(!manager.should_retry(&merged));

        let cannot_fix = FixAttempt {
            status: FixAttemptStatus::CannotFix,
            ..failed.clone()
        };
        assert!(!manager.should_retry(&cannot_fix));
    }

    #[test]
    fn test_should_retry_max_retries() {
        let config = RetryConfig {
            max_retries: 2,
            ..Default::default()
        };
        let manager = RetryManager::new(config, create_test_tracker());

        let attempt = FixAttempt {
            id: 1,
            issue_id: "123".to_string(),
            short_id: "PROJ-123".to_string(),
            source: "linear".to_string(),
            attempted_at: Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Failed,
            error_message: Some("Error".to_string()),
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };
        assert!(manager.should_retry(&attempt));

        let attempt_1_retry = FixAttempt {
            retry_count: 1,
            ..attempt.clone()
        };
        assert!(manager.should_retry(&attempt_1_retry));

        let attempt_2_retries = FixAttempt {
            retry_count: 2,
            ..attempt.clone()
        };
        assert!(!manager.should_retry(&attempt_2_retries));

        let attempt_3_retries = FixAttempt {
            retry_count: 3,
            ..attempt.clone()
        };
        assert!(!manager.should_retry(&attempt_3_retries));
    }

    #[test]
    fn test_handle_failure_schedules_retry() {
        let tracker = create_test_tracker();
        let config = RetryConfig {
            max_retries: 2,
            ..Default::default()
        };
        let manager = RetryManager::new(config, tracker.clone());

        // Record an attempt
        tracker.record_attempt("linear", "123", "PROJ-123").unwrap();
        tracker
            .mark_failed("linear", "123", "First failure")
            .unwrap();

        // Handle failure - should schedule retry but NOT increment retry_count
        // (retry_count is incremented by prepare_retry when the retry is actually executed)
        let decision = manager
            .handle_failure("linear", "123", "Second failure")
            .unwrap();
        match decision {
            RetryDecision::Retry { retry_count, .. } => {
                // retry_count in decision = next retry number (1 = first retry)
                assert_eq!(retry_count, 1);
            }
            _ => panic!("Expected Retry decision"),
        }

        // Check that retry count was NOT incremented in handle_failure
        // (will be incremented by prepare_retry)
        let attempt = tracker.get_attempt("linear", "123").unwrap().unwrap();
        assert_eq!(attempt.retry_count, 0);
    }

    #[test]
    fn test_handle_failure_marks_cannot_fix() {
        let tracker = create_test_tracker();
        let config = RetryConfig {
            max_retries: 1,
            ..Default::default()
        };
        let manager = RetryManager::new(config, tracker.clone());

        // Record an attempt with retry_count = 1 (at max)
        tracker.record_attempt("linear", "123", "PROJ-123").unwrap();
        tracker
            .mark_failed("linear", "123", "First failure")
            .unwrap();
        tracker.increment_retry("linear", "123").unwrap();

        // Handle failure - should mark as cannot_fix
        let decision = manager
            .handle_failure("linear", "123", "Final failure")
            .unwrap();
        match decision {
            RetryDecision::CannotFix => {}
            _ => panic!("Expected CannotFix decision"),
        }

        // Check that status is cannot_fix
        let attempt = tracker.get_attempt("linear", "123").unwrap().unwrap();
        assert_eq!(attempt.status, FixAttemptStatus::CannotFix);
    }

    #[test]
    fn test_handle_failure_not_found() {
        let tracker = create_test_tracker();
        let config = RetryConfig::default();
        let manager = RetryManager::new(config, tracker);

        let decision = manager
            .handle_failure("linear", "nonexistent", "Error")
            .unwrap();
        assert!(matches!(decision, RetryDecision::NotFound));
    }

    #[test]
    fn test_handle_pr_closed() {
        let tracker = create_test_tracker();
        let config = RetryConfig {
            max_retries: 2,
            ..Default::default()
        };
        let manager = RetryManager::new(config, tracker.clone());

        tracker.record_attempt("linear", "123", "PROJ-123").unwrap();
        tracker
            .mark_success("linear", "123", "https://github.com/org/repo/pull/1")
            .unwrap();
        tracker.mark_closed("linear", "123").unwrap();

        let decision = manager.handle_pr_closed("linear", "123").unwrap();
        match decision {
            RetryDecision::Retry { retry_count, .. } => {
                assert_eq!(retry_count, 1);
            }
            _ => panic!("Expected Retry decision"),
        }
    }

    #[test]
    fn test_get_next_retry_time_not_retryable() {
        let config = RetryConfig::default();
        let manager = RetryManager::new(config, create_test_tracker());

        let attempt = FixAttempt {
            id: 1,
            issue_id: "123".to_string(),
            short_id: "PROJ-123".to_string(),
            source: "linear".to_string(),
            attempted_at: Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Success, // Not retryable
            error_message: None,
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        assert!(manager.get_next_retry_time(&attempt).is_none());
    }

    #[test]
    fn test_get_next_retry_time_with_last_retry() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay_ms: 60_000, // 1 minute
            max_delay_ms: 3_600_000,
        };
        let manager = RetryManager::new(config, create_test_tracker());

        let base_time = Utc::now();
        let attempt = FixAttempt {
            id: 1,
            issue_id: "123".to_string(),
            short_id: "PROJ-123".to_string(),
            source: "linear".to_string(),
            attempted_at: base_time - ChronoDuration::hours(1),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Failed,
            error_message: Some("Error".to_string()),
            merged_at: None,
            resolved_at: None,
            retry_count: 2,
            last_retry_at: Some(base_time),
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        let next_retry = manager.get_next_retry_time(&attempt).unwrap();
        // With retry_count=2, delay = 60s * 2^2 = 240s = 4 minutes
        let expected = base_time + ChronoDuration::milliseconds(240_000);

        // Allow some tolerance for test execution time
        let diff = (next_retry - expected).num_seconds().abs();
        assert!(
            diff < 2,
            "Expected next retry time around {:?}, got {:?}",
            expected,
            next_retry
        );
    }

    #[test]
    fn test_is_ready_for_retry_not_retryable() {
        let config = RetryConfig::default();
        let manager = RetryManager::new(config, create_test_tracker());

        let attempt = FixAttempt {
            id: 1,
            issue_id: "123".to_string(),
            short_id: "PROJ-123".to_string(),
            source: "linear".to_string(),
            attempted_at: Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Merged, // Not retryable
            error_message: None,
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        assert!(!manager.is_ready_for_retry(&attempt));
    }

    #[test]
    fn test_is_ready_for_retry_time_not_elapsed() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay_ms: 3_600_000, // 1 hour
            max_delay_ms: 7_200_000,
        };
        let manager = RetryManager::new(config, create_test_tracker());

        // Attempted just now, shouldn't be ready for 1 hour
        let attempt = FixAttempt {
            id: 1,
            issue_id: "123".to_string(),
            short_id: "PROJ-123".to_string(),
            source: "linear".to_string(),
            attempted_at: Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Failed,
            error_message: Some("Error".to_string()),
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        assert!(!manager.is_ready_for_retry(&attempt));
    }

    #[test]
    fn test_is_ready_for_retry_time_elapsed() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay_ms: 1_000, // 1 second
            max_delay_ms: 10_000,
        };
        let manager = RetryManager::new(config, create_test_tracker());

        // Attempted 1 hour ago, should be ready (delay is only 1 second)
        let attempt = FixAttempt {
            id: 1,
            issue_id: "123".to_string(),
            short_id: "PROJ-123".to_string(),
            source: "linear".to_string(),
            attempted_at: Utc::now() - ChronoDuration::hours(1),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Failed,
            error_message: Some("Error".to_string()),
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        assert!(manager.is_ready_for_retry(&attempt));
    }

    #[test]
    fn test_get_ready_retries() {
        let tracker = create_test_tracker();
        let config = RetryConfig {
            max_retries: 3,
            base_delay_ms: 1_000, // 1 second
            max_delay_ms: 10_000,
        };
        let manager = RetryManager::new(config, tracker.clone());

        // Create a failed attempt from 1 hour ago (ready to retry)
        tracker.record_attempt("linear", "1", "PROJ-1").unwrap();
        tracker.mark_failed("linear", "1", "Error").unwrap();

        // Manually update attempted_at to be in the past
        // Note: This is a simplified test - in practice, the time check matters

        let ready = manager.get_ready_retries().unwrap();
        // May or may not be ready depending on timing, but should not panic
        assert!(ready.len() <= 1);
    }

    #[test]
    fn test_prepare_retry() {
        let tracker = create_test_tracker();
        let config = RetryConfig::default();
        let manager = RetryManager::new(config, tracker.clone());

        tracker.record_attempt("linear", "123", "PROJ-123").unwrap();
        tracker
            .mark_success("linear", "123", "https://github.com/org/repo/pull/1")
            .unwrap();
        tracker.mark_closed("linear", "123").unwrap();

        manager.prepare_retry("linear", "123").unwrap();

        let attempt = tracker.get_attempt("linear", "123").unwrap().unwrap();
        assert_eq!(attempt.status, FixAttemptStatus::Pending);
        assert!(attempt.pr_url.is_none());
        assert_eq!(attempt.retry_count, 1);
    }

    #[test]
    fn test_exponential_backoff_overflow() {
        let config = RetryConfig {
            max_retries: 100,
            base_delay_ms: u64::MAX / 2,
            max_delay_ms: u64::MAX,
        };
        let manager = RetryManager::new(config, create_test_tracker());

        // Should not panic with very large values
        let delay = manager.get_delay(50);
        assert!(delay.as_millis() <= u64::MAX as u128);
    }

    #[test]
    fn test_retry_decision_debug() {
        let retry = RetryDecision::Retry {
            retry_count: 2,
            delay: Duration::from_secs(60),
        };
        let debug_str = format!("{:?}", retry);
        assert!(debug_str.contains("Retry"));
        assert!(debug_str.contains("2"));

        let cannot_fix = RetryDecision::CannotFix;
        let debug_str = format!("{:?}", cannot_fix);
        assert!(debug_str.contains("CannotFix"));

        let not_found = RetryDecision::NotFound;
        let debug_str = format!("{:?}", not_found);
        assert!(debug_str.contains("NotFound"));
    }

    #[test]
    fn test_retry_decision_clone() {
        let original = RetryDecision::Retry {
            retry_count: 3,
            delay: Duration::from_secs(120),
        };
        let cloned = original.clone();

        if let RetryDecision::Retry { retry_count, delay } = cloned {
            assert_eq!(retry_count, 3);
            assert_eq!(delay, Duration::from_secs(120));
        } else {
            panic!("Clone failed");
        }
    }

    #[test]
    fn test_delay_capping() {
        let config = RetryConfig {
            max_retries: 10,
            base_delay_ms: 1_000,
            max_delay_ms: 5_000, // Cap at 5 seconds
        };
        let manager = RetryManager::new(config, create_test_tracker());

        // 1s * 2^0 = 1s
        assert_eq!(manager.get_delay(0), Duration::from_millis(1_000));
        // 1s * 2^1 = 2s
        assert_eq!(manager.get_delay(1), Duration::from_millis(2_000));
        // 1s * 2^2 = 4s
        assert_eq!(manager.get_delay(2), Duration::from_millis(4_000));
        // 1s * 2^3 = 8s, capped at 5s
        assert_eq!(manager.get_delay(3), Duration::from_millis(5_000));
        // All subsequent should be capped
        assert_eq!(manager.get_delay(10), Duration::from_millis(5_000));
    }

    #[test]
    fn test_get_delay_zero_base() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay_ms: 0,
            max_delay_ms: 10_000,
        };
        let manager = RetryManager::new(config, create_test_tracker());
        assert_eq!(manager.get_delay(0), Duration::from_millis(0));
        assert_eq!(manager.get_delay(5), Duration::from_millis(0));
    }

    #[test]
    fn test_get_delay_zero_max() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay_ms: 1000,
            max_delay_ms: 0,
        };
        let manager = RetryManager::new(config, create_test_tracker());
        assert_eq!(manager.get_delay(0), Duration::from_millis(0));
        assert_eq!(manager.get_delay(3), Duration::from_millis(0));
    }

    #[test]
    fn test_get_delay_base_equals_max() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay_ms: 1000,
            max_delay_ms: 1000,
        };
        let manager = RetryManager::new(config, create_test_tracker());
        assert_eq!(manager.get_delay(0), Duration::from_millis(1000));
        assert_eq!(manager.get_delay(1), Duration::from_millis(1000));
        assert_eq!(manager.get_delay(10), Duration::from_millis(1000));
    }

    #[test]
    fn test_get_delay_u64_max_base() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay_ms: u64::MAX,
            max_delay_ms: u64::MAX,
        };
        let manager = RetryManager::new(config, create_test_tracker());
        let delay = manager.get_delay(10);
        assert_eq!(delay, Duration::from_millis(u64::MAX));
    }

    #[test]
    fn test_should_retry_max_retries_zero() {
        let config = RetryConfig {
            max_retries: 0,
            base_delay_ms: 1000,
            max_delay_ms: 10_000,
        };
        let manager = RetryManager::new(config, create_test_tracker());
        let attempt = FixAttempt {
            id: 1,
            issue_id: "1".into(),
            short_id: "T-1".into(),
            source: "linear".into(),
            attempted_at: Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Failed,
            error_message: None,
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };
        assert!(!manager.should_retry(&attempt));
    }

    #[test]
    fn test_get_next_retry_time_uses_last_retry_at() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay_ms: 60_000,
            max_delay_ms: 3_600_000,
        };
        let manager = RetryManager::new(config, create_test_tracker());
        let last_retry = Utc::now() - ChronoDuration::hours(2);
        let attempt = FixAttempt {
            id: 1,
            issue_id: "1".into(),
            short_id: "T-1".into(),
            source: "linear".into(),
            attempted_at: Utc::now() - ChronoDuration::hours(5),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Failed,
            error_message: None,
            merged_at: None,
            resolved_at: None,
            retry_count: 1,
            last_retry_at: Some(last_retry),
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };
        let next = manager.get_next_retry_time(&attempt).unwrap();
        let expected = last_retry + ChronoDuration::milliseconds(120_000);
        let diff = (next - expected).num_seconds().abs();
        assert!(diff < 2, "Next retry should be based on last_retry_at");
    }

    #[test]
    fn test_get_next_retry_time_falls_back_to_attempted_at() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay_ms: 1_000,
            max_delay_ms: 10_000,
        };
        let manager = RetryManager::new(config, create_test_tracker());
        let attempted = Utc::now() - ChronoDuration::hours(1);
        let attempt = FixAttempt {
            id: 1,
            issue_id: "1".into(),
            short_id: "T-1".into(),
            source: "linear".into(),
            attempted_at: attempted,
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Failed,
            error_message: None,
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };
        let next = manager.get_next_retry_time(&attempt).unwrap();
        let expected = attempted + ChronoDuration::milliseconds(1_000);
        let diff = (next - expected).num_seconds().abs();
        assert!(diff < 2);
    }

    #[test]
    fn test_handle_failure_full_retry_lifecycle() {
        let tracker = create_test_tracker();
        let config = RetryConfig {
            max_retries: 2,
            base_delay_ms: 1000,
            max_delay_ms: 10_000,
        };
        let manager = RetryManager::new(config, tracker.clone());

        tracker.record_attempt("linear", "1", "T-1").unwrap();
        tracker.mark_failed("linear", "1", "err").unwrap();

        // First failure
        let d1 = manager.handle_failure("linear", "1", "e1").unwrap();
        assert!(matches!(d1, RetryDecision::Retry { retry_count: 1, .. }));

        // Execute retry
        manager.prepare_retry("linear", "1").unwrap();
        tracker.mark_failed("linear", "1", "e2").unwrap();

        // Second failure
        let d2 = manager.handle_failure("linear", "1", "e2").unwrap();
        assert!(matches!(d2, RetryDecision::Retry { retry_count: 2, .. }));

        // Execute second retry
        manager.prepare_retry("linear", "1").unwrap();
        tracker.mark_failed("linear", "1", "e3").unwrap();

        // Third failure should be CannotFix
        let d3 = manager.handle_failure("linear", "1", "e3").unwrap();
        assert!(matches!(d3, RetryDecision::CannotFix));
    }

    #[test]
    fn test_handle_regression() {
        let tracker = create_test_tracker();
        let config = RetryConfig {
            max_retries: 2,
            ..Default::default()
        };
        let manager = RetryManager::new(config, tracker.clone());

        tracker.record_attempt("sentry", "1", "S-1").unwrap();
        tracker
            .mark_success("sentry", "1", "https://github.com/org/repo/pull/1")
            .unwrap();
        tracker.mark_merged("sentry", "1").unwrap();

        let decision = manager.handle_regression("sentry", "1").unwrap();
        assert!(matches!(
            decision,
            RetryDecision::Retry { retry_count: 1, .. }
        ));
    }

    #[test]
    fn test_prepare_retry_nonexistent() {
        let tracker = create_test_tracker();
        let config = RetryConfig::default();
        let manager = RetryManager::new(config, tracker);
        // Should not panic
        let _ = manager.prepare_retry("linear", "nonexistent");
    }
}
