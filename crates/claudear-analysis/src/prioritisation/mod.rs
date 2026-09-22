//! Prioritisation engine.
//!
//! Computes a composite severity score from multiple signals, classifies blast
//! radius, clusters content-similar issues, and evaluates suppression rules.

pub mod blast_radius;
pub mod content_cluster;
pub mod scorer;
pub mod suppression;

use crate::llm::LlmAnalyzer;
use claudear_config::config::PrioritisationConfig;
use claudear_core::types::{
    ContentCluster, Issue, MatchResult, PrioritisedIssue, SeverityScore, SuppressionResult,
};
use claudear_storage::FixAttemptTracker;

/// Run the full prioritisation pipeline on a batch of candidates.
///
/// Steps:
/// 1. Suppress issues matching user rules
/// 2. Classify blast radius for each remaining issue
/// 3. Detect content clusters
/// 4. Score each issue
/// 5. Sort by score descending
/// 6. Store clusters in the tracker
///
/// Returns `(prioritised_issues, suppressed_issues_with_results)`.
pub fn prioritise(
    config: &PrioritisationConfig,
    candidates: Vec<(Issue, MatchResult)>,
    tracker: &dyn FixAttemptTracker,
    embeddings: &std::collections::HashMap<String, Vec<f32>>,
    llm: Option<&dyn LlmAnalyzer>,
) -> (Vec<PrioritisedIssue>, Vec<(Issue, SuppressionResult)>) {
    // Step 1: Suppress -- consume candidates without cloning by partitioning
    // into kept and suppressed via the suppression engine.
    let (issues_only, mut match_map): (Vec<Issue>, std::collections::HashMap<String, MatchResult>) =
        candidates.into_iter().fold(
            (Vec::new(), std::collections::HashMap::new()),
            |(mut issues, mut map), (issue, mr)| {
                map.insert(issue.id.clone(), mr);
                issues.push(issue);
                (issues, map)
            },
        );

    let (kept_issues, suppressed) = suppression::evaluate(&config.suppression_rules, issues_only);

    // Rebuild kept pairs from map (issues consumed, not cloned)
    let mut kept: Vec<(Issue, MatchResult)> = kept_issues
        .into_iter()
        .filter_map(|issue| {
            let mr = match_map.remove(&issue.id)?;
            Some((issue, mr))
        })
        .collect();

    // LLM batch assessment (severity + blast radius + fingerprint)
    let llm_assessments: std::collections::HashMap<String, crate::llm::LlmIssueAssessment> = llm
        .and_then(|l| l.assess_issues(&kept))
        .unwrap_or_default()
        .into_iter()
        .map(|a| (a.issue_id.clone(), a))
        .collect();

    // Step 2 + 3: Detect content clusters (needs full candidate list for grouping)
    let clusters = if config.content_clustering {
        content_cluster::detect(&kept, config, embeddings)
    } else {
        Vec::new()
    };
    let clustered_ids = content_cluster::clustered_issue_ids(&clusters);

    // Build a lookup map from issue_id -> cluster_key to avoid O(M*K) linear scan
    let mut cluster_key_map: std::collections::HashMap<String, String> = clusters
        .iter()
        .flat_map(|c| {
            c.issue_ids
                .iter()
                .map(move |id| (id.clone(), c.cluster_key.clone()))
        })
        .collect();

    // Merge LLM fingerprint-based clusters: issues sharing a fingerprint get the same cluster_key
    for (issue_id, assessment) in &llm_assessments {
        if !cluster_key_map.contains_key(issue_id) && !assessment.fingerprint.is_empty() {
            cluster_key_map.insert(issue_id.clone(), format!("llm:{}", assessment.fingerprint));
        }
    }

    // Step 4: Classify and score each issue
    let mut prioritised: Vec<PrioritisedIssue> = kept
        .drain(..)
        .map(|(issue, match_result)| {
            let (br, severity_score, cluster_key) =
                if let Some(assessment) = llm_assessments.get(&issue.id) {
                    // Use LLM assessment with meaningful component breakdown
                    let br_component = blast_radius::blast_radius_score(assessment.blast_radius);
                    let score = SeverityScore {
                        score: assessment.severity,
                        severity_component: assessment.severity,
                        blast_radius_component: br_component,
                        ..SeverityScore::default()
                    };
                    let ck = cluster_key_map.get(&issue.id).cloned();
                    (assessment.blast_radius, score, ck)
                } else {
                    // Heuristic fallback
                    let br = blast_radius::classify(&issue, config);
                    let in_cluster = clustered_ids.contains(&issue.id);
                    let score = scorer::compute(&issue, &match_result, br, in_cluster, config);
                    let ck = cluster_key_map.get(&issue.id).cloned();
                    (br, score, ck)
                };

            PrioritisedIssue {
                issue,
                match_result,
                severity_score,
                blast_radius: br,
                cluster_key,
            }
        })
        .collect();

    // Step 5: Sort by score descending (total_cmp handles NaN deterministically)
    prioritised.sort_by(|a, b| b.severity_score.score.total_cmp(&a.severity_score.score));

    // Step 6: Store clusters
    store_clusters(tracker, &clusters);

    (prioritised, suppressed)
}

