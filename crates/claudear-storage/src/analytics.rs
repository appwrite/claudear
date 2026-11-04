//! Analytics-specific queries and aggregation functions.
//!
//! This module provides higher-level analytics functionality built on top of
//! the core storage layer, including trend analysis, success rate calculations,
//! and time-series data retrieval.

use chrono::{DateTime, Duration, Utc};
use claudear_core::error::Result;
use claudear_core::types::{AnalyticsSummary, ErrorPattern, ProcessingMetric};
use std::collections::HashMap;

use super::FixAttemptTracker;

/// Time period for trend analysis.
#[derive(Debug, Clone, Copy)]
pub enum TimePeriod {
    /// Last hour
    Hour,
    /// Last 24 hours
    Day,
    /// Last 7 days
    Week,
    /// Last 30 days
    Month,
}

impl TimePeriod {
    /// Get the duration for this time period.
    pub fn duration(&self) -> Duration {
        match self {
            TimePeriod::Hour => Duration::hours(1),
            TimePeriod::Day => Duration::days(1),
            TimePeriod::Week => Duration::days(7),
            TimePeriod::Month => Duration::days(30),
        }
    }

    /// Get the start time for this period from now.
    pub fn start_time(&self) -> DateTime<Utc> {
        Utc::now() - self.duration()
    }
}

/// Trend direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrendDirection {
    Up,
    Down,
    Stable,
}

/// Trend analysis result.
#[derive(Debug, Clone)]
pub struct TrendAnalysis {
    /// Current value
    pub current: f64,
    /// Previous value (for comparison period)
    pub previous: f64,
    /// Percentage change
    pub change_percent: f64,
    /// Trend direction
    pub direction: TrendDirection,
}

impl TrendAnalysis {
    /// Create a new trend analysis.
    ///
    /// Computes percentage change using the absolute value of `previous` as the
    /// denominator so that negative-to-negative transitions (e.g. -100 → -50)
    /// are reported correctly.
    pub fn new(current: f64, previous: f64) -> Self {
        let change_percent = if previous != 0.0 {
            ((current - previous) / previous.abs()) * 100.0
        } else if current > 0.0 {
            100.0
        } else if current < 0.0 {
            -100.0
        } else {
            0.0
        };

        let direction = if change_percent > 5.0 {
            TrendDirection::Up
        } else if change_percent < -5.0 {
            TrendDirection::Down
        } else {
            TrendDirection::Stable
        };

        Self {
            current,
            previous,
            change_percent,
            direction,
        }
    }
}

/// Analytics service for querying and analyzing operational data.
pub struct AnalyticsService<'a> {
    tracker: &'a dyn FixAttemptTracker,
}

impl<'a> AnalyticsService<'a> {
    /// Create a new analytics service.
    pub fn new(tracker: &'a dyn FixAttemptTracker) -> Self {
        Self { tracker }
    }

    /// Get the overall analytics summary.
    pub fn get_summary(&self) -> Result<AnalyticsSummary> {
        self.tracker.get_analytics_summary(crate::AttemptScope::All)
    }

    /// Get the success rate over a time period.
    pub fn get_success_rate(&self) -> Result<f64> {
        self.tracker.get_success_rate()
    }

    /// Get recent metrics for a given metric name.
    pub fn get_recent_metrics(
        &self,
        metric_name: &str,
        period: TimePeriod,
    ) -> Result<Vec<ProcessingMetric>> {
        let since = Some(period.start_time());
        self.tracker.get_metrics(metric_name, since, 1000)
    }

    /// Get the most common error patterns.
    pub fn get_top_errors(&self, limit: usize) -> Result<Vec<ErrorPattern>> {
        self.tracker.get_error_patterns(limit)
    }

    /// Calculate the average metric value over a time period.
    pub fn average_metric(&self, metric_name: &str, period: TimePeriod) -> Result<Option<f64>> {
        let metrics = self.get_recent_metrics(metric_name, period)?;
        if metrics.is_empty() {
            return Ok(None);
        }

        let sum: f64 = metrics.iter().map(|m| m.metric_value).sum();
        Ok(Some(sum / metrics.len() as f64))
    }

    /// Analyze the trend for a metric by comparing two time periods.
    pub fn analyze_metric_trend(
        &self,
        metric_name: &str,
        period: TimePeriod,
    ) -> Result<Option<TrendAnalysis>> {
        let now = Utc::now();
        let period_duration = period.duration();

        // Current period
        let current_start = now - period_duration;
        let current_metrics = self
            .tracker
            .get_metrics(metric_name, Some(current_start), 1000)?;

        // Previous period
        let previous_start = current_start - period_duration;
        let previous_metrics = self
            .tracker
            .get_metrics(metric_name, Some(previous_start), 1000)?;

        // Filter previous metrics to only include those before current period
        let previous_metrics: Vec<_> = previous_metrics
            .into_iter()
            .filter(|m| m.timestamp < current_start)
            .collect();

        if current_metrics.is_empty() && previous_metrics.is_empty() {
            return Ok(None);
        }

        let current_avg = if current_metrics.is_empty() {
            0.0
        } else {
            current_metrics.iter().map(|m| m.metric_value).sum::<f64>()
                / current_metrics.len() as f64
        };

        let previous_avg = if previous_metrics.is_empty() {
            0.0
        } else {
            previous_metrics.iter().map(|m| m.metric_value).sum::<f64>()
                / previous_metrics.len() as f64
        };

        Ok(Some(TrendAnalysis::new(current_avg, previous_avg)))
    }

    /// Get metrics aggregated by source.
    pub fn metrics_by_source(
        &self,
        metric_name: &str,
        period: TimePeriod,
    ) -> Result<HashMap<String, f64>> {
        let metrics = self.get_recent_metrics(metric_name, period)?;

        let mut by_source: HashMap<String, Vec<f64>> = HashMap::new();
        for metric in metrics {
            if let Some(source) = metric.source {
                by_source
                    .entry(source)
                    .or_default()
                    .push(metric.metric_value);
            }
        }

        let mut result = HashMap::new();
        for (source, values) in by_source {
            if !values.is_empty() {
                let avg = values.iter().sum::<f64>() / values.len() as f64;
                result.insert(source, avg);
            }
        }

        Ok(result)
    }

