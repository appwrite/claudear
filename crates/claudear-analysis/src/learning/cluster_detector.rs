//! System 8: Detect clusters of correlated issues arriving within a time window.

use chrono::{DateTime, Utc};
use claudear_core::error::Result;
use claudear_core::types::IssueCluster;
use claudear_storage::FixAttemptTracker;

pub struct ClusterDetector;

impl ClusterDetector {
    /// Detect issue arrival clusters within a time window.
    pub fn detect_clusters(
        tracker: &dyn FixAttemptTracker,
        source: &str,
        window_minutes: i64,
        min_cluster_size: usize,
    ) -> Result<Vec<IssueCluster>> {
        let arrivals = tracker.get_recent_issue_arrivals(source, window_minutes)?;

        if arrivals.len() < min_cluster_size {
            return Ok(Vec::new());
        }

        // Sliding window grouping
        let mut clusters = Vec::new();
        let window_duration = chrono::Duration::minutes(window_minutes);

        let mut i = 0;
        while i < arrivals.len() {
            let window_start = arrivals[i].1;
            let window_end = window_start + window_duration;

            let mut group: Vec<(String, DateTime<Utc>)> = Vec::new();
            let mut j = i;
            while j < arrivals.len() && arrivals[j].1 <= window_end {
                group.push(arrivals[j].clone());
                j += 1;
            }

            if group.len() >= min_cluster_size {
                let mut issue_ids: Vec<String> = group.iter().map(|(id, _)| id.clone()).collect();
                issue_ids.sort();
                let cluster_key = Self::compute_cluster_key(&issue_ids);

                let actual_end = group.last().map(|(_, t)| *t).unwrap_or(window_end);

                clusters.push(IssueCluster {
                    id: 0,
                    cluster_key,
                    source: source.to_string(),
                    issue_ids,
                    window_start,
                    window_end: actual_end,
                    resolved_by_issue_id: None,
                    resolved_by_attempt_id: None,
                    status: "active".to_string(),
                    created_at: Utc::now(),
                });

                // Skip past this cluster
                i = j;
            } else {
                i += 1;
            }
        }

        // Deduplicate by cluster_key
        let mut seen = std::collections::HashSet::new();
        clusters.retain(|c| seen.insert(c.cluster_key.clone()));

        Ok(clusters)
    }

    /// Check if fixing one issue in a cluster resolved the others.
    pub fn check_cluster_resolution(
        tracker: &dyn FixAttemptTracker,
        cluster: &IssueCluster,
    ) -> Result<bool> {
        // Check if any of the cluster's issues have been merged
        let mut resolved_count = 0;
        let mut _resolved_issue = None;

        for issue_id in &cluster.issue_ids {
            if let Ok(Some(attempt)) = tracker.get_attempt(&cluster.source, issue_id) {
                if attempt.status == claudear_core::types::FixAttemptStatus::Merged {
                    resolved_count += 1;
                    if _resolved_issue.is_none() {
                        _resolved_issue = Some(issue_id.clone());
                    }
                }
            }
        }

        // If at least one issue is resolved, the cluster might be resolved
        Ok(resolved_count > 0)
    }