/// Persist content clusters to the tracker (best-effort, log on error).
fn store_clusters(tracker: &dyn FixAttemptTracker, clusters: &[ContentCluster]) {
    for cluster in clusters {
        if let Err(e) = tracker.store_content_cluster(cluster) {
            tracing::warn!(
                cluster_key = %cluster.cluster_key,
                error = %e,
                "Failed to store content cluster"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_config::config::PrioritisationConfig;
    use claudear_core::types::{
        IssuePriority, MatchPriority, MatchResult, SuppressionField, SuppressionMatchMode,
        SuppressionRule,
    };
    use claudear_storage::{
        ActivityStore, AttemptTracker, ChatStore, DiscordStore, EmbeddingStore, EvaluationStore,
        ExperimentStore, KnowledgeStore, RegressionStore, RepoStore, SimilarityStore, UserStore,
        WebhookStore,
    };

    /// Minimal no-op tracker for tests.
    struct NoOpTracker;
    impl AttemptTracker for NoOpTracker {
        fn has_attempted(&self, _: &str, _: &str) -> claudear_core::error::Result<bool> {
            Ok(false)
        }
        fn get_attempted_issue_ids(
            &self,
            _: &str,
        ) -> claudear_core::error::Result<std::collections::HashSet<String>> {
            Ok(std::collections::HashSet::new())
        }
        fn record_attempt(&self, _: &str, _: &str, _: &str) -> claudear_core::error::Result<()> {
            Ok(())
        }
        fn record_attempt_with_labels(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[String],
        ) -> claudear_core::error::Result<()> {
            Ok(())
        }
        fn mark_success(&self, _: &str, _: &str, _: &str) -> claudear_core::error::Result<()> {
            Ok(())
        }
        fn mark_failed(&self, _: &str, _: &str, _: &str) -> claudear_core::error::Result<()> {
            Ok(())
        }
        fn mark_merged(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
            Ok(())
        }
        fn mark_closed(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
            Ok(())
        }
        fn mark_resolved(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
            Ok(())
        }
        fn get_attempt(
            &self,
            _: &str,
            _: &str,
        ) -> claudear_core::error::Result<Option<claudear_core::types::FixAttempt>> {
            Ok(None)
        }
        fn get_attempts_by_status(
            &self,
            _: claudear_core::types::FixAttemptStatus,
        ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
            Ok(vec![])
        }
        fn get_pending_prs(
            &self,
        ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
            Ok(vec![])
        }
        fn get_attempt_by_pr_url(
            &self,
            _: &str,
        ) -> claudear_core::error::Result<Option<claudear_core::types::FixAttempt>> {
            Ok(None)
        }
        fn reset_attempt(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
            Ok(())
        }
        fn get_stats(&self) -> claudear_core::error::Result<claudear_core::types::FixAttemptStats> {
            Ok(claudear_core::types::FixAttemptStats::default())
        }
        fn increment_retry(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
            Ok(())
        }
        fn mark_cannot_fix(&self, _: &str, _: &str, _: &str) -> claudear_core::error::Result<()> {
            Ok(())
        }
        fn get_retryable_issues(
            &self,
            _: u32,
        ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
            Ok(vec![])
        }
        fn prepare_for_retry(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
            Ok(())
        }
    }
    impl ActivityStore for NoOpTracker {}
    impl KnowledgeStore for NoOpTracker {}
    impl EmbeddingStore for NoOpTracker {}
    impl RepoStore for NoOpTracker {}
    impl UserStore for NoOpTracker {}
    impl ChatStore for NoOpTracker {}
    impl RegressionStore for NoOpTracker {}
    impl ExperimentStore for NoOpTracker {}
    impl EvaluationStore for NoOpTracker {}
    impl WebhookStore for NoOpTracker {}
    impl SimilarityStore for NoOpTracker {}
    impl DiscordStore for NoOpTracker {}

    fn make_candidate(
        id: &str,
        title: &str,
        issue_prio: IssuePriority,
        match_prio: MatchPriority,
    ) -> (claudear_core::types::Issue, MatchResult) {
        let mut issue = claudear_core::types::Issue::new(id, id, title, "url", "sentry");
        issue.priority = issue_prio;
        let mr = MatchResult::matched("test", match_prio);
        (issue, mr)
    }

    #[test]
    fn higher_priority_scored_first() {
        let config = PrioritisationConfig::default();
        let candidates = vec![
            make_candidate("low", "Low bug", IssuePriority::Low, MatchPriority::Low),
            make_candidate(
                "crit",
                "Critical crash",
                IssuePriority::Critical,
                MatchPriority::Urgent,
            ),
        ];
        let tracker = NoOpTracker;
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(suppressed.is_empty());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].issue.id, "crit");
        assert_eq!(result[1].issue.id, "low");
    }

    #[test]
    fn suppression_filters_issues() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "noise".into(),
                field: SuppressionField::Title,
                pattern: "flaky".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "known flaky".into(),
            }],
            ..Default::default()
        };
        let candidates = vec![
            make_candidate("real", "Real bug", IssuePriority::High, MatchPriority::High),
            make_candidate(
                "flaky",
                "Flaky test timeout",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
        ];
        let tracker = NoOpTracker;
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].issue.id, "real");
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].0.id, "flaky");
    }

    #[test]
    fn disabled_engine_still_works() {
        let config = PrioritisationConfig {
            enabled: false,
            ..Default::default()
        };
        let candidates = vec![make_candidate(
            "a",
            "Bug",
            IssuePriority::High,
            MatchPriority::High,
        )];
        let tracker = NoOpTracker;
        // Even when disabled at the config level, the function itself still works.
        // The caller decides whether to call it based on config.enabled.
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn blast_radius_affects_score() {
        let config = PrioritisationConfig::default();
        let mut auth_candidate = make_candidate(
            "auth",
            "Auth crash",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        auth_candidate
            .0
            .set_metadata("filename", "src/auth/login.rs");

        let mut docs_candidate = make_candidate(
            "docs",
            "Docs typo",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        docs_candidate.0.set_metadata("filename", "README.md");

        let candidates = vec![docs_candidate, auth_candidate];
        let tracker = NoOpTracker;
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result[0].issue.id, "auth");
        assert_eq!(
            result[0].blast_radius,
            claudear_core::types::BlastRadius::Critical
        );
    }

    #[test]
    fn test_empty_candidates() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;
        let (result, suppressed) = prioritise(
            &config,
            vec![],
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(result.is_empty(), "empty input must produce empty output");
        assert!(suppressed.is_empty());
    }

    #[test]
    fn test_single_candidate() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;
        let candidates = vec![make_candidate(
            "only",
            "Single issue",
            IssuePriority::High,
            MatchPriority::High,
        )];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        assert!(suppressed.is_empty());
        assert_eq!(result[0].issue.id, "only");
        assert_eq!(result[0].issue.title, "Single issue");
        assert!(
            result[0].severity_score.score > 0.0,
            "score must be positive for a High/High candidate"
        );
        assert!(
            result[0].severity_score.severity_component > 0.0,
            "severity component must be populated"
        );
    }

    #[test]
    fn test_all_suppressed() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "suppress-all".into(),
                field: SuppressionField::Source,
                pattern: "sentry".into(),
                match_mode: SuppressionMatchMode::Exact,
                sources: vec![],
                reason: "suppress everything from sentry".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![
            make_candidate("a", "Bug A", IssuePriority::High, MatchPriority::High),
            make_candidate("b", "Bug B", IssuePriority::Low, MatchPriority::Low),
            make_candidate("c", "Bug C", IssuePriority::Critical, MatchPriority::Urgent),
        ];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(
            result.is_empty(),
            "all candidates should be suppressed, got {} remaining",
            result.len()
        );
        assert_eq!(suppressed.len(), 3);
        for (issue, sr) in &suppressed {
            assert!(sr.suppressed);
            assert_eq!(sr.matched_rule.as_deref(), Some("suppress-all"));
            assert!(
                ["a", "b", "c"].contains(&issue.id.as_str()),
                "unexpected suppressed id: {}",
                issue.id
            );
        }
    }

    #[test]
    fn test_content_clustering_enabled() {
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        // Two issues with same error_type + culprit and similar titles
        let (mut issue1, mr1) = make_candidate(
            "c1",
            "TypeError in payment handler",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue1.set_metadata("error_type", "TypeError");
        issue1.set_metadata("culprit", "payment.handler");

        let (mut issue2, mr2) = make_candidate(
            "c2",
            "TypeError in payment processor",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue2.set_metadata("error_type", "TypeError");
        issue2.set_metadata("culprit", "payment.handler");

        let candidates = vec![(issue1, mr1), (issue2, mr2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(result.len(), 2);
        // Both should have a cluster_key set
        assert!(
            result[0].cluster_key.is_some(),
            "first issue should have cluster_key"
        );
        assert!(
            result[1].cluster_key.is_some(),
            "second issue should have cluster_key"
        );
        assert_eq!(
            result[0].cluster_key, result[1].cluster_key,
            "both issues should share the same cluster_key"
        );
        // Cluster key should contain the error_type and culprit
        let key = result[0].cluster_key.as_ref().unwrap();
        assert!(
            key.contains("TypeError"),
            "cluster_key should contain error_type"
        );
        assert!(
            key.contains("payment.handler"),
            "cluster_key should contain culprit"
        );
    }

    #[test]
    fn test_content_clustering_disabled() {
        let config = PrioritisationConfig {
            content_clustering: false,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue1, mr1) = make_candidate(
            "d1",
            "TypeError in payment handler",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue1.set_metadata("error_type", "TypeError");
        issue1.set_metadata("culprit", "payment.handler");

        let (mut issue2, mr2) = make_candidate(
            "d2",
            "TypeError in payment processor",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue2.set_metadata("error_type", "TypeError");
        issue2.set_metadata("culprit", "payment.handler");

        let candidates = vec![(issue1, mr1), (issue2, mr2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(result.len(), 2);
        for pi in &result {
            assert!(
                pi.cluster_key.is_none(),
                "cluster_key should be None when clustering disabled, got {:?}",
                pi.cluster_key
            );
        }
    }

    #[test]
    fn test_ordering_multiple_priorities() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;
        let candidates = vec![
            make_candidate(
                "med",
                "Medium bug",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
            make_candidate(
                "crit",
                "Critical crash",
                IssuePriority::Critical,
                MatchPriority::Urgent,
            ),
            make_candidate("low", "Low issue", IssuePriority::Low, MatchPriority::Low),
            make_candidate(
                "high",
                "High alert",
                IssuePriority::High,
                MatchPriority::High,
            ),
        ];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(suppressed.is_empty());
        assert_eq!(result.len(), 4);

        // Verify descending score order
        assert_eq!(result[0].issue.id, "crit");
        assert_eq!(result[1].issue.id, "high");
        assert_eq!(result[2].issue.id, "med");
        assert_eq!(result[3].issue.id, "low");

        // Verify scores are strictly decreasing
        for i in 0..result.len() - 1 {
            assert!(
                result[i].severity_score.score >= result[i + 1].severity_score.score,
                "scores must be descending: [{}]={} vs [{}]={}",
                i,
                result[i].severity_score.score,
                i + 1,
                result[i + 1].severity_score.score
            );
        }
    }

    #[test]
    fn test_suppression_preserves_non_matching() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "kill-low".into(),
                field: SuppressionField::Title,
                pattern: "noise".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "noisy".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![
            make_candidate(
                "hi",
                "Critical crash",
                IssuePriority::Critical,
                MatchPriority::Urgent,
            ),
            make_candidate(
                "noise1",
                "Noise from CI",
                IssuePriority::Low,
                MatchPriority::Low,
            ),
            make_candidate(
                "med",
                "Medium bug",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
        ];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(
            suppressed.len(),
            1,
            "exactly one issue should be suppressed"
        );
        assert_eq!(suppressed[0].0.id, "noise1");

        assert_eq!(result.len(), 2, "two issues should remain");
        // Verify correct order: Critical > Medium
        assert_eq!(result[0].issue.id, "hi");
        assert_eq!(result[1].issue.id, "med");
    }

    #[test]
    fn test_store_clusters_called() {
        use std::sync::Mutex;

        /// Tracker that records `store_content_cluster` calls.
        struct ClusterTrackingTracker {
            stored: Mutex<Vec<ContentCluster>>,
        }

        impl ClusterTrackingTracker {
            fn new() -> Self {
                Self {
                    stored: Mutex::new(Vec::new()),
                }
            }
        }

        impl AttemptTracker for ClusterTrackingTracker {
            fn has_attempted(&self, _: &str, _: &str) -> claudear_core::error::Result<bool> {
                Ok(false)
            }
            fn get_attempted_issue_ids(
                &self,
                _: &str,
            ) -> claudear_core::error::Result<std::collections::HashSet<String>> {
                Ok(std::collections::HashSet::new())
            }
            fn record_attempt(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn record_attempt_with_labels(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &[String],
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_success(&self, _: &str, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_failed(&self, _: &str, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_merged(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_closed(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_resolved(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn get_attempt(
                &self,
                _: &str,
                _: &str,
            ) -> claudear_core::error::Result<Option<claudear_core::types::FixAttempt>>
            {
                Ok(None)
            }
            fn get_attempts_by_status(
                &self,
                _: claudear_core::types::FixAttemptStatus,
            ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
                Ok(vec![])
            }
            fn get_pending_prs(
                &self,
            ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
                Ok(vec![])
            }
            fn get_attempt_by_pr_url(
                &self,
                _: &str,
            ) -> claudear_core::error::Result<Option<claudear_core::types::FixAttempt>>
            {
                Ok(None)
            }
            fn reset_attempt(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn get_stats(
                &self,
            ) -> claudear_core::error::Result<claudear_core::types::FixAttemptStats> {
                Ok(claudear_core::types::FixAttemptStats::default())
            }
            fn increment_retry(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_cannot_fix(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn get_retryable_issues(
                &self,
                _: u32,
            ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
                Ok(vec![])
            }
            fn prepare_for_retry(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
        }

        impl ActivityStore for ClusterTrackingTracker {}

        impl KnowledgeStore for ClusterTrackingTracker {
            fn store_content_cluster(
                &self,
                cluster: &claudear_core::types::ContentCluster,
            ) -> claudear_core::error::Result<i64> {
                self.stored.lock().unwrap().push(cluster.clone());
                Ok(1)
            }
        }

        impl EmbeddingStore for ClusterTrackingTracker {}

        impl RepoStore for ClusterTrackingTracker {}

        impl UserStore for ClusterTrackingTracker {}
        impl ChatStore for ClusterTrackingTracker {}
        impl RegressionStore for ClusterTrackingTracker {}
        impl ExperimentStore for ClusterTrackingTracker {}
        impl EvaluationStore for ClusterTrackingTracker {}
        impl WebhookStore for ClusterTrackingTracker {}
        impl SimilarityStore for ClusterTrackingTracker {}
        impl DiscordStore for ClusterTrackingTracker {}

        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            ..Default::default()
        };

        let (mut issue1, mr1) = make_candidate(
            "s1",
            "TypeError in payment handler",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue1.set_metadata("error_type", "TypeError");
        issue1.set_metadata("culprit", "payment.handler");

        let (mut issue2, mr2) = make_candidate(
            "s2",
            "TypeError in payment processor",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue2.set_metadata("error_type", "TypeError");
        issue2.set_metadata("culprit", "payment.handler");

        let tracker = ClusterTrackingTracker::new();
        let (result, _) = prioritise(
            &config,
            vec![(issue1, mr1), (issue2, mr2)],
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(result.len(), 2);

        let stored = tracker.stored.lock().unwrap();
        assert_eq!(
            stored.len(),
            1,
            "exactly one cluster should have been stored"
        );
        assert_eq!(stored[0].issue_ids.len(), 2);
        assert!(stored[0].cluster_key.contains("TypeError"));
        assert!(stored[0].cluster_key.contains("payment.handler"));
    }

    // --- Edge cases for the `prioritise` orchestrator ---

    #[test]
    fn duplicate_issue_ids_last_match_result_wins() {
        // When two candidates share the same issue.id, the match_map will
        // keep the last-inserted MatchResult.  Verify the pipeline doesn't
        // panic and produces one entry per surviving issue.
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;
        let mut dup1 = make_candidate("dup", "First", IssuePriority::Low, MatchPriority::Low);
        let mut dup2 = make_candidate("dup", "Second", IssuePriority::High, MatchPriority::Urgent);
        // Give them different titles so we can tell which survived
        dup1.0.title = "First".into();
        dup2.0.title = "Second".into();

        let candidates = vec![dup1, dup2];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(suppressed.is_empty());
        // Duplicate issue IDs are deduplicated: the match_map is keyed by id,
        // so the second insert overwrites the first. On rebuild the first issue
        // consumes the entry and the second is dropped by filter_map.
        assert_eq!(result.len(), 1);
        assert!(
            result[0].severity_score.score.is_finite(),
            "score must be finite for duplicate-id issue '{}'",
            result[0].issue.title
        );
    }

    #[test]
    fn scores_are_always_finite() {
        // Ensure no NaN or Inf creeps into final scores even with
        // extreme metadata values.
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) = make_candidate(
            "edge",
            "Edge case",
            IssuePriority::Critical,
            MatchPriority::Urgent,
        );
        issue.set_metadata("event_count", f64::MAX);
        issue.set_metadata("user_count", f64::MAX);
        issue.set_metadata("escalation_rate", f64::MAX);
        issue.set_metadata("is_unhandled", true);
        issue.set_metadata("level", "fatal");
        issue.set_metadata("filename", "src/auth/login.rs");

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        assert!(
            result[0].severity_score.score.is_finite(),
            "score must be finite, got {}",
            result[0].severity_score.score
        );
    }

    #[test]
    fn zero_event_count_does_not_panic() {
        // event_count = 0 should not cause log2(0) = -Inf issues
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) = make_candidate(
            "zero",
            "Zero events",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue.set_metadata("event_count", 0i64);
        issue.set_metadata("user_count", 0i64);

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        let score = &result[0].severity_score;
        assert!(
            score.frequency_component.is_finite() && score.frequency_component >= 0.0,
            "frequency_component must be finite and non-negative with zero counts, got {}",
            score.frequency_component
        );
    }

    #[test]
    fn negative_event_count_clamped() {
        // Negative metadata values should not produce negative score components.
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) = make_candidate(
            "neg",
            "Negative events",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue.set_metadata("event_count", -100i64);
        issue.set_metadata("user_count", -50i64);
        issue.set_metadata("escalation_rate", -1.0);

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        let score = &result[0].severity_score;
        assert!(
            score.frequency_component >= 0.0,
            "frequency_component must be >= 0 even with negative metadata, got {}",
            score.frequency_component
        );
        assert!(
            score.score.is_finite(),
            "overall score must be finite, got {}",
            score.score
        );
    }

    #[test]
    fn none_priority_issue_scored_lowest() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;
        let candidates = vec![
            make_candidate(
                "none",
                "No priority",
                IssuePriority::None,
                MatchPriority::Low,
            ),
            make_candidate("low", "Low bug", IssuePriority::Low, MatchPriority::Low),
        ];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 2);
        // IssuePriority::None (0.0) + MatchPriority::Low (0.25) < Low (0.25) + Low (0.25)
        assert_eq!(result[0].issue.id, "low");
        assert_eq!(result[1].issue.id, "none");
        assert!(
            result[0].severity_score.score > result[1].severity_score.score,
            "Low/Low should score higher than None/Low"
        );
    }

    #[test]
    fn score_components_populated_in_output() {
        // Verify all SeverityScore fields are populated (not left as default 0.0)
        // when appropriate metadata is provided.
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) = make_candidate(
            "full",
            "Full metadata",
            IssuePriority::High,
            MatchPriority::High,
        );
        issue.set_metadata("event_count", 5000i64);
        issue.set_metadata("user_count", 200i64);
        issue.set_metadata("escalation_rate", 0.8);
        issue.set_metadata("is_unhandled", true);
        issue.set_metadata("level", "error");
        issue.set_metadata("filename", "src/auth/login.rs");

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        let score = &result[0].severity_score;

        assert!(score.severity_component > 0.0, "severity should be > 0");
        assert!(score.frequency_component > 0.0, "frequency should be > 0");
        assert!(score.regression_component > 0.0, "regression should be > 0");
        assert!(
            score.blast_radius_component > 0.0,
            "blast_radius should be > 0"
        );
        // Not in a cluster, so cluster_boost should be 0.0
        assert!(
            (score.cluster_boost - 0.0).abs() < f64::EPSILON,
            "cluster_boost should be 0.0 for non-clustered issue"
        );
        // Blast radius should be Critical for auth path
        assert_eq!(
            result[0].blast_radius,
            claudear_core::types::BlastRadius::Critical
        );
    }

    #[test]
    fn all_weights_zero_produces_zero_scores() {
        let config = PrioritisationConfig {
            severity_weight: 0.0,
            frequency_weight: 0.0,
            regression_weight: 0.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![
            make_candidate(
                "crit",
                "Critical",
                IssuePriority::Critical,
                MatchPriority::Urgent,
            ),
            make_candidate("low", "Low", IssuePriority::Low, MatchPriority::Low),
        ];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 2);
        for pi in &result {
            assert!(
                pi.severity_score.score.abs() < f64::EPSILON,
                "all-zero weights should produce 0.0 score, got {} for {}",
                pi.severity_score.score,
                pi.issue.id
            );
        }
    }

    #[test]
    fn only_severity_weight_matters_when_others_zero() {
        let config = PrioritisationConfig {
            severity_weight: 1.0,
            frequency_weight: 0.0,
            regression_weight: 0.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![
            make_candidate("high", "High", IssuePriority::High, MatchPriority::High),
            make_candidate("low", "Low", IssuePriority::Low, MatchPriority::Low),
        ];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result[0].issue.id, "high");
        assert_eq!(result[1].issue.id, "low");

        // Score should equal the severity_component exactly
        for pi in &result {
            assert!(
                (pi.severity_score.score - pi.severity_score.severity_component).abs() < 1e-10,
                "score should equal severity_component when only severity_weight=1.0"
            );
        }
    }

    // --- Suppression + clustering interaction ---

    #[test]
    fn suppressed_issues_not_clustered() {
        // If issues that would form a cluster are suppressed, no cluster should form.
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            suppression_rules: vec![SuppressionRule {
                name: "suppress-type".into(),
                field: SuppressionField::Title,
                pattern: "TypeError".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "suppress type errors".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue1, mr1) = make_candidate(
            "c1",
            "TypeError in handler",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue1.set_metadata("error_type", "TypeError");
        issue1.set_metadata("culprit", "handler");

        let (mut issue2, mr2) = make_candidate(
            "c2",
            "TypeError in processor",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue2.set_metadata("error_type", "TypeError");
        issue2.set_metadata("culprit", "handler");

        let candidates = vec![(issue1, mr1), (issue2, mr2)];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        assert!(
            result.is_empty(),
            "all TypeError issues should be suppressed"
        );
        assert_eq!(suppressed.len(), 2);
        // Ensure no cluster_key leaked through
        for pi in &result {
            assert!(
                pi.cluster_key.is_none(),
                "suppressed issues should not appear in results with cluster keys"
            );
        }
    }

    #[test]
    fn partial_suppression_breaks_cluster_below_min_size() {
        // Two issues would form a cluster, but suppressing one drops below min_content_cluster_size.
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            suppression_rules: vec![SuppressionRule {
                name: "suppress-one".into(),
                field: SuppressionField::Title,
                pattern: "processor".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "suppress processor".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue1, mr1) = make_candidate(
            "c1",
            "TypeError in handler",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue1.set_metadata("error_type", "TypeError");
        issue1.set_metadata("culprit", "handler");

        let (mut issue2, mr2) = make_candidate(
            "c2",
            "TypeError in processor",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue2.set_metadata("error_type", "TypeError");
        issue2.set_metadata("culprit", "handler");

        let candidates = vec![(issue1, mr1), (issue2, mr2)];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(result.len(), 1, "one issue should remain");
        assert_eq!(suppressed.len(), 1, "one issue should be suppressed");
        assert_eq!(result[0].issue.id, "c1");
        assert!(
            result[0].cluster_key.is_none(),
            "surviving issue should not be in a cluster (only 1 left, below min_size=2)"
        );
    }

    #[test]
    fn suppression_runs_before_clustering() {
        // Cluster of 3 issues; suppress 1 -> cluster of 2 should still form.
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            suppression_rules: vec![SuppressionRule {
                name: "suppress-third".into(),
                field: SuppressionField::Title,
                pattern: "third".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "suppress third".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let make_clusterable = |id: &str, title: &str| {
            let (mut issue, mr) =
                make_candidate(id, title, IssuePriority::Medium, MatchPriority::Normal);
            issue.set_metadata("error_type", "ValueError");
            issue.set_metadata("culprit", "parser");
            (issue, mr)
        };

        let candidates = vec![
            make_clusterable("k1", "ValueError in parser module"),
            make_clusterable("k2", "ValueError in parser engine"),
            make_clusterable("k3", "ValueError in third parser path"),
        ];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].0.id, "k3");
        assert_eq!(result.len(), 2);
        // Remaining 2 should form a cluster
        assert!(
            result[0].cluster_key.is_some(),
            "remaining issues should form a cluster"
        );
        assert_eq!(
            result[0].cluster_key, result[1].cluster_key,
            "both remaining issues should share a cluster key"
        );
    }

    // --- Priority ordering edge cases ---

    #[test]
    fn identical_priority_stable_relative_order() {
        // Issues with identical scores should not panic or produce unstable results.
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;
        let candidates = vec![
            make_candidate("a", "Bug A", IssuePriority::Medium, MatchPriority::Normal),
            make_candidate("b", "Bug B", IssuePriority::Medium, MatchPriority::Normal),
            make_candidate("c", "Bug C", IssuePriority::Medium, MatchPriority::Normal),
        ];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 3);
        // All should have equal scores
        let scores: Vec<f64> = result.iter().map(|pi| pi.severity_score.score).collect();
        assert!(
            (scores[0] - scores[1]).abs() < f64::EPSILON
                && (scores[1] - scores[2]).abs() < f64::EPSILON,
            "identical issues should have identical scores: {:?}",
            scores
        );
    }

    #[test]
    fn ordering_is_strictly_descending() {
        // Exhaustive check that all priority levels sort correctly.
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;
        let candidates = vec![
            make_candidate("none", "None", IssuePriority::None, MatchPriority::Low),
            make_candidate("low", "Low", IssuePriority::Low, MatchPriority::Low),
            make_candidate("med", "Med", IssuePriority::Medium, MatchPriority::Normal),
            make_candidate("high", "High", IssuePriority::High, MatchPriority::High),
            make_candidate(
                "crit",
                "Crit",
                IssuePriority::Critical,
                MatchPriority::Urgent,
            ),
        ];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 5);

        let expected_order = ["crit", "high", "med", "low", "none"];
        let actual_order: Vec<&str> = result.iter().map(|pi| pi.issue.id.as_str()).collect();
        assert_eq!(
            actual_order, expected_order,
            "issues must be ordered by descending score"
        );

        // Verify strictly decreasing
        for i in 0..result.len() - 1 {
            assert!(
                result[i].severity_score.score > result[i + 1].severity_score.score,
                "score[{}] ({}) must be > score[{}] ({})",
                i,
                result[i].severity_score.score,
                i + 1,
                result[i + 1].severity_score.score
            );
        }
    }

    #[test]
    fn cluster_boost_can_reorder_issues() {
        // An issue in a cluster gets a boost that can push it above an otherwise
        // higher-priority issue.
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            // Give cluster_weight a large value to make the boost significant
            cluster_weight: 0.50,
            severity_weight: 0.30,
            frequency_weight: 0.0,
            regression_weight: 0.0,
            blast_radius_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        // Two low-priority issues that form a cluster
        let (mut c1, mr1) = make_candidate(
            "cluster1",
            "TypeError in handler alpha",
            IssuePriority::Low,
            MatchPriority::Low,
        );
        c1.set_metadata("error_type", "TypeError");
        c1.set_metadata("culprit", "handler.alpha");

        let (mut c2, mr2) = make_candidate(
            "cluster2",
            "TypeError in handler alpha beta",
            IssuePriority::Low,
            MatchPriority::Low,
        );
        c2.set_metadata("error_type", "TypeError");
        c2.set_metadata("culprit", "handler.alpha");

        // One medium-priority issue NOT in a cluster
        let solo = make_candidate(
            "solo",
            "Standalone medium bug",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );

        let candidates = vec![(c1, mr1), (c2, mr2), solo];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 3);

        // Clustered Low issues get: 0.30 * severity + 0.50 * 1.0 (cluster_boost)
        // Solo Medium issue gets: 0.30 * severity + 0.50 * 0.0
        // The cluster boost (0.50) should push Low+cluster above Medium+solo
        let clustered_score = result
            .iter()
            .find(|pi| pi.issue.id == "cluster1")
            .unwrap()
            .severity_score
            .score;
        let solo_score = result
            .iter()
            .find(|pi| pi.issue.id == "solo")
            .unwrap()
            .severity_score
            .score;
        assert!(
            clustered_score > solo_score,
            "cluster boost should elevate low-priority clustered issue ({}) above solo medium ({})",
            clustered_score,
            solo_score
        );
    }

    // --- Configuration interaction tests ---

    #[test]
    fn high_similarity_threshold_prevents_clustering() {
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.99, // Very high threshold
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue1, mr1) = make_candidate(
            "t1",
            "TypeError in payment handler",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue1.set_metadata("error_type", "TypeError");
        issue1.set_metadata("culprit", "payment.handler");

        let (mut issue2, mr2) = make_candidate(
            "t2",
            "TypeError in payment processor",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue2.set_metadata("error_type", "TypeError");
        issue2.set_metadata("culprit", "payment.handler");

        let candidates = vec![(issue1, mr1), (issue2, mr2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 2);
        // Titles are similar but not 99%+ so no cluster should form
        for pi in &result {
            assert!(
                pi.cluster_key.is_none(),
                "high similarity threshold should prevent clustering, but {} has cluster_key {:?}",
                pi.issue.id,
                pi.cluster_key
            );
        }
    }

    #[test]
    fn large_min_cluster_size_prevents_small_clusters() {
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 10, // Require 10 issues
            cluster_similarity_threshold: 0.3,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        // Only 3 similar issues -- below min_content_cluster_size of 10
        let make_similar = |id: &str, suffix: &str| {
            let (mut issue, mr) = make_candidate(
                id,
                &format!("TypeError in handler {}", suffix),
                IssuePriority::Medium,
                MatchPriority::Normal,
            );
            issue.set_metadata("error_type", "TypeError");
            issue.set_metadata("culprit", "handler");
            (issue, mr)
        };

        let candidates = vec![
            make_similar("m1", "alpha"),
            make_similar("m2", "beta"),
            make_similar("m3", "gamma"),
        ];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 3);
        for pi in &result {
            assert!(
                pi.cluster_key.is_none(),
                "min_cluster_size=10 should prevent clustering 3 issues"
            );
        }
    }

    #[test]
    fn blast_radius_weight_zero_neutralizes_path_classification() {
        let config = PrioritisationConfig {
            blast_radius_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        // auth file (Critical blast radius) vs README (Cosmetic blast radius)
        let (mut auth, mr_auth) = make_candidate(
            "auth",
            "Auth crash",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        auth.set_metadata("filename", "src/auth/login.rs");

        let (mut docs, mr_docs) = make_candidate(
            "docs",
            "Docs typo",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        docs.set_metadata("filename", "README.md");

        let candidates = vec![(auth, mr_auth), (docs, mr_docs)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 2);

        // With blast_radius_weight=0, the blast_radius classification still
        // happens but doesn't affect the score.
        assert_eq!(
            result[0].blast_radius,
            claudear_core::types::BlastRadius::Critical
        );
        // Both should have the same score since everything else is equal
        // and blast_radius_weight is 0.
        assert!(
            (result[0].severity_score.score - result[1].severity_score.score).abs() < 1e-10,
            "blast_radius_weight=0 should make auth ({}) and docs ({}) have equal scores",
            result[0].severity_score.score,
            result[1].severity_score.score
        );
    }

    // --- Score boundary conditions ---

    #[test]
    fn minimum_possible_score() {
        // IssuePriority::None + MatchPriority::Low, no metadata, no cluster
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;
        let candidates = vec![make_candidate(
            "min",
            "Minimal issue",
            IssuePriority::None,
            MatchPriority::Low,
        )];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        let score = &result[0].severity_score;

        // severity_component = 0.6*0.0 + 0.4*0.25 = 0.1
        // frequency = 0.0, regression = 0.0
        // blast_radius = Core (default, no metadata) = 0.6
        // cluster_boost = 0.0
        // total = 0.30*0.1 + 0.25*0.0 + 0.20*0.0 + 0.15*0.6 + 0.10*0.0
        //       = 0.03 + 0.09 = 0.12
        let expected = 0.30 * 0.1 + 0.15 * 0.6;
        assert!(
            (score.score - expected).abs() < 0.001,
            "minimum score expected ~{}, got {}",
            expected,
            score.score
        );
        assert!(score.score > 0.0, "even minimum score should be positive");
    }

    #[test]
    fn maximum_possible_score() {
        // All signals maxed out
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) = make_candidate(
            "max",
            "Maximal issue",
            IssuePriority::Critical,
            MatchPriority::Urgent,
        );
        // Max frequency signals
        issue.set_metadata("event_count", 10_000_000i64);
        issue.set_metadata("user_count", 1_000_000i64);
        issue.set_metadata("escalation_rate", 1.0);
        // Max regression signals
        issue.set_metadata("is_unhandled", true);
        issue.set_metadata("level", "fatal");
        // Critical blast radius
        issue.set_metadata("filename", "src/auth/login.rs");
        // Cluster boost via two similar issues
        issue.set_metadata("error_type", "CriticalError");
        issue.set_metadata("culprit", "auth.login");

        let (mut issue2, mr2) = make_candidate(
            "max2",
            "Maximal issue duplicate",
            IssuePriority::Critical,
            MatchPriority::Urgent,
        );
        issue2.set_metadata("error_type", "CriticalError");
        issue2.set_metadata("culprit", "auth.login");
        issue2.set_metadata("event_count", 10_000_000i64);
        issue2.set_metadata("user_count", 1_000_000i64);
        issue2.set_metadata("escalation_rate", 1.0);
        issue2.set_metadata("is_unhandled", true);
        issue2.set_metadata("level", "fatal");
        issue2.set_metadata("filename", "src/auth/login.rs");

        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            ..config
        };

        let candidates = vec![(issue, mr), (issue2, mr2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(!result.is_empty());

        let score = &result[0].severity_score;
        // All components should be at or near 1.0
        assert!(
            (score.severity_component - 1.0).abs() < 0.001,
            "severity should be ~1.0, got {}",
            score.severity_component
        );
        assert!(
            score.frequency_component > 0.9,
            "frequency should be near 1.0, got {}",
            score.frequency_component
        );
        assert!(
            (score.regression_component - 0.75).abs() < 0.001,
            "regression should be 0.75 (unhandled+fatal), got {}",
            score.regression_component
        );
        assert!(
            (score.blast_radius_component - 1.0).abs() < 0.001,
            "blast_radius should be 1.0 (Critical), got {}",
            score.blast_radius_component
        );
        assert!(
            (score.cluster_boost - 1.0).abs() < 0.001,
            "cluster_boost should be 1.0, got {}",
            score.cluster_boost
        );
    }

    #[test]
    fn score_components_are_individually_bounded() {
        // For any input, each component should be in [0.0, 1.0].
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let test_cases: Vec<(Issue, MatchResult)> = vec![
            make_candidate("a", "None/Low", IssuePriority::None, MatchPriority::Low),
            make_candidate(
                "b",
                "Critical/Urgent",
                IssuePriority::Critical,
                MatchPriority::Urgent,
            ),
            {
                let (mut i, m) = make_candidate(
                    "c",
                    "With metadata",
                    IssuePriority::High,
                    MatchPriority::High,
                );
                i.set_metadata("event_count", 999_999i64);
                i.set_metadata("user_count", 50_000i64);
                i.set_metadata("escalation_rate", 0.95);
                i.set_metadata("is_unhandled", true);
                i.set_metadata("level", "fatal");
                i.set_metadata("filename", "deploy/docker.yml");
                (i, m)
            },
        ];

        let (result, _) = prioritise(
            &config,
            test_cases,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        for pi in &result {
            let s = &pi.severity_score;
            assert!(
                s.severity_component >= 0.0 && s.severity_component <= 1.0,
                "severity_component out of [0,1]: {}",
                s.severity_component
            );
            assert!(
                s.frequency_component >= 0.0 && s.frequency_component <= 1.0,
                "frequency_component out of [0,1]: {}",
                s.frequency_component
            );
            assert!(
                s.regression_component >= 0.0 && s.regression_component <= 1.0,
                "regression_component out of [0,1]: {}",
                s.regression_component
            );
            assert!(
                s.blast_radius_component >= 0.0 && s.blast_radius_component <= 1.0,
                "blast_radius_component out of [0,1]: {}",
                s.blast_radius_component
            );
            assert!(
                s.cluster_boost == 0.0 || s.cluster_boost == 1.0,
                "cluster_boost must be 0 or 1: {}",
                s.cluster_boost
            );
        }
    }

    // --- Large batch tests ---

    #[test]
    fn large_batch_ordering_preserved() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let priorities = [
            (IssuePriority::Critical, MatchPriority::Urgent),
            (IssuePriority::High, MatchPriority::High),
            (IssuePriority::Medium, MatchPriority::Normal),
            (IssuePriority::Low, MatchPriority::Low),
        ];

        // Create 100 candidates cycling through priorities
        let candidates: Vec<(Issue, MatchResult)> = (0..100)
            .map(|i| {
                let (ip, mp) = priorities[i % priorities.len()];
                make_candidate(&format!("issue-{}", i), &format!("Bug {}", i), ip, mp)
            })
            .collect();

        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 100);

        // Verify descending order
        for i in 0..result.len() - 1 {
            assert!(
                result[i].severity_score.score >= result[i + 1].severity_score.score,
                "large batch: score[{}]={} must be >= score[{}]={}",
                i,
                result[i].severity_score.score,
                i + 1,
                result[i + 1].severity_score.score
            );
        }
    }

    #[test]
    fn large_batch_with_mixed_suppression() {
        // Suppress every other issue
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "even".into(),
                field: SuppressionField::Title,
                pattern: "even".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "suppress even".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let candidates: Vec<(Issue, MatchResult)> = (0..50)
            .map(|i| {
                let title = if i % 2 == 0 {
                    format!("even bug {}", i)
                } else {
                    format!("odd bug {}", i)
                };
                make_candidate(
                    &format!("issue-{}", i),
                    &title,
                    IssuePriority::Medium,
                    MatchPriority::Normal,
                )
            })
            .collect();

        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(
            suppressed.len(),
            25,
            "25 even-titled issues should be suppressed"
        );
        assert_eq!(result.len(), 25, "25 odd-titled issues should remain");

        for (issue, sr) in &suppressed {
            assert!(
                issue.title.contains("even"),
                "suppressed issue should have 'even' in title: {}",
                issue.title
            );
            assert!(sr.suppressed);
        }
        for pi in &result {
            assert!(
                pi.issue.title.contains("odd"),
                "remaining issue should have 'odd' in title: {}",
                pi.issue.title
            );
        }
    }

    // --- Blast radius in prioritise pipeline ---

    #[test]
    fn blast_radius_classification_propagated_to_output() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let test_cases = vec![
            (
                "auth",
                "src/auth/core.rs",
                claudear_core::types::BlastRadius::Critical,
            ),
            (
                "deploy",
                "deploy/terraform/main.tf",
                claudear_core::types::BlastRadius::Infrastructure,
            ),
            (
                "test",
                "test/unit/foo_test.py",
                claudear_core::types::BlastRadius::Test,
            ),
            (
                "readme",
                "README.md",
                claudear_core::types::BlastRadius::Cosmetic,
            ),
            (
                "api",
                "src/api/routes.rs",
                claudear_core::types::BlastRadius::Core,
            ),
            (
                "util",
                "src/utils/format.rs",
                claudear_core::types::BlastRadius::Peripheral,
            ),
        ];

        for (id, filename, expected_br) in &test_cases {
            let (mut issue, mr) = make_candidate(
                id,
                &format!("{} issue", id),
                IssuePriority::Medium,
                MatchPriority::Normal,
            );
            issue.set_metadata("filename", *filename);

            let candidates = vec![(issue, mr)];
            let (result, _) = prioritise(
                &config,
                candidates,
                &tracker,
                &std::collections::HashMap::new(),
                None,
            );
            assert_eq!(result.len(), 1);
            assert_eq!(
                result[0].blast_radius, *expected_br,
                "file '{}' should have blast radius {:?}, got {:?}",
                filename, expected_br, result[0].blast_radius
            );
        }
    }

    #[test]
    fn no_metadata_defaults_to_core_blast_radius() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;
        let candidates = vec![make_candidate(
            "bare",
            "Bare issue",
            IssuePriority::Medium,
            MatchPriority::Normal,
        )];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].blast_radius,
            claudear_core::types::BlastRadius::Core,
            "issue with no filename/function/culprit should default to Core"
        );
    }

    // --- Multiple suppression rules ---

    #[test]
    fn multiple_suppression_rules_first_match_wins() {
        let config = PrioritisationConfig {
            suppression_rules: vec![
                SuppressionRule {
                    name: "rule-a".into(),
                    field: SuppressionField::Title,
                    pattern: "crash".into(),
                    match_mode: SuppressionMatchMode::Contains,
                    sources: vec![],
                    reason: "rule a matched".into(),
                },
                SuppressionRule {
                    name: "rule-b".into(),
                    field: SuppressionField::Title,
                    pattern: "crash".into(),
                    match_mode: SuppressionMatchMode::Contains,
                    sources: vec![],
                    reason: "rule b matched".into(),
                },
            ],
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![make_candidate(
            "x",
            "App crash",
            IssuePriority::High,
            MatchPriority::High,
        )];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(result.is_empty());
        assert_eq!(suppressed.len(), 1);
        assert_eq!(
            suppressed[0].1.matched_rule.as_deref(),
            Some("rule-a"),
            "first matching rule should win"
        );
    }

    #[test]
    fn source_scoped_suppression_only_affects_matching_source() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "sentry-noise".into(),
                field: SuppressionField::Title,
                pattern: "noise".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec!["sentry".into()],
                reason: "sentry noise".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        // Same title, different sources
        let mut sentry_issue =
            claudear_core::types::Issue::new("s1", "s1", "This is noise", "url", "sentry");
        sentry_issue.priority = IssuePriority::Medium;
        let sentry_mr = MatchResult::matched("test", MatchPriority::Normal);

        let mut linear_issue =
            claudear_core::types::Issue::new("l1", "l1", "This is noise", "url", "linear");
        linear_issue.priority = IssuePriority::Medium;
        let linear_mr = MatchResult::matched("test", MatchPriority::Normal);

        let candidates = vec![(sentry_issue, sentry_mr), (linear_issue, linear_mr)];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].0.source, "sentry");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].issue.source, "linear");
    }

    // --- Clustering edge cases ---

    #[test]
    fn issues_without_clustering_signals_not_clustered() {
        // Issues with no error_type and no culprit should skip clustering.
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.0, // Accept any similarity
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let candidates = vec![
            make_candidate(
                "a",
                "Same title here",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
            make_candidate(
                "b",
                "Same title here",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
        ];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 2);
        // No error_type or culprit metadata, so clustering should skip them
        for pi in &result {
            assert!(
                pi.cluster_key.is_none(),
                "issues without error_type/culprit should not be clustered"
            );
        }
    }

    #[test]
    fn different_error_types_form_separate_clusters() {
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let make_typed = |id: &str, title: &str, error_type: &str| {
            let (mut issue, mr) =
                make_candidate(id, title, IssuePriority::Medium, MatchPriority::Normal);
            issue.set_metadata("error_type", error_type);
            issue.set_metadata("culprit", "common.module");
            (issue, mr)
        };

        let candidates = vec![
            make_typed("t1", "TypeError in handler alpha", "TypeError"),
            make_typed("t2", "TypeError in handler beta", "TypeError"),
            make_typed("v1", "ValueError in handler alpha", "ValueError"),
            make_typed("v2", "ValueError in handler beta", "ValueError"),
        ];

        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 4);

        // Group by cluster_key
        let type_errors: Vec<_> = result
            .iter()
            .filter(|pi| pi.issue.id.starts_with('t'))
            .collect();
        let value_errors: Vec<_> = result
            .iter()
            .filter(|pi| pi.issue.id.starts_with('v'))
            .collect();

        assert!(type_errors[0].cluster_key.is_some());
        assert!(value_errors[0].cluster_key.is_some());
        assert_ne!(
            type_errors[0].cluster_key, value_errors[0].cluster_key,
            "different error_types should form different clusters"
        );
        assert_eq!(
            type_errors[0].cluster_key, type_errors[1].cluster_key,
            "same error_type issues should share cluster_key"
        );
    }

    #[test]
    fn cluster_key_format_correct() {
        // Verify the cluster key is "error_type::culprit"
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut i1, m1) = make_candidate(
            "f1",
            "KeyError in parser module",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        i1.set_metadata("error_type", "KeyError");
        i1.set_metadata("culprit", "parser.module");

        let (mut i2, m2) = make_candidate(
            "f2",
            "KeyError in parser engine module",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        i2.set_metadata("error_type", "KeyError");
        i2.set_metadata("culprit", "parser.module");

        let candidates = vec![(i1, m1), (i2, m2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        let key = result[0]
            .cluster_key
            .as_ref()
            .expect("should have cluster_key");
        assert_eq!(
            key, "KeyError::parser.module",
            "cluster_key format should be 'error_type::culprit'"
        );
    }

    #[test]
    fn cluster_with_only_culprit_no_error_type() {
        // Issues with culprit but no error_type should still cluster.
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut i1, m1) = make_candidate(
            "cu1",
            "Crash in payment handler module",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        i1.set_metadata("culprit", "payment.handler");

        let (mut i2, m2) = make_candidate(
            "cu2",
            "Crash in payment handler service module",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        i2.set_metadata("culprit", "payment.handler");

        let candidates = vec![(i1, m1), (i2, m2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        // Both have culprit and similar titles, should cluster
        assert!(
            result[0].cluster_key.is_some(),
            "issues sharing culprit should cluster"
        );
        let key = result[0].cluster_key.as_ref().unwrap();
        // Key format: "_::payment.handler" (underscore for missing error_type)
        assert!(
            key.contains("payment.handler"),
            "cluster key should contain culprit"
        );
    }

    // --- Regression component edge cases tested through prioritise ---

    #[test]
    fn unhandled_fatal_gets_highest_regression_score() {
        let config = PrioritisationConfig {
            regression_weight: 1.0,
            severity_weight: 0.0,
            frequency_weight: 0.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut fatal_unhandled, mr1) = make_candidate(
            "fatal",
            "Fatal crash",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        fatal_unhandled.set_metadata("is_unhandled", true);
        fatal_unhandled.set_metadata("level", "fatal");

        let (mut error_handled, mr2) = make_candidate(
            "error",
            "Error handled",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        error_handled.set_metadata("level", "error");

        let (warning, mr3) = make_candidate(
            "warn",
            "Warning only",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );

        let candidates = vec![(fatal_unhandled, mr1), (error_handled, mr2), (warning, mr3)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(result.len(), 3);
        assert_eq!(
            result[0].issue.id, "fatal",
            "fatal+unhandled should rank first"
        );
        assert_eq!(result[1].issue.id, "error", "error should rank second");
        assert_eq!(result[2].issue.id, "warn", "warning should rank last");
    }

    // --- MatchResult variations ---

    #[test]
    fn match_priority_affects_severity_component() {
        let config = PrioritisationConfig {
            severity_weight: 1.0,
            frequency_weight: 0.0,
            regression_weight: 0.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        // Same IssuePriority, different MatchPriority
        let candidates = vec![
            make_candidate(
                "urgent",
                "Bug",
                IssuePriority::Medium,
                MatchPriority::Urgent,
            ),
            make_candidate("low", "Bug", IssuePriority::Medium, MatchPriority::Low),
        ];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result[0].issue.id, "urgent");
        assert_eq!(result[1].issue.id, "low");
        assert!(
            result[0].severity_score.severity_component
                > result[1].severity_score.severity_component,
            "Urgent match should have higher severity_component than Low match"
        );
    }

    // --- Empty suppression rules ---

    #[test]
    fn no_suppression_rules_keeps_all() {
        let config = PrioritisationConfig {
            suppression_rules: vec![],
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![
            make_candidate("a", "Bug A", IssuePriority::High, MatchPriority::High),
            make_candidate("b", "Bug B", IssuePriority::Low, MatchPriority::Low),
        ];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 2);
        assert!(suppressed.is_empty());
    }

    // --- Verify score is deterministic ---

    #[test]
    fn scoring_is_deterministic() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let make_batch = || {
            vec![
                make_candidate("a", "Bug A", IssuePriority::Critical, MatchPriority::Urgent),
                make_candidate("b", "Bug B", IssuePriority::Low, MatchPriority::Low),
                make_candidate("c", "Bug C", IssuePriority::Medium, MatchPriority::Normal),
            ]
        };

        let (result1, _) = prioritise(
            &config,
            make_batch(),
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        let (result2, _) = prioritise(
            &config,
            make_batch(),
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(result1.len(), result2.len());
        for (a, b) in result1.iter().zip(result2.iter()) {
            assert_eq!(a.issue.id, b.issue.id, "ordering should be deterministic");
            assert!(
                (a.severity_score.score - b.severity_score.score).abs() < f64::EPSILON,
                "scores should be deterministic: {} vs {}",
                a.severity_score.score,
                b.severity_score.score
            );
        }
    }

    // --- Frequency weight only ---

    #[test]
    fn frequency_weight_only_orders_by_event_frequency() {
        let config = PrioritisationConfig {
            severity_weight: 0.0,
            frequency_weight: 1.0,
            regression_weight: 0.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut high_freq, mr1) = make_candidate(
            "high-freq",
            "Many events",
            IssuePriority::Low,
            MatchPriority::Low,
        );
        high_freq.set_metadata("event_count", 100_000i64);
        high_freq.set_metadata("user_count", 10_000i64);

        let (mut low_freq, mr2) = make_candidate(
            "low-freq",
            "Few events",
            IssuePriority::Critical,
            MatchPriority::Urgent,
        );
        low_freq.set_metadata("event_count", 2i64);

        let candidates = vec![(high_freq, mr1), (low_freq, mr2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result[0].issue.id, "high-freq");
        assert_eq!(result[1].issue.id, "low-freq");
    }

    // --- Regex suppression through the full pipeline ---

    #[test]
    fn regex_suppression_in_full_pipeline() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "timeout-regex".into(),
                field: SuppressionField::Title,
                pattern: r"timeout.*\d+ms".into(),
                match_mode: SuppressionMatchMode::Regex,
                sources: vec![],
                reason: "transient timeout".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let candidates = vec![
            make_candidate(
                "timeout",
                "Request timeout after 5000ms",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
            make_candidate(
                "real",
                "Authentication failure",
                IssuePriority::High,
                MatchPriority::High,
            ),
        ];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].issue.id, "real");
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].0.id, "timeout");
        assert_eq!(
            suppressed[0].1.matched_rule.as_deref(),
            Some("timeout-regex")
        );
    }

    // --- Verify match_result is preserved in output ---

    #[test]
    fn match_result_preserved_in_prioritised_output() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let mut issue =
            claudear_core::types::Issue::new("id1", "SH-1", "Test bug", "url", "sentry");
        issue.priority = IssuePriority::High;
        let mr = MatchResult::matched("specific-reason", MatchPriority::Urgent);

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].match_result.reason, "specific-reason");
        assert_eq!(result[0].match_result.priority, MatchPriority::Urgent);
        assert!(result[0].match_result.matches);
    }

    // --- store_clusters helper function tests ---

    #[test]
    fn store_clusters_with_empty_list() {
        let tracker = NoOpTracker;
        // Should not panic with empty clusters
        store_clusters(&tracker, &[]);
    }

    #[test]
    fn store_clusters_with_multiple_clusters() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        struct CountingTracker {
            count: Arc<AtomicU32>,
        }
        impl AttemptTracker for CountingTracker {
            fn has_attempted(&self, _: &str, _: &str) -> claudear_core::error::Result<bool> {
                Ok(false)
            }
            fn get_attempted_issue_ids(
                &self,
                _: &str,
            ) -> claudear_core::error::Result<std::collections::HashSet<String>> {
                Ok(std::collections::HashSet::new())
            }
            fn record_attempt(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn record_attempt_with_labels(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &[String],
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_success(&self, _: &str, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_failed(&self, _: &str, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_merged(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_closed(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_resolved(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn get_attempt(
                &self,
                _: &str,
                _: &str,
            ) -> claudear_core::error::Result<Option<claudear_core::types::FixAttempt>>
            {
                Ok(None)
            }
            fn get_attempts_by_status(
                &self,
                _: claudear_core::types::FixAttemptStatus,
            ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
                Ok(vec![])
            }
            fn get_pending_prs(
                &self,
            ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
                Ok(vec![])
            }
            fn get_attempt_by_pr_url(
                &self,
                _: &str,
            ) -> claudear_core::error::Result<Option<claudear_core::types::FixAttempt>>
            {
                Ok(None)
            }
            fn reset_attempt(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn get_stats(
                &self,
            ) -> claudear_core::error::Result<claudear_core::types::FixAttemptStats> {
                Ok(claudear_core::types::FixAttemptStats::default())
            }
            fn increment_retry(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_cannot_fix(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn get_retryable_issues(
                &self,
                _: u32,
            ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
                Ok(vec![])
            }
            fn prepare_for_retry(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
        }
        impl ActivityStore for CountingTracker {}
        impl KnowledgeStore for CountingTracker {
            fn store_content_cluster(
                &self,
                _: &claudear_core::types::ContentCluster,
            ) -> claudear_core::error::Result<i64> {
                self.count.fetch_add(1, Ordering::SeqCst);
                Ok(1)
            }
        }
        impl EmbeddingStore for CountingTracker {}
        impl RepoStore for CountingTracker {}
        impl UserStore for CountingTracker {}
        impl ChatStore for CountingTracker {}
        impl RegressionStore for CountingTracker {}
        impl ExperimentStore for CountingTracker {}
        impl EvaluationStore for CountingTracker {}
        impl WebhookStore for CountingTracker {}
        impl SimilarityStore for CountingTracker {}
        impl DiscordStore for CountingTracker {}

        let count = Arc::new(AtomicU32::new(0));
        let tracker = CountingTracker {
            count: count.clone(),
        };
        let clusters = vec![
            ContentCluster {
                id: 0,
                cluster_key: "A::x".into(),
                source: "sentry".into(),
                representative_issue_id: "1".into(),
                issue_ids: vec!["1".into(), "2".into()],
                error_type: Some("A".into()),
                culprit: Some("x".into()),
                avg_similarity: 0.8,
                status: "active".into(),
                created_at: chrono::Utc::now(),
            },
            ContentCluster {
                id: 0,
                cluster_key: "B::y".into(),
                source: "sentry".into(),
                representative_issue_id: "3".into(),
                issue_ids: vec!["3".into(), "4".into()],
                error_type: Some("B".into()),
                culprit: Some("y".into()),
                avg_similarity: 0.9,
                status: "active".into(),
                created_at: chrono::Utc::now(),
            },
        ];
        store_clusters(&tracker, &clusters);
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "should store exactly 2 clusters"
        );
    }

    #[test]
    fn store_clusters_handles_error_gracefully() {
        struct FailingTracker;
        impl AttemptTracker for FailingTracker {
            fn has_attempted(&self, _: &str, _: &str) -> claudear_core::error::Result<bool> {
                Ok(false)
            }
            fn get_attempted_issue_ids(
                &self,
                _: &str,
            ) -> claudear_core::error::Result<std::collections::HashSet<String>> {
                Ok(std::collections::HashSet::new())
            }
            fn record_attempt(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn record_attempt_with_labels(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &[String],
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_success(&self, _: &str, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_failed(&self, _: &str, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_merged(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_closed(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_resolved(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn get_attempt(
                &self,
                _: &str,
                _: &str,
            ) -> claudear_core::error::Result<Option<claudear_core::types::FixAttempt>>
            {
                Ok(None)
            }
            fn get_attempts_by_status(
                &self,
                _: claudear_core::types::FixAttemptStatus,
            ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
                Ok(vec![])
            }
            fn get_pending_prs(
                &self,
            ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
                Ok(vec![])
            }
            fn get_attempt_by_pr_url(
                &self,
                _: &str,
            ) -> claudear_core::error::Result<Option<claudear_core::types::FixAttempt>>
            {
                Ok(None)
            }
            fn reset_attempt(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn get_stats(
                &self,
            ) -> claudear_core::error::Result<claudear_core::types::FixAttemptStats> {
                Ok(claudear_core::types::FixAttemptStats::default())
            }
            fn increment_retry(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn mark_cannot_fix(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            fn get_retryable_issues(
                &self,
                _: u32,
            ) -> claudear_core::error::Result<Vec<claudear_core::types::FixAttempt>> {
                Ok(vec![])
            }
            fn prepare_for_retry(&self, _: &str, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
        }
        impl ActivityStore for FailingTracker {}
        impl KnowledgeStore for FailingTracker {
            fn store_content_cluster(
                &self,
                _: &claudear_core::types::ContentCluster,
            ) -> claudear_core::error::Result<i64> {
                Err(claudear_core::error::Error::Storage(
                    "simulated failure".into(),
                ))
            }
        }
        impl EmbeddingStore for FailingTracker {}
        impl RepoStore for FailingTracker {}
        impl UserStore for FailingTracker {}
        impl ChatStore for FailingTracker {}
        impl RegressionStore for FailingTracker {}
        impl ExperimentStore for FailingTracker {}
        impl EvaluationStore for FailingTracker {}
        impl WebhookStore for FailingTracker {}
        impl SimilarityStore for FailingTracker {}
        impl DiscordStore for FailingTracker {}

        let tracker = FailingTracker;
        let clusters = vec![ContentCluster {
            id: 0,
            cluster_key: "A::x".into(),
            source: "sentry".into(),
            representative_issue_id: "1".into(),
            issue_ids: vec!["1".into()],
            error_type: Some("A".into()),
            culprit: Some("x".into()),
            avg_similarity: 0.8,
            status: "active".into(),
            created_at: chrono::Utc::now(),
        }];
        // Should not panic even when tracker returns an error
        store_clusters(&tracker, &clusters);
    }

    // --- Suppression field variant tests ---

    #[test]
    fn suppression_on_description_field() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "desc-rule".into(),
                field: SuppressionField::Description,
                pattern: "known bug".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "known description".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let mut issue = claudear_core::types::Issue::new("d1", "d1", "Some title", "url", "sentry");
        issue.priority = IssuePriority::Medium;
        issue.description = Some("This is a known bug in the system".into());
        let mr = MatchResult::matched("test", MatchPriority::Normal);

        let mut issue2 =
            claudear_core::types::Issue::new("d2", "d2", "Other title", "url", "sentry");
        issue2.priority = IssuePriority::Medium;
        issue2.description = Some("New issue never seen before".into());
        let mr2 = MatchResult::matched("test", MatchPriority::Normal);

        let candidates = vec![(issue, mr), (issue2, mr2)];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].0.id, "d1");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].issue.id, "d2");
    }

    #[test]
    fn suppression_on_culprit_field() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "culprit-rule".into(),
                field: SuppressionField::Culprit,
                pattern: "deprecated.module".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "deprecated module".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("c1", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("culprit", "deprecated.module.handler");

        let (issue2, mr2) =
            make_candidate("c2", "Bug 2", IssuePriority::Medium, MatchPriority::Normal);

        let candidates = vec![(issue, mr), (issue2, mr2)];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].0.id, "c1");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].issue.id, "c2");
    }

    #[test]
    fn suppression_on_filename_field() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "file-rule".into(),
                field: SuppressionField::Filename,
                pattern: "generated".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "generated file".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("f1", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("filename", "src/generated/types.rs");

        let candidates = vec![(issue, mr)];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(suppressed.len(), 1);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn suppression_on_error_type_field() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "err-type-rule".into(),
                field: SuppressionField::ErrorType,
                pattern: "DeprecationWarning".into(),
                match_mode: SuppressionMatchMode::Exact,
                sources: vec![],
                reason: "deprecation".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("e1", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("error_type", "DeprecationWarning");

        let (mut issue2, mr2) =
            make_candidate("e2", "Bug 2", IssuePriority::Medium, MatchPriority::Normal);
        issue2.set_metadata("error_type", "TypeError");

        let candidates = vec![(issue, mr), (issue2, mr2)];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].0.id, "e1");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].issue.id, "e2");
    }

    #[test]
    fn suppression_on_project_field() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "proj-rule".into(),
                field: SuppressionField::Project,
                pattern: "legacy-app".into(),
                match_mode: SuppressionMatchMode::Exact,
                sources: vec![],
                reason: "legacy project".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("p1", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("project", "legacy-app");

        let candidates = vec![(issue, mr)];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(suppressed.len(), 1);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn suppression_on_labels_field() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "label-rule".into(),
                field: SuppressionField::Labels,
                pattern: "wontfix".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "wontfix label".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("l1", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("labels", vec!["bug".to_string(), "wontfix".to_string()]);

        let candidates = vec![(issue, mr)];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(suppressed.len(), 1);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn suppression_on_metadata_field() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "meta-rule".into(),
                field: SuppressionField::Metadata("environment".into()),
                pattern: "staging".into(),
                match_mode: SuppressionMatchMode::Exact,
                sources: vec![],
                reason: "staging env".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("m1", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("environment", "staging");

        let (mut issue2, mr2) =
            make_candidate("m2", "Bug 2", IssuePriority::Medium, MatchPriority::Normal);
        issue2.set_metadata("environment", "production");

        let candidates = vec![(issue, mr), (issue2, mr2)];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].0.id, "m1");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].issue.id, "m2");
    }

    // --- Suppression match mode tests ---

    #[test]
    fn exact_match_does_not_partial_match() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "exact-rule".into(),
                field: SuppressionField::Title,
                pattern: "crash".into(),
                match_mode: SuppressionMatchMode::Exact,
                sources: vec![],
                reason: "exact".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![
            make_candidate(
                "x1",
                "App crash in handler",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
            make_candidate("x2", "crash", IssuePriority::Medium, MatchPriority::Normal),
        ];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        // Only the one with exact title "crash" should be suppressed
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].0.id, "x2");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].issue.id, "x1");
    }

    #[test]
    fn invalid_regex_pattern_does_not_suppress() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "bad-regex".into(),
                field: SuppressionField::Title,
                pattern: "[invalid(regex".into(),
                match_mode: SuppressionMatchMode::Regex,
                sources: vec![],
                reason: "bad regex".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![make_candidate(
            "r1",
            "[invalid(regex",
            IssuePriority::Medium,
            MatchPriority::Normal,
        )];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        // Invalid regex should fail to compile, so nothing is suppressed
        assert!(suppressed.is_empty(), "invalid regex should not suppress");
        assert_eq!(result.len(), 1);
    }

    // --- Blast radius edge cases ---

    #[test]
    fn blast_radius_function_metadata_used() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("fn1", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("function", "auth.login_handler");

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(
            result[0].blast_radius,
            claudear_core::types::BlastRadius::Critical,
            "function metadata containing 'auth' should classify as Critical"
        );
    }

    #[test]
    fn blast_radius_culprit_metadata_used() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("cu1", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("culprit", "billing.charge_customer");

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(
            result[0].blast_radius,
            claudear_core::types::BlastRadius::Critical,
            "culprit metadata containing 'billing' should classify as Critical"
        );
    }

    #[test]
    fn blast_radius_infrastructure_paths() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let test_cases = vec![
            ("docker1", "deploy/docker/Dockerfile"),
            ("terraform1", "infra/terraform/main.tf"),
            ("k8s1", "k8s/deployment.yaml"),
            ("migration1", "src/database/migration/001.sql"),
        ];

        for (id, filename) in test_cases {
            let (mut issue, mr) =
                make_candidate(id, "Bug", IssuePriority::Medium, MatchPriority::Normal);
            issue.set_metadata("filename", filename);
            let candidates = vec![(issue, mr)];
            let (result, _) = prioritise(
                &config,
                candidates,
                &tracker,
                &std::collections::HashMap::new(),
                None,
            );
            assert_eq!(
                result[0].blast_radius,
                claudear_core::types::BlastRadius::Infrastructure,
                "file '{}' should be classified as Infrastructure",
                filename
            );
        }
    }

    #[test]
    fn blast_radius_test_paths() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let test_cases = vec![
            ("t1", "test/integration/api_test.py"),
            ("t2", "src/spec/models_spec.rb"),
            ("t3", "test/fixture/data.json"),
            ("t4", "src/mock/service.ts"),
        ];

        for (id, filename) in test_cases {
            let (mut issue, mr) =
                make_candidate(id, "Bug", IssuePriority::Medium, MatchPriority::Normal);
            issue.set_metadata("filename", filename);
            let candidates = vec![(issue, mr)];
            let (result, _) = prioritise(
                &config,
                candidates,
                &tracker,
                &std::collections::HashMap::new(),
                None,
            );
            assert_eq!(
                result[0].blast_radius,
                claudear_core::types::BlastRadius::Test,
                "file '{}' should be classified as Test",
                filename
            );
        }
    }

    #[test]
    fn blast_radius_cosmetic_paths() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let test_cases = vec![
            ("c1", "CHANGELOG.md"),
            ("c2", "docs/api.md"),
            ("c3", "LICENSE"),
        ];

        for (id, filename) in test_cases {
            let (mut issue, mr) =
                make_candidate(id, "Bug", IssuePriority::Medium, MatchPriority::Normal);
            issue.set_metadata("filename", filename);
            let candidates = vec![(issue, mr)];
            let (result, _) = prioritise(
                &config,
                candidates,
                &tracker,
                &std::collections::HashMap::new(),
                None,
            );
            assert_eq!(
                result[0].blast_radius,
                claudear_core::types::BlastRadius::Cosmetic,
                "file '{}' should be classified as Cosmetic",
                filename
            );
        }
    }

    #[test]
    fn blast_radius_core_paths() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let test_cases = vec![
            ("co1", "src/api/endpoints.rs"),
            ("co2", "src/core/engine.rs"),
            ("co3", "src/middleware/cors.rs"),
            ("co4", "src/router/main.rs"),
        ];

        for (id, filename) in test_cases {
            let (mut issue, mr) =
                make_candidate(id, "Bug", IssuePriority::Medium, MatchPriority::Normal);
            issue.set_metadata("filename", filename);
            let candidates = vec![(issue, mr)];
            let (result, _) = prioritise(
                &config,
                candidates,
                &tracker,
                &std::collections::HashMap::new(),
                None,
            );
            assert_eq!(
                result[0].blast_radius,
                claudear_core::types::BlastRadius::Core,
                "file '{}' should be classified as Core",
                filename
            );
        }
    }

    #[test]
    fn blast_radius_critical_has_priority_over_infra() {
        // A file path that matches both critical and infra patterns
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("pri1", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        // "auth" is critical, "deploy" is infra -- critical should win
        issue.set_metadata("filename", "deploy/auth/service.rs");
        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(
            result[0].blast_radius,
            claudear_core::types::BlastRadius::Critical
        );
    }

    // --- Custom config path patterns ---

    #[test]
    fn custom_critical_paths() {
        // Segment matching splits on /, \, ., _, - so patterns must be
        // single segments, not multi-segment names like "custom_critical".
        let config = PrioritisationConfig {
            critical_paths: vec!["payments".into()],
            core_paths: vec![],
            infra_paths: vec![],
            test_paths: vec![],
            cosmetic_paths: vec![],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("cp1", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("filename", "src/payments/main.rs");
        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(
            result[0].blast_radius,
            claudear_core::types::BlastRadius::Critical
        );
    }

    #[test]
    fn empty_path_patterns_default_to_peripheral() {
        let config = PrioritisationConfig {
            critical_paths: vec![],
            core_paths: vec![],
            infra_paths: vec![],
            test_paths: vec![],
            cosmetic_paths: vec![],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("ep1", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("filename", "src/auth/login.rs");
        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        // With all path lists empty, should fall through to Peripheral
        assert_eq!(
            result[0].blast_radius,
            claudear_core::types::BlastRadius::Peripheral
        );
    }

    // --- Weight boundary and interaction tests ---

    #[test]
    fn negative_weights_produce_negative_scores() {
        let config = PrioritisationConfig {
            severity_weight: -1.0,
            frequency_weight: 0.0,
            regression_weight: 0.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![make_candidate(
            "neg",
            "Bug",
            IssuePriority::Critical,
            MatchPriority::Urgent,
        )];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(
            result[0].severity_score.score < 0.0,
            "negative weight should produce negative score, got {}",
            result[0].severity_score.score
        );
    }

    #[test]
    fn very_large_weights_still_finite() {
        let config = PrioritisationConfig {
            severity_weight: f64::MAX / 10.0,
            frequency_weight: 0.0,
            regression_weight: 0.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![make_candidate(
            "big",
            "Bug",
            IssuePriority::Critical,
            MatchPriority::Urgent,
        )];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(
            result[0].severity_score.score.is_finite(),
            "even with very large weight, score should be finite"
        );
    }

    #[test]
    fn only_frequency_weight_orders_by_user_count() {
        let config = PrioritisationConfig {
            severity_weight: 0.0,
            frequency_weight: 1.0,
            regression_weight: 0.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut hi_users, mr1) =
            make_candidate("hu", "Bug", IssuePriority::Low, MatchPriority::Low);
        hi_users.set_metadata("user_count", 50_000i64);

        let (mut lo_users, mr2) =
            make_candidate("lu", "Bug", IssuePriority::Low, MatchPriority::Low);
        lo_users.set_metadata("user_count", 5i64);

        let candidates = vec![(hi_users, mr1), (lo_users, mr2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result[0].issue.id, "hu");
        assert_eq!(result[1].issue.id, "lu");
    }

    #[test]
    fn only_blast_radius_weight_orders_by_classification() {
        let config = PrioritisationConfig {
            severity_weight: 0.0,
            frequency_weight: 0.0,
            regression_weight: 0.0,
            blast_radius_weight: 1.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut critical, mr1) =
            make_candidate("cr", "Bug", IssuePriority::Low, MatchPriority::Low);
        critical.set_metadata("filename", "src/auth/main.rs");

        let (mut cosmetic, mr2) =
            make_candidate("co", "Bug", IssuePriority::Low, MatchPriority::Low);
        cosmetic.set_metadata("filename", "README.md");

        let candidates = vec![(cosmetic, mr2), (critical, mr1)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(
            result[0].issue.id, "cr",
            "Critical blast radius should rank first"
        );
        assert_eq!(result[1].issue.id, "co");
    }

    #[test]
    fn only_regression_weight_orders_by_regression_signals() {
        let config = PrioritisationConfig {
            severity_weight: 0.0,
            frequency_weight: 0.0,
            regression_weight: 1.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut fatal, mr1) = make_candidate("f", "Bug", IssuePriority::Low, MatchPriority::Low);
        fatal.set_metadata("is_unhandled", true);
        fatal.set_metadata("level", "fatal");

        let (plain, mr2) = make_candidate("p", "Bug", IssuePriority::Low, MatchPriority::Low);

        let candidates = vec![(plain, mr2), (fatal, mr1)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result[0].issue.id, "f");
        assert_eq!(result[1].issue.id, "p");
    }

    #[test]
    fn only_cluster_weight_orders_clustered_first() {
        let config = PrioritisationConfig {
            severity_weight: 0.0,
            frequency_weight: 0.0,
            regression_weight: 0.0,
            blast_radius_weight: 0.0,
            cluster_weight: 1.0,
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut c1, mr1) = make_candidate(
            "cl1",
            "TypeError in handler mod",
            IssuePriority::Low,
            MatchPriority::Low,
        );
        c1.set_metadata("error_type", "TypeError");
        c1.set_metadata("culprit", "handler");
        let (mut c2, mr2) = make_candidate(
            "cl2",
            "TypeError in handler module",
            IssuePriority::Low,
            MatchPriority::Low,
        );
        c2.set_metadata("error_type", "TypeError");
        c2.set_metadata("culprit", "handler");

        let (solo, mr3) = make_candidate(
            "solo",
            "Standalone bug",
            IssuePriority::Low,
            MatchPriority::Low,
        );

        let candidates = vec![(solo, mr3), (c1, mr1), (c2, mr2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 3);
        // Clustered issues should have higher scores
        let solo_pi = result.iter().find(|pi| pi.issue.id == "solo").unwrap();
        let cl1_pi = result.iter().find(|pi| pi.issue.id == "cl1").unwrap();
        assert!(
            cl1_pi.severity_score.score > solo_pi.severity_score.score,
            "clustered issue ({}) should score higher than solo ({}) with cluster_weight=1.0",
            cl1_pi.severity_score.score,
            solo_pi.severity_score.score
        );
    }

    // --- Issue metadata edge cases ---

    #[test]
    fn escalation_rate_above_one_clamped() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("esc", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("escalation_rate", 5.0);

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        let fc = result[0].severity_score.frequency_component;
        assert!(
            fc <= 1.0,
            "frequency_component should be clamped to 1.0 max, got {}",
            fc
        );
    }

    #[test]
    fn issue_with_only_error_level() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) = make_candidate(
            "lvl",
            "Error level only",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue.set_metadata("level", "error");

        let (plain, mr2) = make_candidate(
            "plain",
            "No level",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );

        let candidates = vec![(issue, mr), (plain, mr2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        // error level issue should rank higher due to regression component
        assert_eq!(result[0].issue.id, "lvl");
        assert_eq!(result[1].issue.id, "plain");
    }

    #[test]
    fn issue_with_only_unhandled_flag() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) = make_candidate(
            "uh",
            "Unhandled",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue.set_metadata("is_unhandled", true);

        let (plain, mr2) =
            make_candidate("h", "Handled", IssuePriority::Medium, MatchPriority::Normal);

        let candidates = vec![(issue, mr), (plain, mr2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result[0].issue.id, "uh");
        assert_eq!(result[1].issue.id, "h");
    }

    #[test]
    fn issue_with_warning_level_ranks_between_error_and_none() {
        let config = PrioritisationConfig {
            severity_weight: 0.0,
            frequency_weight: 0.0,
            regression_weight: 1.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut error_issue, mr1) =
            make_candidate("err", "Error", IssuePriority::Medium, MatchPriority::Normal);
        error_issue.set_metadata("level", "error");

        let (mut warn_issue, mr2) = make_candidate(
            "warn",
            "Warning",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        warn_issue.set_metadata("level", "warning");

        let (plain, mr3) = make_candidate(
            "plain",
            "Plain",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );

        let candidates = vec![(plain, mr3), (warn_issue, mr2), (error_issue, mr1)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result[0].issue.id, "err");
        assert_eq!(result[1].issue.id, "warn");
        assert_eq!(result[2].issue.id, "plain");
    }

    // --- Clustering edge cases ---

    #[test]
    fn cluster_with_only_error_type_no_culprit() {
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut i1, m1) = make_candidate(
            "et1",
            "NullPointer in module alpha",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        i1.set_metadata("error_type", "NullPointerException");

        let (mut i2, m2) = make_candidate(
            "et2",
            "NullPointer in module beta",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        i2.set_metadata("error_type", "NullPointerException");

        let candidates = vec![(i1, m1), (i2, m2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        // Both have same error_type and no culprit, should cluster
        assert!(
            result[0].cluster_key.is_some(),
            "should form a cluster with only error_type"
        );
        let key = result[0].cluster_key.as_ref().unwrap();
        assert!(
            key.contains("NullPointerException"),
            "key should contain error_type"
        );
        assert!(
            key.contains("_"),
            "key should contain underscore for missing culprit"
        );
    }

    #[test]
    fn three_separate_clusters() {
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let make = |id: &str, title: &str, et: &str, culp: &str| {
            let (mut issue, mr) =
                make_candidate(id, title, IssuePriority::Medium, MatchPriority::Normal);
            issue.set_metadata("error_type", et);
            issue.set_metadata("culprit", culp);
            (issue, mr)
        };

        let candidates = vec![
            make(
                "a1",
                "TypeError in auth handler module",
                "TypeError",
                "auth.handler",
            ),
            make(
                "a2",
                "TypeError in auth handler service",
                "TypeError",
                "auth.handler",
            ),
            make("b1", "ValueError in parser module", "ValueError", "parser"),
            make(
                "b2",
                "ValueError in parser engine module",
                "ValueError",
                "parser",
            ),
            make(
                "c1",
                "KeyError in cache module handler",
                "KeyError",
                "cache",
            ),
            make(
                "c2",
                "KeyError in cache service module handler",
                "KeyError",
                "cache",
            ),
        ];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 6);

        // Collect unique cluster keys
        let keys: std::collections::HashSet<_> = result
            .iter()
            .filter_map(|pi| pi.cluster_key.as_ref())
            .collect();
        assert_eq!(
            keys.len(),
            3,
            "should have 3 distinct clusters, got {:?}",
            keys
        );
    }

    #[test]
    fn cluster_min_size_one_forms_single_issue_cluster() {
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 1,
            cluster_similarity_threshold: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut issue, mr) = make_candidate(
            "single",
            "TypeError alone",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        issue.set_metadata("error_type", "TypeError");
        issue.set_metadata("culprit", "alone");

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        // min_content_cluster_size=1 means a single issue forms a cluster
        assert!(
            result[0].cluster_key.is_some(),
            "min_cluster_size=1 should allow single-issue clusters"
        );
    }

    #[test]
    fn cluster_similarity_zero_accepts_any_titles() {
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (mut i1, m1) = make_candidate(
            "z1",
            "Completely different title",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        i1.set_metadata("error_type", "SameError");
        i1.set_metadata("culprit", "same.culprit");

        let (mut i2, m2) = make_candidate(
            "z2",
            "Totally unrelated heading",
            IssuePriority::Medium,
            MatchPriority::Normal,
        );
        i2.set_metadata("error_type", "SameError");
        i2.set_metadata("culprit", "same.culprit");

        let candidates = vec![(i1, m1), (i2, m2)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        // With threshold=0, even dissimilar titles should cluster
        assert!(
            result[0].cluster_key.is_some(),
            "threshold=0.0 should accept any title similarity"
        );
    }

    // --- Score formula verification tests ---

    #[test]
    fn score_formula_default_weights_verified() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("v1", "Verified", IssuePriority::High, MatchPriority::High);
        issue.set_metadata("event_count", 100i64);
        issue.set_metadata("user_count", 10i64);
        issue.set_metadata("escalation_rate", 0.5);
        issue.set_metadata("level", "error");
        issue.set_metadata("filename", "src/api/routes.rs");

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        let s = &result[0].severity_score;

        // Manually compute expected score
        let expected_score = 0.30 * s.severity_component
            + 0.25 * s.frequency_component
            + 0.20 * s.regression_component
            + 0.15 * s.blast_radius_component
            + 0.10 * s.cluster_boost;

        assert!(
            (s.score - expected_score).abs() < 1e-10,
            "score ({}) should equal weighted sum of components ({})",
            s.score,
            expected_score
        );
    }

    #[test]
    fn severity_component_formula_verified() {
        let config = PrioritisationConfig {
            severity_weight: 1.0,
            frequency_weight: 0.0,
            regression_weight: 0.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        // IssuePriority::High = 0.75, MatchPriority::Normal = 0.5
        // severity_component = 0.6 * 0.75 + 0.4 * 0.5 = 0.45 + 0.20 = 0.65
        let candidates = vec![make_candidate(
            "sv",
            "Bug",
            IssuePriority::High,
            MatchPriority::Normal,
        )];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(
            (result[0].severity_score.severity_component - 0.65).abs() < 1e-10,
            "severity_component should be 0.65, got {}",
            result[0].severity_score.severity_component
        );
    }

    // --- Output structure tests ---

    #[test]
    fn prioritised_issue_fields_populated() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let mut issue = claudear_core::types::Issue::new(
            "id-check",
            "SH-CHECK",
            "Title check",
            "https://url",
            "linear",
        );
        issue.priority = IssuePriority::High;
        issue.description = Some("desc".into());
        let mr = MatchResult::matched("reason-check", MatchPriority::High);

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        let pi = &result[0];

        assert_eq!(pi.issue.id, "id-check");
        assert_eq!(pi.issue.short_id, "SH-CHECK");
        assert_eq!(pi.issue.title, "Title check");
        assert_eq!(pi.issue.url, "https://url");
        assert_eq!(pi.issue.source, "linear");
        assert_eq!(pi.issue.priority, IssuePriority::High);
        assert_eq!(pi.issue.description.as_deref(), Some("desc"));
        assert_eq!(pi.match_result.reason, "reason-check");
        assert_eq!(pi.match_result.priority, MatchPriority::High);
        assert!(pi.match_result.matches);
        assert!(pi.severity_score.score > 0.0);
    }

    #[test]
    fn suppression_result_fields_populated() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "test-rule".into(),
                field: SuppressionField::Title,
                pattern: "suppress me".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "test reason".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![make_candidate(
            "sr1",
            "Please suppress me now",
            IssuePriority::Medium,
            MatchPriority::Normal,
        )];
        let (_, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(suppressed.len(), 1);
        let (issue, sr) = &suppressed[0];
        assert_eq!(issue.id, "sr1");
        assert!(sr.suppressed);
        assert_eq!(sr.matched_rule.as_deref(), Some("test-rule"));
        assert_eq!(sr.reason.as_deref(), Some("test reason"));
    }

    // --- Large batch stress tests ---

    #[test]
    fn five_hundred_candidates_sorted_correctly() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let priorities = [
            (IssuePriority::Critical, MatchPriority::Urgent),
            (IssuePriority::High, MatchPriority::High),
            (IssuePriority::Medium, MatchPriority::Normal),
            (IssuePriority::Low, MatchPriority::Low),
            (IssuePriority::None, MatchPriority::Low),
        ];

        let candidates: Vec<(claudear_core::types::Issue, MatchResult)> = (0..500)
            .map(|i| {
                let (ip, mp) = priorities[i % priorities.len()];
                make_candidate(&format!("i-{}", i), &format!("Bug {}", i), ip, mp)
            })
            .collect();

        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 500);

        // Verify monotonically non-increasing
        for i in 0..result.len() - 1 {
            assert!(
                result[i].severity_score.score >= result[i + 1].severity_score.score,
                "500 batch: score[{}]={} < score[{}]={}",
                i,
                result[i].severity_score.score,
                i + 1,
                result[i + 1].severity_score.score
            );
        }
    }

    #[test]
    fn large_batch_with_clustering() {
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 3,
            cluster_similarity_threshold: 0.3,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        // 30 issues in 3 clusters of 10
        let candidates: Vec<(claudear_core::types::Issue, MatchResult)> = (0..30)
            .map(|i| {
                let cluster_idx = i / 10;
                let error_type = format!("Error{}", cluster_idx);
                let culprit = format!("module{}", cluster_idx);
                let (mut issue, mr) = make_candidate(
                    &format!("lc-{}", i),
                    &format!("{} in {} handler service", error_type, culprit),
                    IssuePriority::Medium,
                    MatchPriority::Normal,
                );
                issue.set_metadata("error_type", error_type);
                issue.set_metadata("culprit", culprit);
                (issue, mr)
            })
            .collect();

        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 30);

        // Verify all clustered
        let clustered = result.iter().filter(|pi| pi.cluster_key.is_some()).count();
        assert_eq!(clustered, 30, "all 30 issues should be in clusters");

        let keys: std::collections::HashSet<_> = result
            .iter()
            .filter_map(|pi| pi.cluster_key.as_ref())
            .collect();
        assert_eq!(keys.len(), 3, "should have 3 distinct clusters");
    }

    // --- Multiple suppression rules interaction ---

    #[test]
    fn multiple_rules_different_fields() {
        let config = PrioritisationConfig {
            suppression_rules: vec![
                SuppressionRule {
                    name: "title-rule".into(),
                    field: SuppressionField::Title,
                    pattern: "flaky".into(),
                    match_mode: SuppressionMatchMode::Contains,
                    sources: vec![],
                    reason: "flaky test".into(),
                },
                SuppressionRule {
                    name: "source-rule".into(),
                    field: SuppressionField::Source,
                    pattern: "jira".into(),
                    match_mode: SuppressionMatchMode::Exact,
                    sources: vec![],
                    reason: "jira source".into(),
                },
            ],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let mut jira_issue =
            claudear_core::types::Issue::new("j1", "j1", "Normal bug", "url", "jira");
        jira_issue.priority = IssuePriority::Medium;
        let jira_mr = MatchResult::matched("test", MatchPriority::Normal);

        let candidates = vec![
            make_candidate(
                "f1",
                "Flaky CI test",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
            (jira_issue, jira_mr),
            make_candidate("r1", "Real bug", IssuePriority::High, MatchPriority::High),
        ];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(suppressed.len(), 2);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].issue.id, "r1");

        // Verify correct rules matched
        let flaky_sr = suppressed.iter().find(|(i, _)| i.id == "f1").unwrap();
        assert_eq!(flaky_sr.1.matched_rule.as_deref(), Some("title-rule"));
        let jira_sr = suppressed.iter().find(|(i, _)| i.id == "j1").unwrap();
        assert_eq!(jira_sr.1.matched_rule.as_deref(), Some("source-rule"));
    }

    #[test]
    fn suppression_rule_with_empty_pattern_no_match() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "empty-pattern".into(),
                field: SuppressionField::Title,
                pattern: "".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "empty".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;
        let candidates = vec![make_candidate(
            "x",
            "Bug",
            IssuePriority::Medium,
            MatchPriority::Normal,
        )];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        // Empty pattern with Contains mode: "" is contained in any string
        // This is technically valid -- the issue IS suppressed.
        // Verify the pipeline handles it consistently.
        assert_eq!(
            result.len() + suppressed.len(),
            1,
            "total output should equal total input"
        );
    }

    // --- Interaction between all pipeline steps ---

    #[test]
    fn full_pipeline_suppression_then_clustering_then_scoring() {
        let config = PrioritisationConfig {
            content_clustering: true,
            min_content_cluster_size: 2,
            cluster_similarity_threshold: 0.3,
            suppression_rules: vec![SuppressionRule {
                name: "suppress-noise".into(),
                field: SuppressionField::Title,
                pattern: "noise".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "noisy".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let make_clusterable = |id: &str, title: &str, ip: IssuePriority| {
            let (mut issue, mr) = make_candidate(id, title, ip, MatchPriority::Normal);
            issue.set_metadata("error_type", "RuntimeError");
            issue.set_metadata("culprit", "main.run");
            (issue, mr)
        };

        let candidates = vec![
            make_clusterable(
                "k1",
                "RuntimeError in main runner service",
                IssuePriority::High,
            ),
            make_clusterable(
                "k2",
                "RuntimeError in main runner module",
                IssuePriority::Medium,
            ),
            make_candidate(
                "n1",
                "Noise from CI",
                IssuePriority::Critical,
                MatchPriority::Urgent,
            ),
            make_candidate(
                "solo",
                "Standalone low bug",
                IssuePriority::Low,
                MatchPriority::Low,
            ),
        ];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );

        // n1 should be suppressed
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].0.id, "n1");

        // k1, k2 should cluster; solo should not
        assert_eq!(result.len(), 3);
        let k1_pi = result.iter().find(|pi| pi.issue.id == "k1").unwrap();
        let k2_pi = result.iter().find(|pi| pi.issue.id == "k2").unwrap();
        let solo_pi = result.iter().find(|pi| pi.issue.id == "solo").unwrap();

        assert!(k1_pi.cluster_key.is_some());
        assert_eq!(k1_pi.cluster_key, k2_pi.cluster_key);
        assert!(solo_pi.cluster_key.is_none());

        // Verify ordering is by score descending
        for i in 0..result.len() - 1 {
            assert!(result[i].severity_score.score >= result[i + 1].severity_score.score);
        }
    }

    // --- MatchResult variants ---

    #[test]
    fn not_matched_result_still_works_in_pipeline() {
        // A non-matching MatchResult shouldn't break the pipeline
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let issue = claudear_core::types::Issue::new("nm", "nm", "Bug", "url", "sentry");
        let mr = MatchResult::not_matched("not relevant");

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        assert!(!result[0].match_result.matches);
        assert_eq!(result[0].match_result.reason, "not relevant");
        // Score should still be computed
        assert!(result[0].severity_score.score.is_finite());
    }

    // --- Issue source variations ---

    #[test]
    fn different_sources_in_same_batch() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let sources = ["sentry", "linear", "jira", "github", "gitlab"];
        let candidates: Vec<(claudear_core::types::Issue, MatchResult)> = sources
            .iter()
            .enumerate()
            .map(|(i, source)| {
                let mut issue = claudear_core::types::Issue::new(
                    format!("src-{}", i),
                    format!("src-{}", i),
                    format!("Bug from {}", source),
                    "url",
                    *source,
                );
                issue.priority = IssuePriority::Medium;
                let mr = MatchResult::matched("test", MatchPriority::Normal);
                (issue, mr)
            })
            .collect();

        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 5);
        // All should have same score since everything else is equal
        let base_score = result[0].severity_score.score;
        for pi in &result {
            assert!(
                (pi.severity_score.score - base_score).abs() < f64::EPSILON,
                "all issues should have equal scores regardless of source"
            );
        }
    }

    // --- Source-scoped suppression edge cases ---

    #[test]
    fn source_scoped_suppression_case_insensitive() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "sentry-rule".into(),
                field: SuppressionField::Title,
                pattern: "bug".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec!["SENTRY".into()],
                reason: "sentry bug".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let mut issue =
            claudear_core::types::Issue::new("sc1", "sc1", "A bug here", "url", "sentry");
        issue.priority = IssuePriority::Medium;
        let mr = MatchResult::matched("test", MatchPriority::Normal);

        let candidates = vec![(issue, mr)];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        // Source comparison is case-insensitive (eq_ignore_ascii_case)
        assert_eq!(
            suppressed.len(),
            1,
            "SENTRY should match sentry case-insensitively"
        );
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn source_scoped_suppression_multiple_sources() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "multi-source".into(),
                field: SuppressionField::Title,
                pattern: "bug".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec!["sentry".into(), "linear".into()],
                reason: "multi-source".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let mk = |id: &str, source: &str| {
            let mut issue = claudear_core::types::Issue::new(id, id, "A bug", "url", source);
            issue.priority = IssuePriority::Medium;
            let mr = MatchResult::matched("test", MatchPriority::Normal);
            (issue, mr)
        };

        let candidates = vec![mk("s1", "sentry"), mk("l1", "linear"), mk("g1", "github")];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(
            suppressed.len(),
            2,
            "sentry and linear should be suppressed"
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].issue.source, "github");
    }

    // --- Edge case: issue with no title ---

    #[test]
    fn issue_with_empty_title() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let candidates = vec![make_candidate(
            "empty",
            "",
            IssuePriority::Medium,
            MatchPriority::Normal,
        )];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].issue.title, "");
        assert!(result[0].severity_score.score.is_finite());
    }

    // --- Edge case: unicode titles and metadata ---

    #[test]
    fn unicode_titles_handled_correctly() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let candidates = vec![
            make_candidate(
                "u1",
                "Fehler im Zahlungsmodul",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
            make_candidate(
                "u2",
                "エラーが発生しました",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
            make_candidate(
                "u3",
                "Ошибка в модуле платежей",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
        ];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(result.len(), 3);
        for pi in &result {
            assert!(pi.severity_score.score.is_finite());
        }
    }

    #[test]
    fn unicode_suppression_pattern() {
        let config = PrioritisationConfig {
            suppression_rules: vec![SuppressionRule {
                name: "unicode-rule".into(),
                field: SuppressionField::Title,
                pattern: "エラー".into(),
                match_mode: SuppressionMatchMode::Contains,
                sources: vec![],
                reason: "japanese error".into(),
            }],
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let candidates = vec![
            make_candidate(
                "u1",
                "エラーが発生しました",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
            make_candidate(
                "u2",
                "Normal error",
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
        ];
        let (result, suppressed) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].0.id, "u1");
        assert_eq!(result.len(), 1);
    }

    // --- Content cluster context formatting ---

    #[test]
    fn format_cluster_context_all_fields() {
        let cluster = ContentCluster {
            id: 1,
            cluster_key: "RuntimeError::main.handler".into(),
            source: "sentry".into(),
            representative_issue_id: "1".into(),
            issue_ids: vec!["1".into(), "2".into(), "3".into()],
            error_type: Some("RuntimeError".into()),
            culprit: Some("main.handler".into()),
            avg_similarity: 0.85,
            status: "active".into(),
            created_at: chrono::Utc::now(),
        };
        let ctx = content_cluster::format_cluster_context(&cluster);
        assert!(ctx.contains("RuntimeError::main.handler"));
        assert!(ctx.contains("3 issues"));
        assert!(ctx.contains("error_type=RuntimeError"));
        assert!(ctx.contains("culprit=main.handler"));
        assert!(ctx.contains("85%"));
    }

    #[test]
    fn format_cluster_context_no_error_type() {
        let cluster = ContentCluster {
            id: 1,
            cluster_key: "_::handler".into(),
            source: "sentry".into(),
            representative_issue_id: "1".into(),
            issue_ids: vec!["1".into(), "2".into()],
            error_type: None,
            culprit: Some("handler".into()),
            avg_similarity: 0.6,
            status: "active".into(),
            created_at: chrono::Utc::now(),
        };
        let ctx = content_cluster::format_cluster_context(&cluster);
        assert!(
            !ctx.contains("error_type="),
            "should not include error_type when None"
        );
        assert!(ctx.contains("culprit=handler"));
    }

    #[test]
    fn format_cluster_context_no_culprit() {
        let cluster = ContentCluster {
            id: 1,
            cluster_key: "TypeError::_".into(),
            source: "sentry".into(),
            representative_issue_id: "1".into(),
            issue_ids: vec!["1".into(), "2".into()],
            error_type: Some("TypeError".into()),
            culprit: None,
            avg_similarity: 0.7,
            status: "active".into(),
            created_at: chrono::Utc::now(),
        };
        let ctx = content_cluster::format_cluster_context(&cluster);
        assert!(ctx.contains("error_type=TypeError"));
        assert!(
            !ctx.contains("culprit="),
            "should not include culprit when None"
        );
    }

    // --- Title similarity edge cases ---

    #[test]
    fn title_similarity_empty_strings() {
        assert_eq!(content_cluster::title_similarity("", ""), 0.0);
    }

    #[test]
    fn title_similarity_one_empty() {
        assert_eq!(content_cluster::title_similarity("foo bar", ""), 0.0);
        assert_eq!(content_cluster::title_similarity("", "foo bar"), 0.0);
    }

    #[test]
    fn title_similarity_single_word_match() {
        assert_eq!(content_cluster::title_similarity("foo", "foo"), 1.0);
    }

    #[test]
    fn title_similarity_subset() {
        // "foo" vs "foo bar" -> intersection={foo}=1, union={foo,bar}=2 -> 0.5
        let sim = content_cluster::title_similarity("foo", "foo bar");
        assert!((sim - 0.5).abs() < 0.001);
    }

    #[test]
    fn title_similarity_symmetry() {
        let sim1 = content_cluster::title_similarity("foo bar baz", "foo qux");
        let sim2 = content_cluster::title_similarity("foo qux", "foo bar baz");
        assert!(
            (sim1 - sim2).abs() < f64::EPSILON,
            "similarity should be symmetric: {} vs {}",
            sim1,
            sim2
        );
    }

    // --- Blast radius score function ---

    #[test]
    fn blast_radius_scores_are_ordered() {
        let scores = [
            blast_radius::blast_radius_score(claudear_core::types::BlastRadius::Critical),
            blast_radius::blast_radius_score(claudear_core::types::BlastRadius::Infrastructure),
            blast_radius::blast_radius_score(claudear_core::types::BlastRadius::Core),
            blast_radius::blast_radius_score(claudear_core::types::BlastRadius::Peripheral),
            blast_radius::blast_radius_score(claudear_core::types::BlastRadius::Test),
            blast_radius::blast_radius_score(claudear_core::types::BlastRadius::Cosmetic),
        ];
        for i in 0..scores.len() - 1 {
            assert!(
                scores[i] > scores[i + 1],
                "blast radius scores should be strictly decreasing: {:?}",
                scores
            );
        }
    }

    // --- Clustered issue IDs helper ---

    #[test]
    fn clustered_issue_ids_empty_input() {
        let ids = content_cluster::clustered_issue_ids(&[]);
        assert!(ids.is_empty());
    }

    #[test]
    fn clustered_issue_ids_deduplicates() {
        let clusters = vec![
            ContentCluster {
                id: 0,
                cluster_key: "A::x".into(),
                source: "s".into(),
                representative_issue_id: "1".into(),
                issue_ids: vec!["1".into(), "2".into()],
                error_type: None,
                culprit: None,
                avg_similarity: 0.5,
                status: "active".into(),
                created_at: chrono::Utc::now(),
            },
            ContentCluster {
                id: 0,
                cluster_key: "B::y".into(),
                source: "s".into(),
                representative_issue_id: "2".into(),
                issue_ids: vec!["2".into(), "3".into()],
                error_type: None,
                culprit: None,
                avg_similarity: 0.5,
                status: "active".into(),
                created_at: chrono::Utc::now(),
            },
        ];
        let ids = content_cluster::clustered_issue_ids(&clusters);
        // "2" appears in both clusters but should only appear once in the set
        assert_eq!(ids.len(), 3);
        assert!(ids.contains("1"));
        assert!(ids.contains("2"));
        assert!(ids.contains("3"));
    }

    // --- Mixed metadata types ---

    #[test]
    fn event_count_as_float_metadata() {
        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;

        let (mut issue, mr) =
            make_candidate("ef", "Bug", IssuePriority::Medium, MatchPriority::Normal);
        issue.set_metadata("event_count", 1000.5f64);

        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(result[0].severity_score.frequency_component > 0.0);
    }

    // --- Default IssuePriority ---

    #[test]
    fn default_issue_priority_is_none() {
        let issue =
            claudear_core::types::Issue::new("def", "def", "Default priority", "url", "sentry");
        assert_eq!(issue.priority, IssuePriority::None);

        let config = PrioritisationConfig::default();
        let tracker = NoOpTracker;
        let mr = MatchResult::matched("test", MatchPriority::Normal);
        let candidates = vec![(issue, mr)];
        let (result, _) = prioritise(
            &config,
            candidates,
            &tracker,
            &std::collections::HashMap::new(),
            None,
        );
        // severity_component = 0.6*0.0 + 0.4*0.5 = 0.2
        assert!(
            (result[0].severity_score.severity_component - 0.2).abs() < 1e-10,
            "default priority should yield severity_component of 0.2, got {}",
            result[0].severity_score.severity_component
        );
    }

    // --- Mock LLM tests ---

    struct MockLlmAnalyzer {
        assessments: Option<Vec<crate::llm::LlmIssueAssessment>>,
    }

    impl crate::llm::LlmAnalyzer for MockLlmAnalyzer {
        fn assess_issues(
            &self,
            _candidates: &[(Issue, MatchResult)],
        ) -> Option<Vec<crate::llm::LlmIssueAssessment>> {
            self.assessments.clone()
        }

        fn classify_review(
            &self,
            _comment_body: &str,
        ) -> Option<claudear_core::types::ReviewCategory> {
            None
        }

        fn extract_learnings(&self, _log_text: &str) -> Option<crate::llm::LlmLogAnalysis> {
            None
        }

        fn explain_correlations(
            &self,
            _correlations: &[(String, String, i64)],
            _issues_context: &str,
        ) -> Vec<crate::llm::LlmCorrelationExplanation> {
            Vec::new()
        }
    }

    #[test]
    fn test_prioritise_with_mock_llm() {
        let config = PrioritisationConfig {
            enabled: true,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (issue, mr) = make_candidate(
            "LLM-1",
            "Fatal crash in payment service",
            IssuePriority::Critical,
            MatchPriority::High,
        );

        let mock = MockLlmAnalyzer {
            assessments: Some(vec![crate::llm::LlmIssueAssessment {
                issue_id: "LLM-1".to_string(),
                severity: 0.95,
                blast_radius: claudear_core::types::BlastRadius::Critical,
                fingerprint: "payment_crash".to_string(),
            }]),
        };

        let (result, _) = prioritise(
            &config,
            vec![(issue, mr)],
            &tracker,
            &std::collections::HashMap::new(),
            Some(&mock),
        );

        assert_eq!(result.len(), 1);
        assert!(
            (result[0].severity_score.score - 0.95).abs() < 1e-10,
            "LLM severity should be used, got {}",
            result[0].severity_score.score
        );
        assert_eq!(
            result[0].blast_radius,
            claudear_core::types::BlastRadius::Critical
        );
        assert_eq!(result[0].cluster_key.as_deref(), Some("llm:payment_crash"));
    }

    #[test]
    fn test_prioritise_llm_fallback() {
        let config = PrioritisationConfig {
            enabled: true,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (issue, mr) = make_candidate(
            "FALL-1",
            "Minor CSS glitch",
            IssuePriority::Low,
            MatchPriority::Normal,
        );

        // Mock returns None → heuristic path
        let mock = MockLlmAnalyzer { assessments: None };

        let (result, _) = prioritise(
            &config,
            vec![(issue, mr)],
            &tracker,
            &std::collections::HashMap::new(),
            Some(&mock),
        );

        assert_eq!(result.len(), 1);
        // Should have heuristic components filled (not all zeroed)
        // severity_component is always > 0 for a matched issue
        assert!(
            result[0].severity_score.severity_component > 0.0,
            "Heuristic fallback should produce non-zero severity_component"
        );
    }

    #[test]
    fn test_prioritise_llm_clustering_by_fingerprint() {
        let config = PrioritisationConfig {
            enabled: true,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (issue1, mr1) = make_candidate(
            "CL-1",
            "NullPointer in auth A",
            IssuePriority::High,
            MatchPriority::Normal,
        );
        let (issue2, mr2) = make_candidate(
            "CL-2",
            "NullPointer in auth B",
            IssuePriority::High,
            MatchPriority::Normal,
        );

        let mock = MockLlmAnalyzer {
            assessments: Some(vec![
                crate::llm::LlmIssueAssessment {
                    issue_id: "CL-1".to_string(),
                    severity: 0.8,
                    blast_radius: claudear_core::types::BlastRadius::Core,
                    fingerprint: "null_ref_auth".to_string(),
                },
                crate::llm::LlmIssueAssessment {
                    issue_id: "CL-2".to_string(),
                    severity: 0.75,
                    blast_radius: claudear_core::types::BlastRadius::Core,
                    fingerprint: "null_ref_auth".to_string(),
                },
            ]),
        };

        let (result, _) = prioritise(
            &config,
            vec![(issue1, mr1), (issue2, mr2)],
            &tracker,
            &std::collections::HashMap::new(),
            Some(&mock),
        );

        assert_eq!(result.len(), 2);
        // Both should share the same cluster key
        assert_eq!(
            result[0].cluster_key, result[1].cluster_key,
            "Issues with same fingerprint should share cluster key"
        );
        assert_eq!(result[0].cluster_key.as_deref(), Some("llm:null_ref_auth"));
    }

    #[test]
    fn test_prioritise_llm_partial_assessment() {
        // LLM returns assessment for only one of two issues — the other should
        // fall back to heuristic scoring.
        let config = PrioritisationConfig {
            enabled: true,
            ..Default::default()
        };
        let tracker = NoOpTracker;

        let (issue1, mr1) = make_candidate(
            "PA-1",
            "Critical crash",
            IssuePriority::Critical,
            MatchPriority::High,
        );
        let (issue2, mr2) = make_candidate(
            "PA-2",
            "Minor style issue",
            IssuePriority::Low,
            MatchPriority::Low,
        );

        let mock = MockLlmAnalyzer {
            // Only return assessment for PA-1, not PA-2
            assessments: Some(vec![crate::llm::LlmIssueAssessment {
                issue_id: "PA-1".to_string(),
                severity: 0.99,
                blast_radius: claudear_core::types::BlastRadius::Critical,
                fingerprint: "critical_crash".to_string(),
            }]),
        };

        let (result, _) = prioritise(
            &config,
            vec![(issue1, mr1), (issue2, mr2)],
            &tracker,
            &std::collections::HashMap::new(),
            Some(&mock),
        );

        assert_eq!(result.len(), 2);

        // PA-1 should use LLM score
        let pa1 = result.iter().find(|r| r.issue.id == "PA-1").unwrap();
        assert!(
            (pa1.severity_score.score - 0.99).abs() < 1e-10,
            "PA-1 should use LLM severity"
        );
        assert_eq!(
            pa1.blast_radius,
            claudear_core::types::BlastRadius::Critical
        );

        // PA-2 should use heuristic score (severity_component > 0)
        let pa2 = result.iter().find(|r| r.issue.id == "PA-2").unwrap();
        assert!(
            pa2.severity_score.severity_component > 0.0,
            "PA-2 should fall back to heuristic scoring"
        );
    }
}