    /// Calculate throughput (issues processed per hour) over a time period.
    pub fn calculate_throughput(&self, period: TimePeriod) -> Result<f64> {
        let activities = self.tracker.get_recent_activities(10000)?;

        let start_time = period.start_time();
        let processing_count = activities
            .iter()
            .filter(|a| {
                a.timestamp >= start_time
                    && (a.activity_type == "processing_completed"
                        || a.activity_type == "pr_created")
            })
            .count();

        let hours = period.duration().num_hours() as f64;
        if hours > 0.0 {
            Ok(processing_count as f64 / hours)
        } else {
            Ok(0.0)
        }
    }

    /// Get error rate (errors per total processing) over a time period.
    pub fn calculate_error_rate(&self, period: TimePeriod) -> Result<f64> {
        let activities = self.tracker.get_recent_activities(10000)?;
        let start_time = period.start_time();

        let total = activities
            .iter()
            .filter(|a| {
                a.timestamp >= start_time
                    && (a.activity_type == "processing_completed"
                        || a.activity_type == "error"
                        || a.activity_type == "pr_created")
            })
            .count();

        let errors = activities
            .iter()
            .filter(|a| a.timestamp >= start_time && a.activity_type == "error")
            .count();

        if total > 0 {
            Ok(errors as f64 / total as f64)
        } else {
            Ok(0.0)
        }
    }
}

/// Helper function to compute a hash for error message normalization.
///
/// Normalises by lowercasing, collapsing each run of digits to the
/// placeholder `<N>` (so "port 8080" and "port 3000" both become
/// "port <N>"), and collapsing whitespace.
pub fn compute_error_hash(error_message: &str) -> String {
    use sha2::{Digest, Sha256};

    // Normalize the error message:
    // 1. Convert to lowercase
    // 2. Replace each contiguous run of digits with `<N>` (preserves structure)
    // 3. Collapse extra whitespace
    let lower = error_message.to_lowercase();
    let mut collapsed = String::with_capacity(lower.len());
    let mut in_digits = false;
    for ch in lower.chars() {
        if ch.is_numeric() {
            if !in_digits {
                collapsed.push_str("<N>");
                in_digits = true;
            }
        } else {
            in_digits = false;
            collapsed.push(ch);
        }
    }
    let normalized: String = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");

    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hex::encode(&hasher.finalize()[..16]) // Use first 16 bytes for shorter hash
}

