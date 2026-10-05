//! Sentry regression checker.
//!
//! Checks if a Sentry issue has new events after a fix was released.

use crate::regression::{RegressionChecker, RegressionResult};
use async_trait::async_trait;
use claudear_core::error::{Error, Result};
use claudear_core::types::RegressionWatch;
use claudear_core::types::SentryHttpClient;
use serde::Deserialize;

/// Configuration for Sentry regression checking.
#[derive(Debug, Clone)]
pub struct SentryRegressionConfig {
    /// Auth token for Sentry API.
    pub auth_token: String,
    /// Organization slug.
    pub org_slug: String,
    /// Minimum events to consider a regression.
    pub event_threshold: u32,
}

/// Sentry API response for issue events.
#[derive(Debug, Deserialize)]
struct SentryIssue {
    #[expect(dead_code)]
    id: String,
    #[serde(rename = "shortId")]
    short_id: String,
    #[expect(dead_code)]
    title: String,
    count: String,
    status: String,
    #[serde(rename = "lastSeen")]
    last_seen: String,
}

/// Sentry regression checker implementation.
pub struct SentryRegressionChecker<H: SentryHttpClient> {
    config: SentryRegressionConfig,
    http: H,
}

impl<H: SentryHttpClient> SentryRegressionChecker<H> {
    /// Create a new Sentry regression checker.
    pub fn new(config: SentryRegressionConfig, http: H) -> Self {
        Self { config, http }
    }

    /// Get the current state of a Sentry issue.
    async fn get_issue_state(&self, issue_id: &str) -> Result<Option<SentryIssue>> {
        let url = format!(
            "https://sentry.io/api/0/organizations/{}/issues/{}/",
            self.config.org_slug, issue_id
        );

        let response = self.http.get(&url, &self.config.auth_token).await?;

        if !response.is_success() {
            if response.status == 404 {
                return Ok(None);
            }
            return Err(Error::Other(format!(
                "Sentry API error ({}): {}",
                response.status, response.body
            )));
        }

        let issue: SentryIssue = response.json()?;
        Ok(Some(issue))
    }
}