    fn compute_cluster_key(sorted_issue_ids: &[String]) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        sorted_issue_ids.hash(&mut hasher);
        format!("cluster_{:016x}", hasher.finish())
    }

    /// Format cluster context for prompt injection.
    pub fn format_cluster_context(cluster: &IssueCluster) -> String {
        format!(
            "This issue arrived alongside {} other issues from the same source within {} minutes. \
             Related issue IDs: {}. These may share a common root cause.",
            cluster.issue_ids.len() - 1,
            (cluster.window_end - cluster.window_start).num_minutes(),
            cluster.issue_ids.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_storage::{
        ActivityStore, AttemptTracker, ChatStore, DiscordStore, EmbeddingStore, EvaluationStore,
        ExperimentStore, KnowledgeStore, RegressionStore, RepoStore, SimilarityStore, UserStore,
        WebhookStore,
    };
    use std::collections::HashMap;

    /// Simple mock that returns canned arrivals.
    struct MockTracker {
        arrivals: Vec<(String, DateTime<Utc>)>,
    }

    // Minimal FixAttemptTracker impl for testing
    impl AttemptTracker for MockTracker {
        fn has_attempted(&self, _: &str, _: &str) -> Result<bool> {
            Ok(false)
        }
        fn get_attempted_issue_ids(&self, _: &str) -> Result<std::collections::HashSet<String>> {
            Ok(std::collections::HashSet::new())
        }
        fn record_attempt(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn record_attempt_with_labels(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[String],
        ) -> Result<()> {
            Ok(())
        }
        fn mark_success(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn mark_failed(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn mark_merged(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn mark_closed(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn mark_resolved(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_attempt(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<claudear_core::types::FixAttempt>> {
            Ok(None)
        }
        fn get_attempts_by_status(
            &self,
            _: claudear_core::types::FixAttemptStatus,
        ) -> Result<Vec<claudear_core::types::FixAttempt>> {
            Ok(Vec::new())
        }
        fn get_pending_prs(&self) -> Result<Vec<claudear_core::types::FixAttempt>> {
            Ok(Vec::new())
        }
        fn get_attempt_by_pr_url(
            &self,
            _: &str,
        ) -> Result<Option<claudear_core::types::FixAttempt>> {
            Ok(None)
        }
        fn reset_attempt(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_stats(&self) -> Result<claudear_core::types::FixAttemptStats> {
            Ok(claudear_core::types::FixAttemptStats {
                total: 0,
                pending: 0,
                success: 0,
                failed: 0,
                merged: 0,
                closed: 0,
                cannot_fix: 0,
                by_source: HashMap::new(),
            })
        }
        fn increment_retry(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn mark_cannot_fix(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_retryable_issues(&self, _: u32) -> Result<Vec<claudear_core::types::FixAttempt>> {
            Ok(Vec::new())
        }
        fn prepare_for_retry(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
    }
    impl ActivityStore for MockTracker {}
    impl KnowledgeStore for MockTracker {
        fn get_recent_issue_arrivals(
            &self,
            _source: &str,
            _window_minutes: i64,
        ) -> Result<Vec<(String, DateTime<Utc>)>> {
            Ok(self.arrivals.clone())
        }
    }
    impl EmbeddingStore for MockTracker {}
    impl RepoStore for MockTracker {}
    impl UserStore for MockTracker {}
    impl ChatStore for MockTracker {}
    impl RegressionStore for MockTracker {}
    impl ExperimentStore for MockTracker {}
    impl EvaluationStore for MockTracker {}
    impl WebhookStore for MockTracker {}
    impl SimilarityStore for MockTracker {}
    impl DiscordStore for MockTracker {}

    #[test]
    fn test_detect_clusters_too_few() {
        let tracker = MockTracker {
            arrivals: vec![("issue-1".to_string(), Utc::now())],
        };
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 30, 3).unwrap();
        assert!(clusters.is_empty());
    }

    #[test]
    fn test_detect_clusters_found() {
        let now = Utc::now();
        let tracker = MockTracker {
            arrivals: vec![
                ("issue-1".to_string(), now),
                ("issue-2".to_string(), now + chrono::Duration::minutes(5)),
                ("issue-3".to_string(), now + chrono::Duration::minutes(10)),
            ],
        };
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 30, 3).unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].issue_ids.len(), 3);
    }

    #[test]
    fn test_format_cluster_context() {
        let now = Utc::now();
        let cluster = IssueCluster {
            id: 1,
            cluster_key: "test".to_string(),
            source: "sentry".to_string(),
            issue_ids: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            window_start: now,
            window_end: now + chrono::Duration::minutes(15),
            resolved_by_issue_id: None,
            resolved_by_attempt_id: None,
            status: "active".to_string(),
            created_at: now,
        };
        let ctx = ClusterDetector::format_cluster_context(&cluster);
        assert!(ctx.contains("2 other issues"));
        assert!(ctx.contains("15 minutes"));
    }

    #[test]
    fn test_detect_clusters_empty_arrivals() {
        let tracker = MockTracker { arrivals: vec![] };
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 30, 3).unwrap();
        assert!(clusters.is_empty());
    }

    #[test]
    fn test_detect_clusters_spread_out_no_cluster() {
        let now = Utc::now();
        let tracker = MockTracker {
            arrivals: vec![
                ("issue-1".to_string(), now),
                ("issue-2".to_string(), now + chrono::Duration::minutes(60)),
                ("issue-3".to_string(), now + chrono::Duration::minutes(120)),
            ],
        };
        // Window is 30 minutes, issues are 60 minutes apart -- no cluster
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 30, 3).unwrap();
        assert!(clusters.is_empty());
    }

    #[test]
    fn test_detect_clusters_two_separate_clusters() {
        let now = Utc::now();
        let tracker = MockTracker {
            arrivals: vec![
                // Cluster 1: 3 issues within 5 minutes
                ("issue-1".to_string(), now),
                ("issue-2".to_string(), now + chrono::Duration::minutes(2)),
                ("issue-3".to_string(), now + chrono::Duration::minutes(4)),
                // Gap of 2 hours
                // Cluster 2: 3 issues within 5 minutes
                ("issue-4".to_string(), now + chrono::Duration::minutes(120)),
                ("issue-5".to_string(), now + chrono::Duration::minutes(122)),
                ("issue-6".to_string(), now + chrono::Duration::minutes(124)),
            ],
        };
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 30, 3).unwrap();
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].issue_ids.len(), 3);
        assert_eq!(clusters[1].issue_ids.len(), 3);
    }

    #[test]
    fn test_cluster_key_stability() {
        // Same issue IDs should always produce the same cluster key
        let ids1 = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let ids2 = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let key1 = ClusterDetector::compute_cluster_key(&ids1);
        let key2 = ClusterDetector::compute_cluster_key(&ids2);
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_cluster_key_order_matters() {
        let ids_abc = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let ids_cba = vec!["c".to_string(), "b".to_string(), "a".to_string()];
        let key_abc = ClusterDetector::compute_cluster_key(&ids_abc);
        let key_cba = ClusterDetector::compute_cluster_key(&ids_cba);
        // Different order should produce different keys (they're sorted before computing)
        assert_ne!(key_abc, key_cba);
    }

    #[test]
    fn test_detect_clusters_deduplicates_by_key() {
        let now = Utc::now();
        // All 4 issues within the window — the sliding window might try to form overlapping clusters
        // but dedup by cluster_key should keep only unique ones
        let tracker = MockTracker {
            arrivals: vec![
                ("issue-1".to_string(), now),
                ("issue-2".to_string(), now + chrono::Duration::minutes(1)),
                ("issue-3".to_string(), now + chrono::Duration::minutes(2)),
                ("issue-4".to_string(), now + chrono::Duration::minutes(3)),
            ],
        };
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 30, 3).unwrap();
        // Should find at least one cluster
        assert!(!clusters.is_empty());
        // Issue IDs in the cluster should be sorted
        for cluster in &clusters {
            let mut sorted = cluster.issue_ids.clone();
            sorted.sort();
            assert_eq!(cluster.issue_ids, sorted);
        }
    }

    #[test]
    fn test_format_cluster_context_single_companion() {
        let now = Utc::now();
        let cluster = IssueCluster {
            id: 1,
            cluster_key: "test".to_string(),
            source: "sentry".to_string(),
            issue_ids: vec!["a".to_string(), "b".to_string()],
            window_start: now,
            window_end: now + chrono::Duration::minutes(5),
            resolved_by_issue_id: None,
            resolved_by_attempt_id: None,
            status: "active".to_string(),
            created_at: now,
        };
        let ctx = ClusterDetector::format_cluster_context(&cluster);
        assert!(ctx.contains("1 other issues"));
        assert!(ctx.contains("a, b"));
    }

    #[test]
    fn test_check_cluster_resolution_no_attempts() {
        let tracker = MockTracker { arrivals: vec![] };
        let now = Utc::now();
        let cluster = IssueCluster {
            id: 1,
            cluster_key: "test".to_string(),
            source: "sentry".to_string(),
            issue_ids: vec!["a".to_string(), "b".to_string()],
            window_start: now,
            window_end: now + chrono::Duration::minutes(5),
            resolved_by_issue_id: None,
            resolved_by_attempt_id: None,
            status: "active".to_string(),
            created_at: now,
        };
        let resolved = ClusterDetector::check_cluster_resolution(&tracker, &cluster).unwrap();
        assert!(!resolved);
    }

    #[test]
    fn test_min_cluster_size_boundary() {
        let now = Utc::now();
        // Exactly min_cluster_size items
        let tracker = MockTracker {
            arrivals: vec![
                ("issue-1".to_string(), now),
                ("issue-2".to_string(), now + chrono::Duration::minutes(1)),
            ],
        };
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 30, 2).unwrap();
        assert_eq!(clusters.len(), 1);

        // One fewer than min_cluster_size
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 30, 3).unwrap();
        assert!(clusters.is_empty());
    }

    #[test]
    fn test_detect_clusters_with_sqlite_tracker() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();

        // Record several attempts close together
        tracker.record_attempt("sentry", "iss-1", "I-1").unwrap();
        tracker.record_attempt("sentry", "iss-2", "I-2").unwrap();
        tracker.record_attempt("sentry", "iss-3", "I-3").unwrap();

        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 60, 3).unwrap();
        // All 3 recorded within ~instant, so they should cluster
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].issue_ids.len(), 3);
    }

    #[test]
    fn test_store_and_check_cluster_resolution_with_sqlite() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();

        // Record some attempts
        tracker.record_attempt("sentry", "iss-1", "I-1").unwrap();
        tracker.record_attempt("sentry", "iss-2", "I-2").unwrap();
        tracker.record_attempt("sentry", "iss-3", "I-3").unwrap();

        // Detect clusters
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 60, 3).unwrap();
        assert_eq!(clusters.len(), 1);

        // Store the cluster
        let cluster_id = tracker.store_issue_cluster(&clusters[0]).unwrap();
        assert!(cluster_id > 0);

        // Check resolution before any fix — should be false
        let resolved = ClusterDetector::check_cluster_resolution(&tracker, &clusters[0]).unwrap();
        assert!(!resolved);

        // Mark one issue as merged
        tracker
            .mark_success("sentry", "iss-1", "https://github.com/org/repo/pull/1")
            .unwrap();
        tracker.mark_merged("sentry", "iss-1").unwrap();

        // Now check resolution — should be true
        let resolved = ClusterDetector::check_cluster_resolution(&tracker, &clusters[0]).unwrap();
        assert!(resolved);
    }

    #[test]
    fn test_detect_clusters_window_zero() {
        let now = Utc::now();
        let tracker = MockTracker {
            arrivals: vec![
                ("issue-1".to_string(), now),
                ("issue-2".to_string(), now),
                ("issue-3".to_string(), now),
            ],
        };
        // Zero window — only exact same timestamp matches
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 0, 3).unwrap();
        // All at same timestamp, so they should cluster even with window=0
        assert_eq!(clusters.len(), 1);
    }

    #[test]
    fn test_detect_clusters_min_size_one() {
        let now = Utc::now();
        let tracker = MockTracker {
            arrivals: vec![("issue-1".to_string(), now)],
        };
        // min_cluster_size=1 means every single issue is its own cluster
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 30, 1).unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].issue_ids.len(), 1);
    }

    #[test]
    fn test_detect_clusters_large_window_collapses_all() {
        let now = Utc::now();
        let tracker = MockTracker {
            arrivals: vec![
                ("issue-1".to_string(), now),
                ("issue-2".to_string(), now + chrono::Duration::minutes(500)),
                ("issue-3".to_string(), now + chrono::Duration::minutes(1000)),
            ],
        };
        // Very large window collapses all into one cluster
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 10000, 3).unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].issue_ids.len(), 3);
    }

    #[test]
    fn test_format_cluster_context_zero_duration() {
        let now = Utc::now();
        let cluster = IssueCluster {
            id: 1,
            cluster_key: "test".to_string(),
            source: "sentry".to_string(),
            issue_ids: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            window_start: now,
            window_end: now, // same time
            resolved_by_issue_id: None,
            resolved_by_attempt_id: None,
            status: "active".to_string(),
            created_at: now,
        };
        let ctx = ClusterDetector::format_cluster_context(&cluster);
        assert!(ctx.contains("0 minutes"));
    }

    #[test]
    fn test_cluster_key_empty_ids() {
        let key = ClusterDetector::compute_cluster_key(&[]);
        assert!(key.starts_with("cluster_"));
    }

    #[test]
    fn test_cluster_key_single_id() {
        let key = ClusterDetector::compute_cluster_key(&["only-one".to_string()]);
        assert!(key.starts_with("cluster_"));
        assert!(key.len() > "cluster_".len());
    }

    #[test]
    fn test_detect_clusters_exactly_at_boundary() {
        let now = Utc::now();
        let tracker = MockTracker {
            arrivals: vec![
                ("issue-1".to_string(), now),
                ("issue-2".to_string(), now + chrono::Duration::minutes(15)),
                // Exactly at window boundary (30 min)
                ("issue-3".to_string(), now + chrono::Duration::minutes(30)),
            ],
        };
        // issue-3 at exactly window_end should be included (<=)
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 30, 3).unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].issue_ids.len(), 3);
    }

    #[test]
    fn test_detect_clusters_one_past_boundary() {
        let now = Utc::now();
        let tracker = MockTracker {
            arrivals: vec![
                ("issue-1".to_string(), now),
                ("issue-2".to_string(), now + chrono::Duration::minutes(15)),
                // Just past window boundary
                ("issue-3".to_string(), now + chrono::Duration::minutes(31)),
            ],
        };
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 30, 3).unwrap();
        // issue-3 is outside the 30-min window from issue-1, so no cluster of 3
        assert!(clusters.is_empty());
    }

    #[test]
    fn test_full_cluster_lifecycle() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();

        // Step 1: Issues arrive
        tracker.record_attempt("sentry", "crash-1", "C-1").unwrap();
        tracker.record_attempt("sentry", "crash-2", "C-2").unwrap();
        tracker.record_attempt("sentry", "crash-3", "C-3").unwrap();

        // Step 2: Detect
        let clusters = ClusterDetector::detect_clusters(&tracker, "sentry", 60, 3).unwrap();
        assert_eq!(clusters.len(), 1);

        // Step 3: Store
        let cluster_id = tracker.store_issue_cluster(&clusters[0]).unwrap();

        // Step 4: Format context
        let ctx = ClusterDetector::format_cluster_context(&clusters[0]);
        assert!(ctx.contains("2 other issues"));
        assert!(ctx.contains("crash-1"));

        // Step 5: Active clusters should show up
        let active = tracker.get_active_clusters("sentry").unwrap();
        assert_eq!(active.len(), 1);

        // Step 6: Resolve
        tracker
            .update_cluster_resolution(cluster_id, "crash-1", 42)
            .unwrap();
        let active = tracker.get_active_clusters("sentry").unwrap();
        assert!(active.is_empty());
    }
}