/// Classify an error message into an error type.
///
/// Categories are checked in priority order. More specific patterns (e.g.
/// "timeout", "git") are checked before broader ones (e.g. "test", "api").
pub fn classify_error(error_message: &str) -> &'static str {
    let lower = error_message.to_lowercase();

    if lower.contains("timeout") || lower.contains("timed out") {
        "timeout"
    } else if lower.contains("build") || lower.contains("compile") || lower.contains("cargo") {
        "build_failure"
    } else if lower.contains("git") || lower.contains("merge") || lower.contains("conflict") {
        "git_error"
    } else if lower.contains("test") || lower.contains("assertion") {
        "test_failure"
    } else if lower.contains("claude") || lower.contains("rate limit") {
        "claude_error"
    } else if lower.contains("permission") || lower.contains("access denied") {
        "permission_error"
    } else if lower.contains("network") || lower.contains("connection") {
        "network_error"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttemptTracker, SqliteTracker};

    #[test]
    fn test_time_period_duration() {
        assert_eq!(TimePeriod::Hour.duration().num_hours(), 1);
        assert_eq!(TimePeriod::Day.duration().num_days(), 1);
        assert_eq!(TimePeriod::Week.duration().num_days(), 7);
        assert_eq!(TimePeriod::Month.duration().num_days(), 30);
    }

    #[test]
    fn test_trend_analysis_up() {
        let trend = TrendAnalysis::new(120.0, 100.0);
        assert_eq!(trend.direction, TrendDirection::Up);
        assert!((trend.change_percent - 20.0).abs() < 0.01);
    }

    #[test]
    fn test_trend_analysis_down() {
        let trend = TrendAnalysis::new(80.0, 100.0);
        assert_eq!(trend.direction, TrendDirection::Down);
        assert!((trend.change_percent - (-20.0)).abs() < 0.01);
    }

    #[test]
    fn test_trend_analysis_stable() {
        let trend = TrendAnalysis::new(101.0, 100.0);
        assert_eq!(trend.direction, TrendDirection::Stable);
    }

    #[test]
    fn test_trend_analysis_from_zero() {
        let trend = TrendAnalysis::new(100.0, 0.0);
        assert_eq!(trend.direction, TrendDirection::Up);
        assert_eq!(trend.change_percent, 100.0);
    }

    #[test]
    fn test_compute_error_hash() {
        let hash1 = compute_error_hash("Error at line 42: undefined variable");
        let hash2 = compute_error_hash("Error at line 100: undefined variable");
        // Should produce the same hash (numbers removed)
        assert_eq!(hash1, hash2);

        let hash3 = compute_error_hash("Different error message");
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_classify_error() {
        assert_eq!(
            classify_error("Process timed out after 60 seconds"),
            "timeout"
        );
        assert_eq!(
            classify_error("Build failed: cargo build error"),
            "build_failure"
        );
        assert_eq!(classify_error("Test assertion failed"), "test_failure");
        assert_eq!(
            classify_error("Claude API rate limit exceeded"),
            "claude_error"
        );
        assert_eq!(classify_error("Git merge conflict"), "git_error");
        assert_eq!(classify_error("Permission denied"), "permission_error");
        assert_eq!(
            classify_error("Network connection refused"),
            "network_error"
        );
        assert_eq!(classify_error("Some random error"), "unknown");
    }

    #[test]
    fn test_analytics_service_creation() {
        let tracker = SqliteTracker::in_memory().unwrap();
        let _service = AnalyticsService::new(&tracker);
    }

    #[test]
    fn test_analytics_summary() {
        let tracker = SqliteTracker::in_memory().unwrap();
        let service = AnalyticsService::new(&tracker);

        let summary = service.get_summary().unwrap();
        assert_eq!(summary.total_processed, 0);
        assert_eq!(summary.success_rate, 0.0);
    }

    #[test]
    fn test_classify_error_timeout_beats_test_failure() {
        // "test timeout" contains both "test" and "timeout".
        // timeout is checked first, so it wins.
        assert_eq!(
            classify_error("test timeout after 60s"),
            "timeout",
            "timeout should take priority over test_failure"
        );
    }

    #[test]
    fn test_classify_error_build_beats_test() {
        // "build test failed" contains both "build" and "test".
        // build_failure is checked before test_failure.
        assert_eq!(
            classify_error("build test failed"),
            "build_failure",
            "build_failure should take priority over test_failure"
        );
    }

    #[test]
    fn test_classify_error_compile_beats_test() {
        // "compile test error" contains both "compile" and "test".
        assert_eq!(
            classify_error("compile test error"),
            "build_failure",
            "compile (build_failure) should take priority over test"
        );
    }

    #[test]
    fn test_classify_error_git_beats_test() {
        // "git merge conflict in test file" contains "git", "merge", "conflict", AND "test".
        // git_error is now checked before test_failure in the priority order.
        let result = classify_error("git merge conflict in test file");
        assert_eq!(
            result, "git_error",
            "'git merge conflict in test file' should be classified as git_error"
        );
    }

    #[test]
    fn test_classify_error_connection_timeout() {
        // "connection timeout" contains both "timeout" and "connection".
        // timeout is checked first, so it wins over network_error.
        assert_eq!(
            classify_error("connection timeout"),
            "timeout",
            "timeout should take priority over network_error (connection)"
        );
    }

    #[test]
    fn test_classify_error_sentry_api_not_misclassified() {
        // "Sentry API error" should not be classified as claude_error.
        // The claude_error branch now matches "claude" or "claude api" or "rate limit",
        // not the generic keyword "api".
        let result = classify_error("Sentry API error");
        assert_eq!(
            result, "unknown",
            "'Sentry API error' should be unknown — it is not a Claude error"
        );
    }

    #[test]
    fn test_classify_error_rate_limit_without_api() {
        // "rate limit exceeded" alone matches "rate limit" in the claude_error branch.
        assert_eq!(classify_error("rate limit exceeded"), "claude_error");
    }

    #[test]
    fn test_classify_error_empty_string() {
        assert_eq!(classify_error(""), "unknown");
    }

    #[test]
    fn test_classify_error_whitespace_only() {
        assert_eq!(classify_error("   \t\n  "), "unknown");
    }

    #[test]
    fn test_classify_error_case_insensitive() {
        assert_eq!(classify_error("TIMEOUT"), "timeout");
        assert_eq!(classify_error("BUILD FAILURE"), "build_failure");
        assert_eq!(classify_error("Test Failed"), "test_failure");
        assert_eq!(classify_error("GIT ERROR"), "git_error");
    }

    #[test]
    fn test_classify_error_timed_out_variant() {
        assert_eq!(classify_error("the process timed out"), "timeout");
    }

    #[test]
    fn test_classify_error_cargo_is_build_failure() {
        assert_eq!(
            classify_error("cargo build returned exit code 1"),
            "build_failure"
        );
    }

    #[test]
    fn test_classify_error_assertion_is_test_failure() {
        assert_eq!(
            classify_error("assertion failed: expected 5 got 3"),
            "test_failure"
        );
    }

    #[test]
    fn test_classify_error_access_denied() {
        assert_eq!(classify_error("access denied for user"), "permission_error");
    }

    #[test]
    fn test_classify_error_merge_without_git() {
        // "merge conflict" without "git" — still matches git_error via "merge" keyword.
        // But "test" is checked before "merge". If the message is just "merge conflict",
        // there's no "test" keyword, so it falls through to git_error.
        assert_eq!(classify_error("merge conflict in file.rs"), "git_error");
    }

    #[test]
    fn test_classify_error_conflict_without_git_or_merge() {
        // "conflict" alone matches git_error.
        assert_eq!(classify_error("conflict detected"), "git_error");
    }

    #[test]
    fn test_compute_error_hash_http_status_codes_differ() {
        // "HTTP 404 Not Found" vs "HTTP 500 Internal Error"
        // After digit removal: "http  not found" vs "http  internal error"
        // After whitespace normalization: "http not found" vs "http internal error"
        // These SHOULD produce different hashes (and they do, because the text differs).
        let hash_404 = compute_error_hash("HTTP 404 Not Found");
        let hash_500 = compute_error_hash("HTTP 500 Internal Error");
        assert_ne!(
            hash_404, hash_500,
            "HTTP 404 and HTTP 500 should produce different hashes \
             because the textual parts differ after digit removal"
        );
    }

    #[test]
    fn test_compute_error_hash_port_numbers_collapse() {
        // "Error at port 8080" and "Error at port 3000"
        // Both become "error at port <N>" after normalization.
        // This is intentional: the port number is variable and we want to
        // group structurally identical errors together.
        let hash1 = compute_error_hash("Error at port 8080");
        let hash2 = compute_error_hash("Error at port 3000");
        assert_eq!(
            hash1, hash2,
            "Different port numbers should hash the same (digit runs are collapsed to <N>)"
        );
    }

    #[test]
    fn test_compute_error_hash_version_numbers_cause_collision() {
        // "version 1.2.3" and "version 4.5.6" both become "version ..."
        let hash1 = compute_error_hash("version 1.2.3");
        let hash2 = compute_error_hash("version 4.5.6");
        assert_eq!(
            hash1, hash2,
            "Different version numbers collide because digits are stripped"
        );
    }

    #[test]
    fn test_compute_error_hash_empty_string() {
        // Should produce a valid 32-char hex hash, not panic.
        let hash = compute_error_hash("");
        assert_eq!(
            hash.len(),
            32,
            "Hash should be 32 hex characters (16 bytes)"
        );
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash should contain only hex digits"
        );
    }

    #[test]
    fn test_compute_error_hash_only_numbers() {
        // "42" -> digits collapsed to "<N>" -> hash of "<N>"
        let hash_numbers = compute_error_hash("42");
        let hash_empty = compute_error_hash("");
        assert_ne!(
            hash_numbers, hash_empty,
            "A digits-only string becomes '<N>' which differs from the empty-string hash"
        );
        // All digit-only strings should hash the same (they all become "<N>")
        let hash_99 = compute_error_hash("99");
        assert_eq!(hash_numbers, hash_99);
    }

    #[test]
    fn test_compute_error_hash_very_long_message() {
        let long_message = "a]".repeat(10_000);
        let hash = compute_error_hash(&long_message);
        assert_eq!(hash.len(), 32, "Hash should always be 32 hex characters");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_compute_error_hash_unicode() {
        let hash = compute_error_hash("错误: 连接失败 🔥");
        assert_eq!(hash.len(), 32);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

        // Different unicode messages should produce different hashes.
        let hash2 = compute_error_hash("エラー: 接続に失敗しました");
        assert_ne!(hash, hash2);
    }

    #[test]
    fn test_compute_error_hash_whitespace_normalization() {
        // Extra whitespace should be collapsed.
        let hash1 = compute_error_hash("error   in    file");
        let hash2 = compute_error_hash("error in file");
        assert_eq!(
            hash1, hash2,
            "Extra whitespace should be normalized to single spaces"
        );
    }

    #[test]
    fn test_compute_error_hash_case_insensitive() {
        let hash1 = compute_error_hash("Error Message");
        let hash2 = compute_error_hash("error message");
        assert_eq!(hash1, hash2, "Hashing should be case-insensitive");
    }

    #[test]
    fn test_compute_error_hash_leading_trailing_whitespace() {
        let hash1 = compute_error_hash("  error message  ");
        let hash2 = compute_error_hash("error message");
        assert_eq!(
            hash1, hash2,
            "Leading/trailing whitespace should be normalized away"
        );
    }

    #[test]
    fn test_compute_error_hash_only_whitespace() {
        // Whitespace-only string normalizes to empty.
        let hash = compute_error_hash("   \t\n   ");
        let hash_empty = compute_error_hash("");
        assert_eq!(
            hash, hash_empty,
            "Whitespace-only string should hash the same as empty string"
        );
    }

    #[test]
    fn test_trend_analysis_both_zero() {
        let trend = TrendAnalysis::new(0.0, 0.0);
        assert_eq!(trend.change_percent, 0.0);
        assert_eq!(trend.direction, TrendDirection::Stable);
    }

    #[test]
    fn test_trend_analysis_negative_current() {
        // current=-10, previous=100
        // change_percent = ((-10) - 100) / 100 * 100 = -110%
        let trend = TrendAnalysis::new(-10.0, 100.0);
        assert!(
            (trend.change_percent - (-110.0)).abs() < 0.01,
            "change_percent should be -110%, got {}",
            trend.change_percent
        );
        assert_eq!(trend.direction, TrendDirection::Down);
    }

    #[test]
    fn test_trend_analysis_both_negative_improvement() {
        // current=-50, previous=-100
        // The value improved from -100 to -50 (less negative).
        // change = (-50 - (-100)) / |-100| * 100 = 50%  => Up
        let trend = TrendAnalysis::new(-50.0, -100.0);
        assert!(
            (trend.change_percent - 50.0).abs() < 0.01,
            "Both-negative improvement: change should be +50%, got {}",
            trend.change_percent
        );
        assert_eq!(trend.direction, TrendDirection::Up);
    }

    #[test]
    fn test_trend_analysis_current_zero_previous_positive() {
        // current=0, previous=100 => change = -100%
        let trend = TrendAnalysis::new(0.0, 100.0);
        assert!((trend.change_percent - (-100.0)).abs() < 0.01);
        assert_eq!(trend.direction, TrendDirection::Down);
    }

    #[test]
    fn test_trend_analysis_current_zero_previous_negative() {
        // current=0, previous=-100
        // change = (0 - (-100)) / |-100| * 100 = 100% => Up
        let trend = TrendAnalysis::new(0.0, -100.0);
        assert!(
            (trend.change_percent - 100.0).abs() < 0.01,
            "Going from -100 to 0 should be +100%, got {}",
            trend.change_percent
        );
        assert_eq!(trend.direction, TrendDirection::Up);
    }

    #[test]
    fn test_trend_analysis_boundary_exactly_5_percent_up() {
        // 5% change exactly: current=105, previous=100
        // change = (105-100)/100 * 100 = 5.0
        // direction: 5.0 > 5.0 is false, so Stable
        let trend = TrendAnalysis::new(105.0, 100.0);
        assert!((trend.change_percent - 5.0).abs() < 0.01);
        assert_eq!(
            trend.direction,
            TrendDirection::Stable,
            "Exactly 5.0% should be Stable (need > 5.0 for Up)"
        );
    }

    #[test]
    fn test_trend_analysis_boundary_just_over_5_percent_up() {
        // 5.01% change: current=105.01, previous=100
        let trend = TrendAnalysis::new(105.01, 100.0);
        assert!(trend.change_percent > 5.0);
        assert_eq!(
            trend.direction,
            TrendDirection::Up,
            "Just over 5% should be Up"
        );
    }

    #[test]
    fn test_trend_analysis_boundary_exactly_minus_5_percent() {
        // -5% change exactly: current=95, previous=100
        // change = (95-100)/100 * 100 = -5.0
        // direction: -5.0 < -5.0 is false, so Stable
        let trend = TrendAnalysis::new(95.0, 100.0);
        assert!((trend.change_percent - (-5.0)).abs() < 0.01);
        assert_eq!(
            trend.direction,
            TrendDirection::Stable,
            "Exactly -5.0% should be Stable (need < -5.0 for Down)"
        );
    }

    #[test]
    fn test_trend_analysis_boundary_just_under_minus_5_percent() {
        // current=94.99, previous=100 => -5.01%
        let trend = TrendAnalysis::new(94.99, 100.0);
        assert!(trend.change_percent < -5.0);
        assert_eq!(trend.direction, TrendDirection::Down);
    }

    #[test]
    fn test_trend_analysis_very_large_values() {
        let trend = TrendAnalysis::new(1e15, 1e14);
        // change = (1e15 - 1e14) / 1e14 * 100 = 9e14 / 1e14 * 100 = 900%
        assert!((trend.change_percent - 900.0).abs() < 0.01);
        assert_eq!(trend.direction, TrendDirection::Up);
        assert!(trend.change_percent.is_finite());
    }

    #[test]
    fn test_trend_analysis_very_small_previous() {
        // previous very close to zero but positive => huge percentage change
        let trend = TrendAnalysis::new(100.0, 0.0001);
        // change = (100 - 0.0001) / 0.0001 * 100 = ~99999900%
        assert!(trend.change_percent > 1_000_000.0);
        assert_eq!(trend.direction, TrendDirection::Up);
        assert!(trend.change_percent.is_finite());
    }

    #[test]
    fn test_trend_analysis_nan_propagation() {
        // NaN inputs: NaN != 0.0 is true, so we enter the percentage formula.
        // (NaN - NaN) / NaN.abs() = NaN. NaN comparisons (> 5, < -5) are false.
        let trend = TrendAnalysis::new(f64::NAN, f64::NAN);
        assert!(trend.change_percent.is_nan());
        // NaN > 5.0 and NaN < -5.0 are both false, so direction = Stable
        assert_eq!(trend.direction, TrendDirection::Stable);
    }

    #[test]
    fn test_trend_analysis_infinity() {
        let trend = TrendAnalysis::new(f64::INFINITY, 100.0);
        // change = (INF - 100) / 100 * 100 = INF
        assert!(trend.change_percent.is_infinite());
        // INF > 5.0 is true
        assert_eq!(trend.direction, TrendDirection::Up);
    }

    #[test]
    fn test_trend_analysis_negative_infinity() {
        let trend = TrendAnalysis::new(f64::NEG_INFINITY, 100.0);
        assert!(trend.change_percent.is_infinite());
        assert_eq!(trend.direction, TrendDirection::Down);
    }

    #[test]
    fn test_trend_analysis_previous_zero_current_negative() {
        // previous=0.0, current=-5.0
        // previous is 0, current < 0 => change_percent = -100%
        let trend = TrendAnalysis::new(-5.0, 0.0);
        assert!(
            (trend.change_percent - (-100.0)).abs() < 0.01,
            "Going from 0 to -5 should be -100%, got {}",
            trend.change_percent
        );
        assert_eq!(trend.direction, TrendDirection::Down);
    }

    #[test]
    fn test_time_period_start_time_is_in_the_past() {
        let now = Utc::now();
        for period in &[
            TimePeriod::Hour,
            TimePeriod::Day,
            TimePeriod::Week,
            TimePeriod::Month,
        ] {
            let start = period.start_time();
            assert!(
                start < now,
                "{:?}.start_time() should be in the past",
                period
            );
        }
    }

    #[test]
    fn test_time_period_start_time_approximately_correct() {
        let now = Utc::now();

        let hour_start = TimePeriod::Hour.start_time();
        let diff = now - hour_start;
        // Should be approximately 1 hour (within a few seconds of test execution)
        assert!(
            (diff.num_seconds() - 3600).abs() < 5,
            "Hour start_time should be ~3600s ago, was {}s",
            diff.num_seconds()
        );

        let day_start = TimePeriod::Day.start_time();
        let diff = now - day_start;
        assert!(
            (diff.num_seconds() - 86400).abs() < 5,
            "Day start_time should be ~86400s ago, was {}s",
            diff.num_seconds()
        );
    }

    #[test]
    fn test_time_period_duration_values() {
        assert_eq!(TimePeriod::Hour.duration().num_seconds(), 3600);
        assert_eq!(TimePeriod::Day.duration().num_seconds(), 86400);
        assert_eq!(TimePeriod::Week.duration().num_seconds(), 604800);
        assert_eq!(TimePeriod::Month.duration().num_seconds(), 2592000); // 30 * 86400
    }

    #[test]
    fn test_success_rate_with_mixed_attempts() {
        let tracker = SqliteTracker::in_memory().unwrap();

        // Record 4 attempts: 2 success, 1 failed, 1 merged
        tracker
            .record_attempt("test_source", "issue-1", "TST-1")
            .unwrap();
        tracker
            .mark_success("test_source", "issue-1", "https://github.com/pr/1")
            .unwrap();

        tracker
            .record_attempt("test_source", "issue-2", "TST-2")
            .unwrap();
        tracker
            .mark_failed("test_source", "issue-2", "build error")
            .unwrap();

        tracker
            .record_attempt("test_source", "issue-3", "TST-3")
            .unwrap();
        tracker
            .mark_success("test_source", "issue-3", "https://github.com/pr/3")
            .unwrap();
        tracker.mark_merged("test_source", "issue-3").unwrap();

        tracker
            .record_attempt("test_source", "issue-4", "TST-4")
            .unwrap();
        tracker
            .mark_success("test_source", "issue-4", "https://github.com/pr/4")
            .unwrap();

        let service = AnalyticsService::new(&tracker);
        let rate = service.get_success_rate().unwrap();
        // 3 successes (issue-1 success, issue-3 merged, issue-4 success) out of 4 total = 0.75
        assert!(
            (rate - 0.75).abs() < 0.01,
            "Success rate should be 0.75 (3/4), got {}",
            rate
        );
    }

    #[test]
    fn test_success_rate_all_failures() {
        let tracker = SqliteTracker::in_memory().unwrap();

        for i in 0..5 {
            let issue_id = format!("issue-{}", i);
            let short_id = format!("TST-{}", i);
            tracker.record_attempt("src", &issue_id, &short_id).unwrap();
            tracker.mark_failed("src", &issue_id, "some error").unwrap();
        }

        let service = AnalyticsService::new(&tracker);
        let rate = service.get_success_rate().unwrap();
        assert!(
            rate.abs() < 0.001,
            "Success rate should be 0.0 when all attempts failed, got {}",
            rate
        );
    }

    #[test]
    fn test_success_rate_empty_db() {
        let tracker = SqliteTracker::in_memory().unwrap();
        let service = AnalyticsService::new(&tracker);
        let rate = service.get_success_rate().unwrap();
        assert_eq!(rate, 0.0, "Empty DB should have 0.0 success rate");
    }

    #[test]
    fn test_top_errors_with_multiple_patterns() {
        let tracker = SqliteTracker::in_memory().unwrap();

        // Insert error patterns with different occurrence counts.
        let mut pattern1 = ErrorPattern::new("hash-a");
        pattern1.error_type = Some("build_failure".to_string());
        pattern1.error_message = Some("cargo build failed".to_string());
        pattern1.occurrence_count = 10;
        tracker.record_error_pattern(&pattern1).unwrap();

        let mut pattern2 = ErrorPattern::new("hash-b");
        pattern2.error_type = Some("timeout".to_string());
        pattern2.error_message = Some("process timed out".to_string());
        pattern2.occurrence_count = 5;
        tracker.record_error_pattern(&pattern2).unwrap();

        let mut pattern3 = ErrorPattern::new("hash-c");
        pattern3.error_type = Some("test_failure".to_string());
        pattern3.error_message = Some("assertion failed".to_string());
        pattern3.occurrence_count = 20;
        tracker.record_error_pattern(&pattern3).unwrap();

        let service = AnalyticsService::new(&tracker);
        let top = service.get_top_errors(2).unwrap();

        assert_eq!(top.len(), 2, "Should return at most 2 error patterns");
        // Ordered by occurrence_count DESC
        assert_eq!(
            top[0].error_type.as_deref(),
            Some("test_failure"),
            "Most frequent error should be first"
        );
        assert_eq!(top[0].occurrence_count, 20);
        assert_eq!(top[1].error_type.as_deref(), Some("build_failure"));
        assert_eq!(top[1].occurrence_count, 10);
    }

    #[test]
    fn test_top_errors_empty_db() {
        let tracker = SqliteTracker::in_memory().unwrap();
        let service = AnalyticsService::new(&tracker);
        let top = service.get_top_errors(10).unwrap();
        assert!(top.is_empty());
    }

    #[test]
    fn test_average_metric_no_metrics() {
        let tracker = SqliteTracker::in_memory().unwrap();
        let service = AnalyticsService::new(&tracker);
        let avg = service
            .average_metric("nonexistent_metric", TimePeriod::Day)
            .unwrap();
        assert_eq!(avg, None, "No metrics should return None");
    }

    #[test]
    fn test_average_metric_with_data() {
        let tracker = SqliteTracker::in_memory().unwrap();

        // Record some metrics.
        let m1 = ProcessingMetric::new("latency", 100.0);
        let m2 = ProcessingMetric::new("latency", 200.0);
        let m3 = ProcessingMetric::new("latency", 300.0);
        tracker.record_metric(&m1).unwrap();
        tracker.record_metric(&m2).unwrap();
        tracker.record_metric(&m3).unwrap();

        let service = AnalyticsService::new(&tracker);
        let avg = service.average_metric("latency", TimePeriod::Day).unwrap();
        assert!(avg.is_some());
        let avg_val = avg.unwrap();
        assert!(
            (avg_val - 200.0).abs() < 0.01,
            "Average of 100, 200, 300 should be 200.0, got {}",
            avg_val
        );
    }

    #[test]
    fn test_average_metric_wrong_name_returns_none() {
        let tracker = SqliteTracker::in_memory().unwrap();
        let m = ProcessingMetric::new("latency", 100.0);
        tracker.record_metric(&m).unwrap();

        let service = AnalyticsService::new(&tracker);
        let avg = service
            .average_metric("throughput", TimePeriod::Day)
            .unwrap();
        assert_eq!(
            avg, None,
            "Querying a different metric name should return None"
        );
    }

    #[test]
    fn test_calculate_throughput_empty_db() {
        let tracker = SqliteTracker::in_memory().unwrap();
        let service = AnalyticsService::new(&tracker);
        let throughput = service.calculate_throughput(TimePeriod::Hour).unwrap();
        assert!(
            throughput.abs() < 0.001,
            "Empty DB should have 0.0 throughput, got {}",
            throughput
        );
    }

    #[test]
    fn test_calculate_throughput_with_activities() {
        use claudear_core::types::ActivityLogEntry;
        let tracker = SqliteTracker::in_memory().unwrap();

        // Record some processing_completed activities (timestamped now, within Hour).
        for i in 0..6 {
            let entry =
                ActivityLogEntry::new("processing_completed", format!("Processed issue {}", i));
            tracker.record_activity(&entry).unwrap();
        }
        // Also record a pr_created activity (also counts for throughput).
        let pr_entry = ActivityLogEntry::new("pr_created", "PR created");
        tracker.record_activity(&pr_entry).unwrap();

        // Record an unrelated activity (should not count).
        let other = ActivityLogEntry::new("error", "something went wrong");
        tracker.record_activity(&other).unwrap();

        let service = AnalyticsService::new(&tracker);
        let throughput = service.calculate_throughput(TimePeriod::Hour).unwrap();
        // 7 relevant activities / 1 hour = 7.0
        assert!(
            (throughput - 7.0).abs() < 0.01,
            "Throughput should be 7.0/hr, got {}",
            throughput
        );
    }

    #[test]
    fn test_calculate_error_rate_empty_db() {
        let tracker = SqliteTracker::in_memory().unwrap();
        let service = AnalyticsService::new(&tracker);
        let rate = service.calculate_error_rate(TimePeriod::Hour).unwrap();
        assert!(
            rate.abs() < 0.001,
            "Empty DB should have 0.0 error rate, got {}",
            rate
        );
    }

    #[test]
    fn test_calculate_error_rate_with_mixed_activities() {
        use claudear_core::types::ActivityLogEntry;
        let tracker = SqliteTracker::in_memory().unwrap();

        // 3 processing_completed, 2 errors, 1 pr_created => total=6, errors=2 => rate=2/6
        for _ in 0..3 {
            let entry = ActivityLogEntry::new("processing_completed", "done");
            tracker.record_activity(&entry).unwrap();
        }
        for _ in 0..2 {
            let entry = ActivityLogEntry::new("error", "failed");
            tracker.record_activity(&entry).unwrap();
        }
        let entry = ActivityLogEntry::new("pr_created", "PR opened");
        tracker.record_activity(&entry).unwrap();

        let service = AnalyticsService::new(&tracker);
        let rate = service.calculate_error_rate(TimePeriod::Hour).unwrap();
        assert!(
            (rate - (2.0 / 6.0)).abs() < 0.01,
            "Error rate should be 2/6 = 0.333, got {}",
            rate
        );
    }

    #[test]
    fn test_calculate_error_rate_all_errors() {
        use claudear_core::types::ActivityLogEntry;
        let tracker = SqliteTracker::in_memory().unwrap();

        for _ in 0..5 {
            let entry = ActivityLogEntry::new("error", "everything broke");
            tracker.record_activity(&entry).unwrap();
        }

        let service = AnalyticsService::new(&tracker);
        let rate = service.calculate_error_rate(TimePeriod::Day).unwrap();
        assert!(
            (rate - 1.0).abs() < 0.01,
            "Error rate should be 1.0 when all activities are errors, got {}",
            rate
        );
    }

    #[test]
    fn test_analytics_summary_with_data() {
        let tracker = SqliteTracker::in_memory().unwrap();

        tracker
            .record_attempt("linear", "issue-1", "LIN-1")
            .unwrap();
        tracker
            .mark_success("linear", "issue-1", "https://github.com/pr/1")
            .unwrap();

        tracker
            .record_attempt("linear", "issue-2", "LIN-2")
            .unwrap();
        tracker
            .mark_failed("linear", "issue-2", "build failed")
            .unwrap();

        let service = AnalyticsService::new(&tracker);
        let summary = service.get_summary().unwrap();
        assert_eq!(summary.total_processed, 2);
        assert!((summary.success_rate - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_error_pattern_deduplication_via_hash() {
        // Recording the same pattern_hash twice should increment occurrence_count
        // rather than creating a duplicate row.
        let tracker = SqliteTracker::in_memory().unwrap();

        let hash = compute_error_hash("connection refused on port 443");
        let mut pattern = ErrorPattern::new(hash.clone());
        pattern.error_type = Some("network_error".to_string());
        pattern.error_message = Some("connection refused on port 443".to_string());
        pattern.occurrence_count = 1;
        tracker.record_error_pattern(&pattern).unwrap();

        // Record the same hash again (simulating a second occurrence).
        let mut pattern2 = ErrorPattern::new(hash);
        pattern2.error_type = Some("network_error".to_string());
        pattern2.error_message = Some("connection refused on port 8080".to_string());
        pattern2.occurrence_count = 1;
        tracker.record_error_pattern(&pattern2).unwrap();

        let service = AnalyticsService::new(&tracker);
        let top = service.get_top_errors(10).unwrap();
        // Should be 1 row (deduplicated), with occurrence_count incremented.
        assert_eq!(
            top.len(),
            1,
            "Same pattern_hash should be deduplicated into one row"
        );
        assert_eq!(
            top[0].occurrence_count, 2,
            "Occurrence count should be incremented on conflict"
        );
    }

    // Additional coverage: classify_error nuance, compute_error_hash
    // digit-run collapsing, TrendAnalysis symmetry, TimePeriod
    // ordering, AnalyticsService metrics_by_source, and throughput
    // with specific period.
    #[test]
    fn test_classify_error_network_without_connection() {
        assert_eq!(classify_error("network unreachable"), "network_error");
    }

    #[test]
    fn test_classify_error_claude_keyword() {
        assert_eq!(classify_error("Claude returned 500"), "claude_error");
    }

    #[test]
    fn test_classify_error_permission_keyword() {
        assert_eq!(
            classify_error("permission denied: /etc/shadow"),
            "permission_error"
        );
    }

    #[test]
    fn test_classify_error_build_without_compile() {
        assert_eq!(classify_error("build step 3 failed"), "build_failure");
    }

    #[test]
    fn test_classify_error_git_keyword_alone() {
        assert_eq!(classify_error("git push failed"), "git_error");
    }

    #[test]
    fn test_classify_error_conflict_alone() {
        // "conflict in Cargo.lock" contains "cargo" which matches build_failure
        // before "conflict" matches git_error (priority order)
        assert_eq!(classify_error("conflict in Cargo.lock"), "build_failure");
        // Pure "conflict" without "cargo" matches git_error
        assert_eq!(classify_error("conflict in file.rs"), "git_error");
    }

    #[test]
    fn test_classify_error_test_keyword_alone() {
        assert_eq!(classify_error("test suite failed"), "test_failure");
    }

    #[test]
    fn test_classify_error_assertion_alone() {
        assert_eq!(classify_error("assertion error in module"), "test_failure");
    }

    #[test]
    fn test_classify_error_unrecognized_punctuation() {
        assert_eq!(classify_error("!!!???..."), "unknown");
    }

    #[test]
    fn test_compute_error_hash_mixed_digits_and_text() {
        // "Error 42 on line 100" -> "error <N> on line <N>"
        // "Error 99 on line 200" -> "error <N> on line <N>"
        let hash1 = compute_error_hash("Error 42 on line 100");
        let hash2 = compute_error_hash("Error 99 on line 200");
        assert_eq!(
            hash1, hash2,
            "Digit runs should be collapsed: 'error <N> on line <N>'"
        );
    }

    #[test]
    fn test_compute_error_hash_adjacent_digit_runs() {
        // "12 34" -> "<N> <N>"
        // "56 78" -> "<N> <N>"
        let hash1 = compute_error_hash("12 34");
        let hash2 = compute_error_hash("56 78");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_compute_error_hash_single_digit_run() {
        // "error 1" and "error 99999" both become "error <N>"
        let hash1 = compute_error_hash("error 1");
        let hash2 = compute_error_hash("error 99999");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_compute_error_hash_no_digits_unchanged() {
        let hash1 = compute_error_hash("pure text error");
        let hash2 = compute_error_hash("pure text error");
        assert_eq!(hash1, hash2);

        let hash3 = compute_error_hash("different text error");
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_compute_error_hash_special_chars_preserved() {
        // Special characters are preserved in the normalization
        let hash1 = compute_error_hash("error: file not found!");
        let hash2 = compute_error_hash("error: something else!");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_compute_error_hash_tabs_and_newlines_as_whitespace() {
        let hash1 = compute_error_hash("error\tin\nfile");
        let hash2 = compute_error_hash("error in file");
        assert_eq!(
            hash1, hash2,
            "Tabs and newlines should be normalized to single spaces"
        );
    }

    #[test]
    fn test_trend_analysis_equal_values_stable() {
        let trend = TrendAnalysis::new(50.0, 50.0);
        assert_eq!(trend.change_percent, 0.0);
        assert_eq!(trend.direction, TrendDirection::Stable);
    }

    #[test]
    fn test_trend_analysis_negative_to_positive() {
        // current=50, previous=-100
        // change = (50 - (-100)) / |-100| * 100 = 150%
        let trend = TrendAnalysis::new(50.0, -100.0);
        assert!(
            (trend.change_percent - 150.0).abs() < 0.01,
            "Expected 150%, got {}",
            trend.change_percent
        );
        assert_eq!(trend.direction, TrendDirection::Up);
    }

    #[test]
    fn test_trend_analysis_positive_to_negative() {
        // current=-50, previous=100
        // change = (-50 - 100) / 100 * 100 = -150%
        let trend = TrendAnalysis::new(-50.0, 100.0);
        assert!(
            (trend.change_percent - (-150.0)).abs() < 0.01,
            "Expected -150%, got {}",
            trend.change_percent
        );
        assert_eq!(trend.direction, TrendDirection::Down);
    }

    #[test]
    fn test_trend_analysis_small_fractional_change_stable() {
        // 1% change should be stable (within 5% threshold)
        let trend = TrendAnalysis::new(101.0, 100.0);
        assert_eq!(trend.direction, TrendDirection::Stable);
        assert!((trend.change_percent - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_trend_analysis_fields_accessible() {
        let trend = TrendAnalysis::new(200.0, 100.0);
        assert_eq!(trend.current, 200.0);
        assert_eq!(trend.previous, 100.0);
        assert!((trend.change_percent - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_trend_direction_partial_eq() {
        assert_eq!(TrendDirection::Up, TrendDirection::Up);
        assert_eq!(TrendDirection::Down, TrendDirection::Down);
        assert_eq!(TrendDirection::Stable, TrendDirection::Stable);
        assert_ne!(TrendDirection::Up, TrendDirection::Down);
        assert_ne!(TrendDirection::Up, TrendDirection::Stable);
        assert_ne!(TrendDirection::Down, TrendDirection::Stable);
    }

    #[test]
    fn test_trend_direction_debug_format() {
        assert_eq!(format!("{:?}", TrendDirection::Up), "Up");
        assert_eq!(format!("{:?}", TrendDirection::Down), "Down");
        assert_eq!(format!("{:?}", TrendDirection::Stable), "Stable");
    }

    #[test]
    fn test_trend_direction_clone() {
        let dir = TrendDirection::Up;
        let cloned = dir;
        assert_eq!(dir, cloned);
    }

    #[test]
    fn test_time_period_duration_ordering() {
        // Hour < Day < Week < Month
        assert!(TimePeriod::Hour.duration() < TimePeriod::Day.duration());
        assert!(TimePeriod::Day.duration() < TimePeriod::Week.duration());
        assert!(TimePeriod::Week.duration() < TimePeriod::Month.duration());
    }

    #[test]
    fn test_time_period_start_time_ordering() {
        // Longer periods have earlier start times
        let hour_start = TimePeriod::Hour.start_time();
        let day_start = TimePeriod::Day.start_time();
        let week_start = TimePeriod::Week.start_time();
        let month_start = TimePeriod::Month.start_time();

        assert!(month_start < week_start);
        assert!(week_start < day_start);
        assert!(day_start < hour_start);
    }

    #[test]
    fn test_time_period_debug_format() {
        assert_eq!(format!("{:?}", TimePeriod::Hour), "Hour");
        assert_eq!(format!("{:?}", TimePeriod::Day), "Day");
        assert_eq!(format!("{:?}", TimePeriod::Week), "Week");
        assert_eq!(format!("{:?}", TimePeriod::Month), "Month");
    }

    #[test]
    fn test_time_period_clone() {
        let period = TimePeriod::Week;
        let cloned = period;
        assert_eq!(cloned.duration().num_days(), 7);
    }

    #[test]
    fn test_metrics_by_source_empty() {
        let tracker = SqliteTracker::in_memory().unwrap();
        let service = AnalyticsService::new(&tracker);
        let result = service
            .metrics_by_source("latency", TimePeriod::Day)
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_metrics_by_source_with_data() {
        let tracker = SqliteTracker::in_memory().unwrap();

        let mut m1 = ProcessingMetric::new("latency", 100.0);
        m1.source = Some("linear".to_string());
        let mut m2 = ProcessingMetric::new("latency", 200.0);
        m2.source = Some("linear".to_string());
        let mut m3 = ProcessingMetric::new("latency", 300.0);
        m3.source = Some("sentry".to_string());

        tracker.record_metric(&m1).unwrap();
        tracker.record_metric(&m2).unwrap();
        tracker.record_metric(&m3).unwrap();

        let service = AnalyticsService::new(&tracker);
        let result = service
            .metrics_by_source("latency", TimePeriod::Day)
            .unwrap();

        assert_eq!(result.len(), 2);
        let linear_avg = result.get("linear").unwrap();
        assert!(
            (*linear_avg - 150.0).abs() < 0.01,
            "linear average should be 150.0, got {}",
            linear_avg
        );
        let sentry_avg = result.get("sentry").unwrap();
        assert!(
            (*sentry_avg - 300.0).abs() < 0.01,
            "sentry average should be 300.0, got {}",
            sentry_avg
        );
    }

    #[test]
    fn test_metrics_by_source_no_source_field_excluded() {
        let tracker = SqliteTracker::in_memory().unwrap();

        // Metric without source field set
        let m = ProcessingMetric::new("latency", 100.0);
        tracker.record_metric(&m).unwrap();

        let service = AnalyticsService::new(&tracker);
        let result = service
            .metrics_by_source("latency", TimePeriod::Day)
            .unwrap();
        // Metrics without a source are excluded from by_source aggregation
        assert!(
            result.is_empty(),
            "Metrics without source should not appear in by_source"
        );
    }

    #[test]
    fn test_calculate_throughput_day_period() {
        use claudear_core::types::ActivityLogEntry;
        let tracker = SqliteTracker::in_memory().unwrap();

        for i in 0..24 {
            let entry =
                ActivityLogEntry::new("processing_completed", format!("Processed item {}", i));
            tracker.record_activity(&entry).unwrap();
        }

        let service = AnalyticsService::new(&tracker);
        let throughput = service.calculate_throughput(TimePeriod::Day).unwrap();
        // 24 activities / 24 hours = 1.0 per hour
        assert!(
            (throughput - 1.0).abs() < 0.01,
            "Throughput should be 1.0/hr for 24 items over 24 hours, got {}",
            throughput
        );
    }

    #[test]
    fn test_calculate_error_rate_no_errors() {
        use claudear_core::types::ActivityLogEntry;
        let tracker = SqliteTracker::in_memory().unwrap();

        for _ in 0..10 {
            let entry = ActivityLogEntry::new("processing_completed", "success");
            tracker.record_activity(&entry).unwrap();
        }

        let service = AnalyticsService::new(&tracker);
        let rate = service.calculate_error_rate(TimePeriod::Hour).unwrap();
        assert!(
            rate.abs() < 0.001,
            "Error rate should be 0.0 when no errors, got {}",
            rate
        );
    }

    #[test]
    fn test_average_metric_single_value() {
        let tracker = SqliteTracker::in_memory().unwrap();
        let m = ProcessingMetric::new("latency", 42.5);
        tracker.record_metric(&m).unwrap();

        let service = AnalyticsService::new(&tracker);
        let avg = service.average_metric("latency", TimePeriod::Day).unwrap();
        assert_eq!(avg, Some(42.5));
    }

    #[test]
    fn test_compute_error_hash_deterministic() {
        let msg = "fatal error: segmentation fault at address 0xDEADBEEF";
        let hash1 = compute_error_hash(msg);
        let hash2 = compute_error_hash(msg);
        assert_eq!(hash1, hash2, "Same input must produce same hash");
    }

    #[test]
    fn test_compute_error_hash_hex_only_output() {
        let hash = compute_error_hash("any arbitrary error message here");
        assert_eq!(hash.len(), 32);
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash should be hex-only, got: {}",
            hash
        );
    }

    #[test]
    fn test_trend_analysis_debug_format() {
        let trend = TrendAnalysis::new(110.0, 100.0);
        let debug = format!("{:?}", trend);
        assert!(debug.contains("TrendAnalysis"));
        assert!(debug.contains("current"));
        assert!(debug.contains("previous"));
        assert!(debug.contains("change_percent"));
        assert!(debug.contains("direction"));
    }

    #[test]
    fn test_trend_analysis_clone() {
        let trend = TrendAnalysis::new(50.0, 25.0);
        let cloned = trend.clone();
        assert_eq!(cloned.current, 50.0);
        assert_eq!(cloned.previous, 25.0);
        assert_eq!(cloned.direction, trend.direction);
        assert!((cloned.change_percent - trend.change_percent).abs() < 0.001);
    }
}