#[async_trait]
impl<H: SentryHttpClient> RegressionChecker for SentryRegressionChecker<H> {
    async fn check_regression(&self, watch: &RegressionWatch) -> Result<RegressionResult> {
        // Get current issue state from Sentry
        let issue = match self.get_issue_state(&watch.issue_id).await? {
            Some(i) => i,
            None => {
                // Issue not found - can't determine regression
                return Ok(RegressionResult {
                    regression_detected: false,
                    details: Some("Sentry issue not found".to_string()),
                });
            }
        };

        // Parse event count once, with proper error handling
        // Using u64 since event counts are always non-negative
        let event_count: u64 = match issue.count.parse() {
            Ok(count) => count,
            Err(e) => {
                tracing::warn!(
                    issue_id = %issue.short_id,
                    raw_count = %issue.count,
                    error = %e,
                    "Failed to parse Sentry event count, defaulting to 0"
                );
                0
            }
        };

        // Check if issue status indicates an active problem
        let is_active = issue.status != "resolved" && issue.status != "ignored";

        // For active (unresolved) issues with events above threshold, it's a regression
        if is_active && event_count >= u64::from(self.config.event_threshold) {
            return Ok(RegressionResult::regression(format!(
                "Sentry issue {} has {} events and status '{}' after fix",
                issue.short_id, event_count, issue.status
            )));
        }

        // Check last seen date - but only for active issues
        // For resolved/ignored issues, the last_seen timestamp might be from before
        // the issue was resolved, so we shouldn't use it to determine regression.
        if is_active {
            if let Some(monitoring_started) = watch.monitoring_started_at {
                if let Ok(last_seen) = chrono::DateTime::parse_from_rfc3339(&issue.last_seen) {
                    if last_seen.with_timezone(&chrono::Utc) > monitoring_started {
                        return Ok(RegressionResult::regression(format!(
                            "Sentry issue {} had activity at {} (after monitoring started at {}), {} total events",
                            issue.short_id,
                            last_seen.format("%Y-%m-%d %H:%M"),
                            monitoring_started.format("%Y-%m-%d %H:%M"),
                            event_count
                        )));
                    }
                } else {
                    tracing::warn!(
                        issue_id = %issue.short_id,
                        last_seen = %issue.last_seen,
                        "Failed to parse Sentry last_seen timestamp"
                    );
                }
            }
        }

        Ok(RegressionResult {
            regression_detected: false,
            details: Some(format!(
                "Sentry issue {} is {} with no new events since monitoring started",
                issue.short_id, issue.status
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abnegate_http::HttpResponse;
    use chrono::Duration;
    use chrono::Utc;
    use claudear_core::types::IssueType;

    struct MockSentryClient {
        response: HttpResponse,
    }

    impl MockSentryClient {
        fn new(status: u16, body: &str) -> Self {
            Self {
                response: HttpResponse::new(status, body.to_string()),
            }
        }
    }

    #[async_trait]
    impl SentryHttpClient for MockSentryClient {
        async fn get(&self, _url: &str, _auth_token: &str) -> Result<HttpResponse> {
            Ok(self.response.clone())
        }

        async fn put(
            &self,
            _url: &str,
            _auth_token: &str,
            _body: serde_json::Value,
        ) -> Result<HttpResponse> {
            Ok(HttpResponse::new(200, "{}"))
        }
    }

    fn create_config() -> SentryRegressionConfig {
        SentryRegressionConfig {
            auth_token: "test".to_string(),
            org_slug: "test-org".to_string(),
            event_threshold: 1,
        }
    }

    #[tokio::test]
    async fn test_no_regression_when_resolved() {
        let mock = MockSentryClient::new(
            200,
            r#"{
                "id": "123",
                "shortId": "TEST-123",
                "title": "Test Error",
                "count": "0",
                "status": "resolved",
                "lastSeen": "2024-01-15T10:00:00Z"
            }"#,
        );

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let mut watch = RegressionWatch::new(IssueType::SentryIssue, "123", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_regression_when_unresolved_with_events() {
        let mock = MockSentryClient::new(
            200,
            r#"{
                "id": "456",
                "shortId": "TEST-456",
                "title": "Test Error",
                "count": "50",
                "status": "unresolved",
                "lastSeen": "2024-01-15T10:00:00Z"
            }"#,
        );

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let mut watch = RegressionWatch::new(IssueType::SentryIssue, "456", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(result.regression_detected);
        assert!(result.details.unwrap().contains("50 events"));
    }

    #[tokio::test]
    async fn test_regression_when_new_events_after_monitoring() {
        // Test that an unresolved issue with lastSeen after monitoring_started triggers regression
        // Use a count below threshold to test the lastSeen check path
        let future_time = Utc::now() + Duration::hours(1);
        let body = format!(
            r#"{{
                "id": "789",
                "shortId": "TEST-789",
                "title": "Test Error",
                "count": "0",
                "status": "unresolved",
                "lastSeen": "{}"
            }}"#,
            future_time.to_rfc3339()
        );

        let mock = MockSentryClient::new(200, &body);

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let mut watch = RegressionWatch::new(IssueType::SentryIssue, "789", 1);
        watch.monitoring_started_at = Some(Utc::now());

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(result.regression_detected);
        assert!(result.details.unwrap().contains("had activity"));
    }

    #[tokio::test]
    async fn test_regression_when_unresolved_with_high_event_count() {
        // Test that an unresolved issue with events above threshold triggers regression
        let body = r#"{
            "id": "791",
            "shortId": "TEST-791",
            "title": "Test Error",
            "count": "10",
            "status": "unresolved",
            "lastSeen": "2024-01-01T00:00:00Z"
        }"#;

        let mock = MockSentryClient::new(200, body);

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let watch = RegressionWatch::new(IssueType::SentryIssue, "791", 1);

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(result.regression_detected);
        assert!(result.details.unwrap().contains("10 events"));
    }

    #[tokio::test]
    async fn test_no_regression_for_resolved_issue_with_old_activity() {
        // Resolved issues should not trigger regression based on lastSeen
        // even if lastSeen is after monitoring started (the activity may have occurred
        // before the issue was manually resolved)
        let future_time = Utc::now() + Duration::hours(1);
        let body = format!(
            r#"{{
                "id": "790",
                "shortId": "TEST-790",
                "title": "Test Error",
                "count": "10",
                "status": "resolved",
                "lastSeen": "{}"
            }}"#,
            future_time.to_rfc3339()
        );

        let mock = MockSentryClient::new(200, &body);

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let mut watch = RegressionWatch::new(IssueType::SentryIssue, "790", 1);
        watch.monitoring_started_at = Some(Utc::now());

        let result = checker.check_regression(&watch).await.unwrap();
        // Resolved issues should NOT trigger regression based on lastSeen alone
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_issue_not_found() {
        let mock = MockSentryClient::new(404, r#"{"detail": "Not Found"}"#);

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let watch = RegressionWatch::new(IssueType::SentryIssue, "nonexistent", 1);

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
        assert!(result.details.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_no_regression_below_threshold() {
        let config = SentryRegressionConfig {
            auth_token: "test".to_string(),
            org_slug: "test-org".to_string(),
            event_threshold: 100, // High threshold
        };

        let mock = MockSentryClient::new(
            200,
            r#"{
                "id": "111",
                "shortId": "TEST-111",
                "title": "Test Error",
                "count": "50",
                "status": "unresolved",
                "lastSeen": "2024-01-15T10:00:00Z"
            }"#,
        );

        let checker = SentryRegressionChecker::new(config, mock);
        let mut watch = RegressionWatch::new(IssueType::SentryIssue, "111", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::days(2)); // Old monitoring start

        let result = checker.check_regression(&watch).await.unwrap();
        // The last_seen check won't trigger regression since it's before monitoring started
        // and the event count is below threshold
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_api_error_non_404_non_200() {
        // A non-404, non-200 status should return an Error
        let mock = MockSentryClient::new(500, r#"{"detail": "Internal Server Error"}"#);

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let watch = RegressionWatch::new(IssueType::SentryIssue, "err-issue", 1);

        let result = checker.check_regression(&watch).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("500"));
    }

    #[tokio::test]
    async fn test_unparseable_event_count() {
        // An unparseable count should default to 0, and with threshold=1
        // and status=unresolved, 0 < 1 so no regression from count path.
        // lastSeen is in the past, so no regression from timing path either.
        let mock = MockSentryClient::new(
            200,
            r#"{
                "id": "bad-count",
                "shortId": "TEST-BAD",
                "title": "Bad Count Error",
                "count": "not_a_number",
                "status": "unresolved",
                "lastSeen": "2024-01-01T00:00:00Z"
            }"#,
        );

        let config = SentryRegressionConfig {
            auth_token: "test".to_string(),
            org_slug: "test-org".to_string(),
            event_threshold: 1, // threshold is 1, unparseable defaults to 0 which is below
        };

        let checker = SentryRegressionChecker::new(config, mock);
        let mut watch = RegressionWatch::new(IssueType::SentryIssue, "bad-count", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_status_ignored_no_regression() {
        // Status "ignored" should not be considered active, so no regression
        // even with a high event count above threshold
        let mock = MockSentryClient::new(
            200,
            r#"{
                "id": "ign-1",
                "shortId": "TEST-IGN",
                "title": "Ignored Error",
                "count": "500",
                "status": "ignored",
                "lastSeen": "2099-01-01T00:00:00Z"
            }"#,
        );

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let mut watch = RegressionWatch::new(IssueType::SentryIssue, "ign-1", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_zero_event_count_below_threshold() {
        // Zero events with threshold of 1 -> no regression from event count path
        let mock = MockSentryClient::new(
            200,
            r#"{
                "id": "zero-1",
                "shortId": "TEST-ZERO",
                "title": "Zero Events Error",
                "count": "0",
                "status": "unresolved",
                "lastSeen": "2024-01-01T00:00:00Z"
            }"#,
        );

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let mut watch = RegressionWatch::new(IssueType::SentryIssue, "zero-1", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        // lastSeen is in 2024, well before monitoring_started_at, so no regression
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_no_monitoring_started_at() {
        // When monitoring_started_at is None, the lastSeen timing check is skipped.
        // Only the event count path is evaluated.
        // With count "0" and threshold 1, no regression should be detected.
        let mock = MockSentryClient::new(
            200,
            r#"{
                "id": "no-mon",
                "shortId": "TEST-NOMON",
                "title": "No Monitoring Start",
                "count": "0",
                "status": "unresolved",
                "lastSeen": "2099-12-31T23:59:59Z"
            }"#,
        );

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let watch = RegressionWatch::new(IssueType::SentryIssue, "no-mon", 1);
        // monitoring_started_at is None by default

        let result = checker.check_regression(&watch).await.unwrap();
        // count is 0 which is below threshold=1, so no regression from count path
        // monitoring_started_at is None so the lastSeen timing check is skipped
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_last_seen_before_monitoring_started() {
        // lastSeen is before monitoring_started_at -> no regression from timing path
        // even if the issue is unresolved
        let past_time = Utc::now() - Duration::hours(10);
        let body = format!(
            r#"{{
                "id": "before-mon",
                "shortId": "TEST-BEFORE",
                "title": "Old Activity",
                "count": "0",
                "status": "unresolved",
                "lastSeen": "{}"
            }}"#,
            past_time.to_rfc3339()
        );

        let mock = MockSentryClient::new(200, &body);

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let mut watch = RegressionWatch::new(IssueType::SentryIssue, "before-mon", 1);
        // monitoring started 5 hours ago, lastSeen is 10 hours ago (before monitoring)
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(5));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_invalid_last_seen_timestamp() {
        // Invalid lastSeen format should be handled gracefully: warning logged, no crash,
        // falls through to no-regression result
        let mock = MockSentryClient::new(
            200,
            r#"{
                "id": "bad-ts",
                "shortId": "TEST-BADTS",
                "title": "Bad Timestamp Error",
                "count": "0",
                "status": "unresolved",
                "lastSeen": "not-a-valid-timestamp"
            }"#,
        );

        let checker = SentryRegressionChecker::new(create_config(), mock);
        let mut watch = RegressionWatch::new(IssueType::SentryIssue, "bad-ts", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        // Should not panic or error - handles gracefully
        let result = checker.check_regression(&watch).await.unwrap();
        // count is 0 < threshold 1, invalid lastSeen is skipped, so no regression
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_exact_threshold_boundary() {
        // Event count exactly equals threshold -> regression detected
        let config = SentryRegressionConfig {
            auth_token: "test".to_string(),
            org_slug: "test-org".to_string(),
            event_threshold: 5,
        };

        let mock = MockSentryClient::new(
            200,
            r#"{
                "id": "exact-th",
                "shortId": "TEST-EXACT",
                "title": "Exact Threshold Error",
                "count": "5",
                "status": "unresolved",
                "lastSeen": "2024-01-01T00:00:00Z"
            }"#,
        );

        let checker = SentryRegressionChecker::new(config, mock);
        let mut watch = RegressionWatch::new(IssueType::SentryIssue, "exact-th", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        // count (5) >= threshold (5) and status is unresolved -> regression
        assert!(result.regression_detected);
        assert!(result.details.unwrap().contains("5 events"));
    }
}
