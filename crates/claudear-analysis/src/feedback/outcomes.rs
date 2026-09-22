//! Outcome tracking for fix attempts.

use crate::feedback::{cosine_similarity, EmbeddingClient};
use claudear_core::error::Result;
use claudear_core::types::{FixOutcome, Outcome};
use std::collections::HashMap;

/// Categorize an error message using semantic similarity against reference embeddings.
///
/// Embeds the error message with the provided `EmbeddingClient`, then computes
/// cosine similarity against each reference embedding. Returns the category of
/// the best match if similarity exceeds 0.3, otherwise returns `"unknown"`.
pub async fn categorize_error_semantic(
    error_message: &str,
    client: &EmbeddingClient,
    reference_embeddings: &[(String, Vec<f32>)],
) -> Result<String> {
    let error_embedding = client.embed(error_message).await?;

    let mut best_category = "unknown".to_string();
    let mut best_similarity: f32 = 0.3; // threshold

    for (category, ref_embedding) in reference_embeddings {
        let similarity = cosine_similarity(&error_embedding, ref_embedding);
        if similarity > best_similarity {
            best_similarity = similarity;
            best_category = category.clone();
        }
    }

    Ok(best_category)
}

/// Build reference embeddings for error categorization.
///
/// Defines 8 category description strings and embeds them in a single batch call,
/// returning a mapping from category name to its embedding vector.
pub async fn build_error_reference_embeddings(
    client: &EmbeddingClient,
) -> Result<Vec<(String, Vec<f32>)>> {
    let categories = [
        ("timeout", "network timeout, connection timed out, request deadline exceeded, slow response"),
        ("permission", "permission denied, access denied, authentication failed, authorization error, forbidden"),
        ("syntax", "syntax error, parse error, unexpected token, invalid syntax, malformed input"),
        ("test_failure", "test failed, test failure, assertion error, test case not passing, spec failure"),
        ("build_failure", "build failed, compilation error, link error, build process failure"),
        ("not_found", "not found, missing file, missing module, missing dependency, module not found"),
        ("conflict", "merge conflict, git conflict, conflicting changes, conflict resolution needed"),
        ("dependency", "dependency version mismatch, incompatible dependency, package version conflict"),
    ];

    let descriptions: Vec<&str> = categories.iter().map(|(_, desc)| *desc).collect();
    let embeddings = client.embed_batch(&descriptions).await?;

    let result: Vec<(String, Vec<f32>)> = categories
        .iter()
        .zip(embeddings)
        .map(|((name, _), embedding)| (name.to_string(), embedding))
        .collect();

    Ok(result)
}

/// Tracks fix outcomes in memory (can be persisted to DB later).
pub struct OutcomeTracker {
    outcomes: Vec<FixOutcome>,
    next_id: i64,
}

impl OutcomeTracker {
    /// Create a new outcome tracker.
    pub fn new() -> Self {
        Self {
            outcomes: Vec::new(),
            next_id: 1,
        }
    }

    /// Load outcomes from persistent storage (e.g. DB hydration on startup).
    pub fn load(&mut self, outcomes: Vec<FixOutcome>) {
        if let Some(max_id) = outcomes.iter().map(|o| o.id).max() {
            self.next_id = max_id + 1;
        }
        self.outcomes = outcomes;
    }

    /// Record a new outcome.
    pub fn record(&mut self, mut outcome: FixOutcome) -> Result<i64> {
        outcome.id = self.next_id;
        self.next_id += 1;

        let id = outcome.id;
        self.outcomes.push(outcome);
        Ok(id)
    }

    /// Set embedding for an outcome by ID.
    pub fn set_embedding(&mut self, id: i64, embedding: Vec<f32>) -> Result<()> {
        if let Some(outcome) = self.outcomes.iter_mut().find(|o| o.id == id) {
            outcome.set_embedding(embedding);
        }
        Ok(())
    }

    /// Get outcomes by result.
    pub fn get_by_outcome(&self, outcome: Outcome) -> Vec<&FixOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.outcome == outcome)
            .collect()
    }

    /// Get success rate for a source.
    pub fn success_rate(&self, source: Option<&str>) -> f64 {
        let filtered: Vec<_> = match source {
            Some(s) => self.outcomes.iter().filter(|o| o.source == s).collect(),
            None => self.outcomes.iter().collect(),
        };

        if filtered.is_empty() {
            return 0.0;
        }

        let successes = filtered.iter().filter(|o| o.outcome.is_success()).count();
        successes as f64 / filtered.len() as f64
    }

    /// Get common error types.
    pub fn common_errors(&self, limit: usize) -> Vec<(String, usize)> {
        let mut error_counts: HashMap<String, usize> = HashMap::new();

        for outcome in &self.outcomes {
            if let Some(ref error_type) = outcome.error_type {
                *error_counts.entry(error_type.clone()).or_insert(0) += 1;
            }
        }

        let mut counts: Vec<_> = error_counts.into_iter().collect();
        counts.sort_by_key(|b| std::cmp::Reverse(b.1));
        counts.truncate(limit);
        counts
    }

    /// Get all outcomes.
    pub fn all(&self) -> &[FixOutcome] {
        &self.outcomes
    }

    /// Add learnings to an outcome.
    pub fn add_learnings(&mut self, id: i64, learnings: &str) -> Result<()> {
        if let Some(outcome) = self.outcomes.iter_mut().find(|o| o.id == id) {
            outcome.learnings = Some(learnings.to_string());
        }
        Ok(())
    }
}

impl Default for OutcomeTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use claudear_core::types::{is_common_word, FixAttempt, Issue, IssuePriority, IssueStatus};

    fn create_test_issue(title: &str, description: &str) -> Issue {
        Issue {
            id: "test-1".to_string(),
            short_id: "TEST-1".to_string(),
            title: title.to_string(),
            description: Some(description.to_string()),
            url: "https://example.com".to_string(),
            source: "test".to_string(),
            priority: IssuePriority::Medium,
            status: IssueStatus::Open,
            metadata: Default::default(),
            created_at: None,
            updated_at: None,
        }
    }

    fn create_test_attempt() -> FixAttempt {
        FixAttempt {
            id: 1,
            source: "test".to_string(),
            issue_id: "test-1".to_string(),
            short_id: "TEST-1".to_string(),
            status: claudear_core::types::FixAttemptStatus::Success,
            pr_url: Some("https://github.com/test/pr/1".to_string()),
            scm_repo: None,
            scm_pr_number: None,
            error_message: None,
            attempted_at: Utc::now(),
            resolved_at: None,
            merged_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        }
    }

    #[test]
    fn test_outcome_parse() {
        assert_eq!(Outcome::parse("merged"), Some(Outcome::Merged));
        assert_eq!(Outcome::parse("closed"), Some(Outcome::Closed));
        assert_eq!(Outcome::parse("failed"), Some(Outcome::Failed));
        assert_eq!(Outcome::parse("cannot_fix"), Some(Outcome::CannotFix));
        assert_eq!(Outcome::parse("invalid"), None);
    }

    #[test]
    fn test_outcome_is_success() {
        assert!(Outcome::Merged.is_success());
        assert!(!Outcome::Closed.is_success());
        assert!(!Outcome::Failed.is_success());
    }

    #[test]
    fn test_extract_keywords() {
        let keywords = FixOutcome::extract_keywords(
            "Database connection timeout",
            "The database connection times out when processing large queries",
        );

        assert!(keywords.contains(&"database".to_string()));
        assert!(keywords.contains(&"connection".to_string()));
        assert!(keywords.contains(&"timeout".to_string()));
        assert!(keywords.contains(&"queries".to_string()));
    }

    #[test]
    fn test_categorize_error() {
        assert_eq!(
            FixOutcome::categorize_error("Connection timed out"),
            "timeout"
        );
        assert_eq!(
            FixOutcome::categorize_error("Permission denied"),
            "permission"
        );
        assert_eq!(
            FixOutcome::categorize_error("Syntax error on line 5"),
            "syntax"
        );
        assert_eq!(FixOutcome::categorize_error("Tests failed"), "test_failure");
        assert_eq!(
            FixOutcome::categorize_error("Build failed"),
            "build_failure"
        );
        assert_eq!(FixOutcome::categorize_error("File not found"), "not_found");
    }

    #[test]
    fn test_similarity_with_embeddings() {
        let issue1 = create_test_issue(
            "Database timeout error",
            "Connection to PostgreSQL times out",
        );
        let issue2 = create_test_issue("Database connection issue", "PostgreSQL connection fails");
        let attempt = create_test_attempt();

        let mut outcome1 =
            FixOutcome::from_attempt(&attempt, &issue1, "test prompt", Outcome::Merged);
        let mut outcome2 =
            FixOutcome::from_attempt(&attempt, &issue2, "test prompt", Outcome::Closed);

        outcome1.set_embedding(vec![0.9, 0.1, 0.0]);
        outcome2.set_embedding(vec![0.8, 0.2, 0.0]);

        let similarity = outcome1.similarity(&outcome2);
        assert!(similarity > 0.0);
        assert!(similarity <= 1.0);
    }

    #[test]
    fn test_similarity_without_embeddings_returns_zero() {
        let issue1 = create_test_issue("Database timeout error", "Connection times out");
        let attempt = create_test_attempt();

        let outcome1 = FixOutcome::from_attempt(&attempt, &issue1, "test prompt", Outcome::Merged);
        let outcome2 = FixOutcome::from_attempt(&attempt, &issue1, "test prompt", Outcome::Closed);

        assert_eq!(outcome1.similarity(&outcome2), 0.0);
    }

    #[test]
    fn test_outcome_tracker() {
        let mut tracker = OutcomeTracker::new();
        let issue = create_test_issue("Test issue", "Description here");
        let attempt = create_test_attempt();

        let outcome = FixOutcome::from_attempt(&attempt, &issue, "prompt", Outcome::Merged);
        let id = tracker.record(outcome).unwrap();

        assert_eq!(id, 1);
        assert_eq!(tracker.all().len(), 1);
    }

    #[test]
    fn test_success_rate() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Closed,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();

        let rate = tracker.success_rate(None);
        assert!((rate - 0.5).abs() < 0.01); // 50% success rate
    }

    #[test]
    fn test_outcome_as_str() {
        assert_eq!(Outcome::Merged.as_str(), "merged");
        assert_eq!(Outcome::Closed.as_str(), "closed");
        assert_eq!(Outcome::Failed.as_str(), "failed");
        assert_eq!(Outcome::CannotFix.as_str(), "cannot_fix");
    }

    #[test]
    fn test_outcome_parse_case_insensitive() {
        assert_eq!(Outcome::parse("MERGED"), Some(Outcome::Merged));
        assert_eq!(Outcome::parse("Closed"), Some(Outcome::Closed));
        assert_eq!(Outcome::parse("FAILED"), Some(Outcome::Failed));
        assert_eq!(Outcome::parse("CannotFix"), Some(Outcome::CannotFix));
    }

    #[test]
    fn test_categorize_error_all_types() {
        assert_eq!(FixOutcome::categorize_error("Request timed out"), "timeout");
        assert_eq!(
            FixOutcome::categorize_error("Access denied error"),
            "permission"
        );
        assert_eq!(
            FixOutcome::categorize_error("Parse error in JSON"),
            "syntax"
        );
        assert_eq!(
            FixOutcome::categorize_error("3 tests failed"),
            "test_failure"
        );
        assert_eq!(
            FixOutcome::categorize_error("Build failed with errors"),
            "build_failure"
        );
        assert_eq!(
            FixOutcome::categorize_error("Module not found"),
            "not_found"
        );
        assert_eq!(
            FixOutcome::categorize_error("Missing dependency"),
            "not_found"
        );
        assert_eq!(FixOutcome::categorize_error("Merge conflict"), "conflict");
        assert_eq!(FixOutcome::categorize_error("Some random error"), "unknown");
    }

    #[test]
    fn test_is_common_word() {
        assert!(is_common_word("the"));
        assert!(is_common_word("is"));
        assert!(is_common_word("error"));
        assert!(is_common_word("fix"));
        assert!(!is_common_word("database"));
        assert!(!is_common_word("postgresql"));
        assert!(!is_common_word("timeout"));
    }

    #[test]
    fn test_extract_keywords_filters_short_words() {
        let keywords = FixOutcome::extract_keywords("A bug in the API", "It is bad");
        // Short words like "a", "in", "is", "it" should be filtered
        for kw in &keywords {
            assert!(kw.len() > 3);
        }
    }

    #[test]
    fn test_extract_keywords_max_count() {
        let long_text = "word1 word2 word3 word4 word5 word6 word7 word8 word9 word10 word11 word12 word13 word14 word15 word16 word17 word18 word19 word20 word21 word22 word23 word24 word25";
        let keywords = FixOutcome::extract_keywords(long_text, "");
        assert!(keywords.len() <= 20);
    }

    #[test]
    fn test_similarity_identical_embeddings() {
        let issue = create_test_issue("Same title", "Same description");
        let attempt = create_test_attempt();
        let mut outcome1 = FixOutcome::from_attempt(&attempt, &issue, "prompt", Outcome::Merged);
        let mut outcome2 = FixOutcome::from_attempt(&attempt, &issue, "prompt", Outcome::Merged);

        outcome1.set_embedding(vec![1.0, 0.0, 0.0]);
        outcome2.set_embedding(vec![1.0, 0.0, 0.0]);

        let similarity = outcome1.similarity(&outcome2);
        assert!((similarity - 1.0).abs() < 0.0001); // Identical embeddings should be ~1.0
    }

    #[test]
    fn test_similarity_completely_different() {
        let attempt = create_test_attempt();
        let issue1 = create_test_issue("PostgreSQL database error", "Connection to database fails");
        let issue2 = create_test_issue("JavaScript CSS styling", "Frontend React component");

        let outcome1 = FixOutcome::from_attempt(&attempt, &issue1, "prompt", Outcome::Merged);
        let outcome2 = FixOutcome::from_attempt(&attempt, &issue2, "prompt", Outcome::Merged);

        let similarity = outcome1.similarity(&outcome2);
        assert!(similarity < 0.3); // Very different topics
    }

    #[test]
    fn test_outcome_tracker_default() {
        let tracker = OutcomeTracker::default();
        assert!(tracker.all().is_empty());
    }

    #[test]
    fn test_outcome_tracker_increments_id() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        let id1 = tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        let id2 = tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        let id3 = tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_get_by_outcome() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Closed,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();

        let merged = tracker.get_by_outcome(Outcome::Merged);
        assert_eq!(merged.len(), 2);

        let closed = tracker.get_by_outcome(Outcome::Closed);
        assert_eq!(closed.len(), 1);

        let failed = tracker.get_by_outcome(Outcome::Failed);
        assert!(failed.is_empty());
    }

    #[test]
    fn test_success_rate_by_source() {
        let mut tracker = OutcomeTracker::new();
        let issue = create_test_issue("Test", "Test");

        // Linear: 1 success, 1 fail = 50%
        let mut linear_attempt = create_test_attempt();
        linear_attempt.source = "linear".to_string();
        let mut linear_outcome =
            FixOutcome::from_attempt(&linear_attempt, &issue, "p", Outcome::Merged);
        linear_outcome.source = "linear".to_string();
        tracker.record(linear_outcome).unwrap();

        let mut linear_outcome2 =
            FixOutcome::from_attempt(&linear_attempt, &issue, "p", Outcome::Failed);
        linear_outcome2.source = "linear".to_string();
        tracker.record(linear_outcome2).unwrap();

        // Sentry: 2 successes = 100%
        let mut sentry_attempt = create_test_attempt();
        sentry_attempt.source = "sentry".to_string();
        let mut sentry_outcome =
            FixOutcome::from_attempt(&sentry_attempt, &issue, "p", Outcome::Merged);
        sentry_outcome.source = "sentry".to_string();
        tracker.record(sentry_outcome.clone()).unwrap();
        tracker.record(sentry_outcome).unwrap();

        let linear_rate = tracker.success_rate(Some("linear"));
        assert!((linear_rate - 0.5).abs() < 0.01);

        let sentry_rate = tracker.success_rate(Some("sentry"));
        assert!((sentry_rate - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_success_rate_empty() {
        let tracker = OutcomeTracker::new();
        assert_eq!(tracker.success_rate(None), 0.0);
        assert_eq!(tracker.success_rate(Some("nonexistent")), 0.0);
    }

    #[test]
    fn test_common_errors() {
        let mut tracker = OutcomeTracker::new();
        let issue = create_test_issue("Test", "Test");
        let mut attempt = create_test_attempt();

        attempt.error_message = Some("Connection timed out".to_string());
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();

        attempt.error_message = Some("Permission denied".to_string());
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();

        attempt.error_message = Some("Build failed".to_string());
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();

        let errors = tracker.common_errors(10);
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0].0, "timeout");
        assert_eq!(errors[0].1, 3);
        assert_eq!(errors[1].0, "permission");
        assert_eq!(errors[1].1, 2);
    }

    #[test]
    fn test_add_learnings() {
        let mut tracker = OutcomeTracker::new();
        let issue = create_test_issue("Test", "Test");
        let attempt = create_test_attempt();

        let id = tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();

        tracker
            .add_learnings(id, "Important learning here")
            .unwrap();

        let outcome = &tracker.all()[0];
        assert_eq!(
            outcome.learnings,
            Some("Important learning here".to_string())
        );
    }

    #[test]
    fn test_add_learnings_nonexistent_id() {
        let mut tracker = OutcomeTracker::new();

        // Should not panic
        let result = tracker.add_learnings(999, "Learning");
        assert!(result.is_ok());
    }

    #[test]
    fn test_fix_outcome_from_attempt_sets_fields() {
        let issue = create_test_issue("Test Title", "Test Description");
        let mut attempt = create_test_attempt();
        attempt.error_message = Some("Connection timed out".to_string());

        let outcome = FixOutcome::from_attempt(&attempt, &issue, "test prompt", Outcome::Failed);

        assert_eq!(outcome.source, "test");
        assert_eq!(outcome.issue_id, "test-1");
        assert_eq!(outcome.prompt_used, "test prompt");
        assert_eq!(outcome.outcome, Outcome::Failed);
        assert_eq!(outcome.error_type, Some("timeout".to_string()));
        assert!(outcome.issue_text.contains("Test Title"));
        assert!(outcome.issue_text.contains("Test Description"));
        assert!(!outcome.keywords.is_empty());
    }

    #[test]
    fn test_outcome_serde() {
        let outcome = Outcome::Merged;
        let json = serde_json::to_string(&outcome).unwrap();
        assert_eq!(json, "\"merged\"");

        let parsed: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Outcome::Merged);
    }

    #[test]
    fn test_outcome_serde_cannot_fix() {
        let outcome = Outcome::CannotFix;
        let json = serde_json::to_string(&outcome).unwrap();
        assert_eq!(json, "\"cannot_fix\"");

        let parsed: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Outcome::CannotFix);
    }

    #[test]
    fn test_embedding_similarity() {
        let issue = create_test_issue("Test", "Test");
        let attempt = create_test_attempt();

        let mut outcome1 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        let mut outcome2 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);

        // Set similar embeddings
        outcome1.set_embedding(vec![1.0, 0.0, 0.0]);
        outcome2.set_embedding(vec![0.9, 0.1, 0.0]);

        // Should use cosine similarity when embeddings are available
        let similarity = outcome1.similarity(&outcome2);
        assert!(similarity > 0.9); // High similarity
        assert!(similarity <= 1.0);
    }

    #[test]
    fn test_embedding_similarity_orthogonal() {
        let issue = create_test_issue("Test", "Test");
        let attempt = create_test_attempt();

        let mut outcome1 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        let mut outcome2 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);

        // Set orthogonal embeddings
        outcome1.set_embedding(vec![1.0, 0.0, 0.0]);
        outcome2.set_embedding(vec![0.0, 1.0, 0.0]);

        let similarity = outcome1.similarity(&outcome2);
        assert!(similarity < 0.1); // Very low similarity (orthogonal vectors)
    }

    #[test]
    fn test_similarity_returns_zero_without_both_embeddings() {
        let issue = create_test_issue("Database error", "PostgreSQL connection fails");
        let attempt = create_test_attempt();

        let mut outcome1 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        let outcome2 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);

        // Only outcome1 has embedding — should return 0.0 without fallback
        outcome1.set_embedding(vec![1.0, 0.0, 0.0]);

        let similarity = outcome1.similarity(&outcome2);
        assert_eq!(similarity, 0.0);
    }

    #[test]
    fn test_set_embedding_via_tracker() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        let id = tracker.record(outcome).unwrap();

        // Set embedding via tracker
        tracker.set_embedding(id, vec![1.0, 2.0, 3.0]).unwrap();

        // Verify embedding was set
        let stored = &tracker.all()[0];
        assert!(stored.embedding.is_some());
        assert_eq!(stored.embedding.as_ref().unwrap(), &vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_fix_outcome_serde_with_embedding() {
        let issue = create_test_issue("Test", "Test");
        let attempt = create_test_attempt();

        let mut outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        outcome.set_embedding(vec![1.0, 2.0, 3.0]);

        // Serialize and deserialize
        let json = serde_json::to_string(&outcome).unwrap();
        let parsed: FixOutcome = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.embedding, Some(vec![1.0, 2.0, 3.0]));
    }

    #[test]
    fn test_outcome_tracker_load() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        // Create outcomes with pre-set IDs (as if loaded from DB)
        let mut o1 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        o1.id = 10;
        let mut o2 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Failed);
        o2.id = 20;

        tracker.load(vec![o1, o2]);

        assert_eq!(tracker.all().len(), 2);

        // next_id should continue from max loaded id
        let new_id = tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Closed,
            ))
            .unwrap();
        assert_eq!(new_id, 21);
    }

    #[test]
    fn test_fix_outcome_serde_without_embedding() {
        let issue = create_test_issue("Test", "Test");
        let attempt = create_test_attempt();

        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        // No embedding set

        // Serialize - embedding should be skipped (skip_serializing_if = "Option::is_none")
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(!json.contains("embedding"));

        // Deserialize
        let parsed: FixOutcome = serde_json::from_str(&json).unwrap();
        assert!(parsed.embedding.is_none());
    }

    #[test]
    fn test_outcome_parse_empty_string() {
        assert_eq!(Outcome::parse(""), None);
    }

    #[test]
    fn test_outcome_parse_whitespace() {
        // Leading/trailing whitespace should not match
        assert_eq!(Outcome::parse(" merged "), None);
        assert_eq!(Outcome::parse("merged "), None);
    }

    #[test]
    fn test_outcome_parse_cannotfix_variant() {
        // Both underscore and no-underscore should work
        assert_eq!(Outcome::parse("cannot_fix"), Some(Outcome::CannotFix));
        assert_eq!(Outcome::parse("cannotfix"), Some(Outcome::CannotFix));
        assert_eq!(Outcome::parse("CANNOTFIX"), Some(Outcome::CannotFix));
        assert_eq!(Outcome::parse("CANNOT_FIX"), Some(Outcome::CannotFix));
    }

    #[test]
    fn test_outcome_is_success_only_merged() {
        // Only Merged is considered success
        assert!(Outcome::Merged.is_success());
        assert!(!Outcome::CannotFix.is_success());
    }

    #[test]
    fn test_outcome_roundtrip_all_variants() {
        for outcome in [
            Outcome::Merged,
            Outcome::Closed,
            Outcome::Failed,
            Outcome::CannotFix,
        ] {
            let s = outcome.as_str();
            let parsed = Outcome::parse(s).unwrap();
            assert_eq!(parsed, outcome, "roundtrip failed for {:?}", outcome);
        }
    }

    #[test]
    fn test_extract_keywords_empty_inputs() {
        let keywords = FixOutcome::extract_keywords("", "");
        assert!(keywords.is_empty());
    }

    #[test]
    fn test_extract_keywords_only_common_words() {
        // All words are either short (<= 3 chars) or common words
        let keywords = FixOutcome::extract_keywords("the bug is a fix", "error in the issue");
        // "bug" and "is" are 3 chars or less, "the" is common, "fix" is common, "error" is common, "issue" is common
        // Only words > 3 chars that are not common should appear
        for kw in &keywords {
            assert!(kw.len() > 3, "short word leaked: {}", kw);
            assert!(!is_common_word(kw), "common word leaked: {}", kw);
        }
    }

    #[test]
    fn test_extract_keywords_special_characters() {
        let keywords =
            FixOutcome::extract_keywords("null_pointer_exception", "at com.example.MyClass:42");
        // Should split on non-alphanumeric (except underscore)
        assert!(
            keywords.contains(&"null_pointer_exception".to_string())
                || keywords.contains(&"null_pointer".to_string())
                || keywords.iter().any(|k| k.contains("null"))
        );
    }

    #[test]
    fn test_extract_keywords_unicode() {
        let keywords = FixOutcome::extract_keywords("Ошибка базы данных", "подключение не удалось");
        // Should handle Unicode gracefully - either extract unicode words or produce empty
        // The important thing is it doesn't panic
        for kw in &keywords {
            assert!(kw.len() > 3);
        }
    }

    #[test]
    fn test_categorize_error_case_insensitive() {
        assert_eq!(FixOutcome::categorize_error("TIMEOUT"), "timeout");
        assert_eq!(
            FixOutcome::categorize_error("Permission DENIED"),
            "permission"
        );
        assert_eq!(FixOutcome::categorize_error("SYNTAX error"), "syntax");
    }

    #[test]
    fn test_categorize_error_empty_string() {
        assert_eq!(FixOutcome::categorize_error(""), "unknown");
    }

    #[test]
    fn test_categorize_error_multiple_matching_keywords() {
        // "timeout" checked first, so it should win over other matches
        assert_eq!(
            FixOutcome::categorize_error("Build failed due to timeout"),
            "timeout"
        );
        // "permission" checked before "syntax"
        assert_eq!(
            FixOutcome::categorize_error("Permission denied: parse error"),
            "permission"
        );
    }

    #[test]
    fn test_categorize_error_priority_order() {
        // Verify the priority ordering: timeout > permission > syntax > test_failure > build_failure > not_found > conflict > unknown
        // "test fail" should match test_failure not other patterns
        assert_eq!(FixOutcome::categorize_error("test failed"), "test_failure");
        // But "test" alone isn't enough — needs both "test" and "fail"
        assert_eq!(FixOutcome::categorize_error("test passed"), "unknown");
    }

    #[test]
    fn test_similarity_self_is_one() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Database timeout", "PostgreSQL connection timed out");
        let mut outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        outcome.set_embedding(vec![1.0, 0.0, 0.0]);

        let sim = outcome.similarity(&outcome);
        assert!((sim - 1.0).abs() < 0.0001, "self-similarity should be ~1.0");
    }

    #[test]
    fn test_similarity_symmetric() {
        let attempt = create_test_attempt();
        let issue1 = create_test_issue("Database timeout error", "PostgreSQL connection");
        let issue2 = create_test_issue("API server crash", "PostgreSQL pool exhausted");
        let o1 = FixOutcome::from_attempt(&attempt, &issue1, "p", Outcome::Merged);
        let o2 = FixOutcome::from_attempt(&attempt, &issue2, "p", Outcome::Merged);
        let sim_12 = o1.similarity(&o2);
        let sim_21 = o2.similarity(&o1);
        assert!(
            (sim_12 - sim_21).abs() < 1e-10,
            "similarity should be symmetric"
        );
    }

    #[test]
    fn test_similarity_both_empty_keywords() {
        let attempt = create_test_attempt();
        // Very short words only -> no keywords extracted
        let issue1 = Issue {
            id: "1".to_string(),
            short_id: "1".to_string(),
            title: "a b c".to_string(),
            description: None,
            url: "u".to_string(),
            source: "t".to_string(),
            priority: IssuePriority::Medium,
            status: IssueStatus::Open,
            metadata: Default::default(),
            created_at: None,
            updated_at: None,
        };
        let issue2 = Issue {
            id: "2".to_string(),
            short_id: "2".to_string(),
            title: "x y z".to_string(),
            description: None,
            url: "u".to_string(),
            source: "t".to_string(),
            priority: IssuePriority::Medium,
            status: IssueStatus::Open,
            metadata: Default::default(),
            created_at: None,
            updated_at: None,
        };
        let o1 = FixOutcome::from_attempt(&attempt, &issue1, "p", Outcome::Merged);
        let o2 = FixOutcome::from_attempt(&attempt, &issue2, "p", Outcome::Merged);
        assert_eq!(o1.similarity(&o2), 0.0, "both empty keywords should be 0.0");
    }

    #[test]
    fn test_embedding_similarity_identical_vectors() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        let mut o1 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        let mut o2 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        o1.set_embedding(vec![1.0, 2.0, 3.0]);
        o2.set_embedding(vec![1.0, 2.0, 3.0]);
        let sim = o1.similarity(&o2);
        assert!(
            (sim - 1.0).abs() < 0.01,
            "identical embeddings should have similarity ~1.0, got {}",
            sim
        );
    }

    #[test]
    fn test_embedding_similarity_opposite_vectors() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        let mut o1 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        let mut o2 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        o1.set_embedding(vec![1.0, 0.0, 0.0]);
        o2.set_embedding(vec![-1.0, 0.0, 0.0]);
        let sim = o1.similarity(&o2);
        assert!(
            sim < 0.0,
            "opposite vectors should have negative cosine similarity, got {}",
            sim
        );
    }

    #[test]
    fn test_common_errors_limit_zero() {
        let mut tracker = OutcomeTracker::new();
        let issue = create_test_issue("Test", "Test");
        let mut attempt = create_test_attempt();
        attempt.error_message = Some("Connection timed out".to_string());
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();

        let errors = tracker.common_errors(0);
        assert!(errors.is_empty(), "limit=0 should return empty");
    }

    #[test]
    fn test_common_errors_no_errors() {
        let mut tracker = OutcomeTracker::new();
        let issue = create_test_issue("Test", "Test");
        let attempt = create_test_attempt();
        // Merged outcomes don't have error_type set (error_message is None)
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();

        let errors = tracker.common_errors(10);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_set_embedding_nonexistent_id() {
        let mut tracker = OutcomeTracker::new();
        // Should not panic
        let result = tracker.set_embedding(999, vec![1.0, 2.0]);
        assert!(result.is_ok());
        // Verify nothing was changed
        assert!(tracker.all().is_empty());
    }

    #[test]
    fn test_load_replaces_existing_outcomes() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        assert_eq!(tracker.all().len(), 1);

        // Load replaces everything
        tracker.load(vec![]);
        assert!(tracker.all().is_empty());
    }

    #[test]
    fn test_load_empty_keeps_next_id_at_one() {
        let mut tracker = OutcomeTracker::new();
        tracker.load(vec![]);
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        let id = tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        assert_eq!(id, 1, "after loading empty, next_id should still be 1");
    }

    #[test]
    fn test_success_rate_all_failed() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        for _ in 0..5 {
            tracker
                .record(FixOutcome::from_attempt(
                    &attempt,
                    &issue,
                    "p",
                    Outcome::Failed,
                ))
                .unwrap();
        }
        assert_eq!(tracker.success_rate(None), 0.0);
    }

    #[test]
    fn test_success_rate_all_merged() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        for _ in 0..3 {
            tracker
                .record(FixOutcome::from_attempt(
                    &attempt,
                    &issue,
                    "p",
                    Outcome::Merged,
                ))
                .unwrap();
        }
        assert!((tracker.success_rate(None) - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_success_rate_nonexistent_source() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        assert_eq!(tracker.success_rate(Some("nonexistent")), 0.0);
    }

    #[test]
    fn test_from_attempt_no_description() {
        let attempt = create_test_attempt();
        let issue = Issue {
            id: "1".to_string(),
            short_id: "1".to_string(),
            title: "Title only".to_string(),
            description: None,
            url: "u".to_string(),
            source: "t".to_string(),
            priority: IssuePriority::Medium,
            status: IssueStatus::Open,
            metadata: Default::default(),
            created_at: None,
            updated_at: None,
        };
        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        assert!(outcome.issue_text.contains("Title only"));
        assert!(outcome.error_type.is_none());
    }

    #[test]
    fn test_from_attempt_no_error_message() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Failed);
        // No error_message on attempt means no error_type on outcome
        assert!(outcome.error_type.is_none());
    }

    #[test]
    fn test_keyword_similarity_disjoint_sets() {
        let attempt = create_test_attempt();
        let issue1 = create_test_issue("PostgreSQL database timeout", "connection pool exhausted");
        let issue2 = create_test_issue("JavaScript rendering crash", "React component lifecycle");
        let o1 = FixOutcome::from_attempt(&attempt, &issue1, "p", Outcome::Merged);
        let o2 = FixOutcome::from_attempt(&attempt, &issue2, "p", Outcome::Merged);
        let sim = o1.similarity(&o2);
        assert!(
            sim < 0.1,
            "disjoint keyword sets should have near-zero similarity, got {}",
            sim
        );
    }

    #[test]
    fn test_is_common_word_not_in_list() {
        assert!(!is_common_word("postgresql"));
        assert!(!is_common_word("database"));
        assert!(!is_common_word("timeout"));
        assert!(!is_common_word("kubernetes"));
    }

    #[test]
    fn test_is_common_word_case_sensitive() {
        // is_common_word is case-sensitive - uppercase versions should NOT match
        assert!(!is_common_word("The"));
        assert!(!is_common_word("IS"));
        assert!(!is_common_word("Error"));
    }

    #[test]
    fn test_categorize_error_semantic_best_match() {
        // We test categorize_error_semantic with mock data by calling the
        // inner logic directly: embed the error message as a known vector,
        // provide reference embeddings, and verify the best match is returned.
        //
        // Since categorize_error_semantic requires an EmbeddingClient (which
        // needs model download), we replicate its core matching logic here
        // with handcrafted vectors.

        // Mock reference embeddings: 3-dimensional for simplicity
        let reference_embeddings: Vec<(String, Vec<f32>)> = vec![
            ("timeout".to_string(), vec![1.0, 0.0, 0.0]),
            ("permission".to_string(), vec![0.0, 1.0, 0.0]),
            ("syntax".to_string(), vec![0.0, 0.0, 1.0]),
            ("test_failure".to_string(), vec![0.7, 0.7, 0.0]),
            ("build_failure".to_string(), vec![0.5, 0.0, 0.5]),
        ];

        // Simulate an error embedding very close to "timeout"
        let error_embedding = vec![0.95, 0.05, 0.0];

        let mut best_category = "unknown".to_string();
        let mut best_similarity: f32 = 0.3;

        for (category, ref_embedding) in &reference_embeddings {
            let similarity = cosine_similarity(&error_embedding, ref_embedding);
            if similarity > best_similarity {
                best_similarity = similarity;
                best_category = category.clone();
            }
        }

        assert_eq!(best_category, "timeout");
        assert!(best_similarity > 0.9);
    }

    #[test]
    fn test_categorize_error_semantic_returns_unknown_below_threshold() {
        // When no reference embedding is similar enough, "unknown" should be returned.
        let reference_embeddings: Vec<(String, Vec<f32>)> = vec![
            ("timeout".to_string(), vec![1.0, 0.0, 0.0]),
            ("permission".to_string(), vec![0.0, 1.0, 0.0]),
        ];

        // An error embedding orthogonal to all references
        let error_embedding = vec![0.0, 0.0, 1.0];

        let mut best_category = "unknown".to_string();
        let mut best_similarity: f32 = 0.3;

        for (category, ref_embedding) in &reference_embeddings {
            let similarity = cosine_similarity(&error_embedding, ref_embedding);
            if similarity > best_similarity {
                best_similarity = similarity;
                best_category = category.clone();
            }
        }

        assert_eq!(best_category, "unknown");
    }

    #[test]
    fn test_categorize_error_semantic_picks_highest_similarity() {
        // When multiple references have similarity > 0.3, the highest wins.
        let reference_embeddings: Vec<(String, Vec<f32>)> = vec![
            ("timeout".to_string(), vec![0.8, 0.2, 0.0]),
            ("permission".to_string(), vec![0.7, 0.3, 0.0]),
            ("build_failure".to_string(), vec![0.9, 0.1, 0.0]),
        ];

        let error_embedding = vec![1.0, 0.0, 0.0];

        let mut best_category = "unknown".to_string();
        let mut best_similarity: f32 = 0.3;

        for (category, ref_embedding) in &reference_embeddings {
            let similarity = cosine_similarity(&error_embedding, ref_embedding);
            if similarity > best_similarity {
                best_similarity = similarity;
                best_category = category.clone();
            }
        }

        // build_failure [0.9, 0.1, 0.0] is closest to [1.0, 0.0, 0.0]
        assert_eq!(best_category, "build_failure");
    }

    #[test]
    fn test_categorize_error_semantic_empty_references() {
        // With no reference embeddings, result should be "unknown".
        let reference_embeddings: Vec<(String, Vec<f32>)> = vec![];
        let error_embedding = vec![1.0, 0.0, 0.0];

        let mut best_category = "unknown".to_string();
        let mut best_similarity: f32 = 0.3;

        for (category, ref_embedding) in &reference_embeddings {
            let similarity = cosine_similarity(&error_embedding, ref_embedding);
            if similarity > best_similarity {
                best_similarity = similarity;
                best_category = category.clone();
            }
        }

        assert_eq!(best_category, "unknown");
    }

    #[test]
    fn test_categorize_error_semantic_exact_match() {
        // When error embedding exactly matches a reference, similarity should be ~1.0.
        let reference_embeddings: Vec<(String, Vec<f32>)> = vec![
            ("timeout".to_string(), vec![1.0, 0.0, 0.0]),
            ("syntax".to_string(), vec![0.0, 1.0, 0.0]),
        ];

        let error_embedding = vec![0.0, 1.0, 0.0]; // exactly matches syntax

        let mut best_category = "unknown".to_string();
        let mut best_similarity: f32 = 0.3;

        for (category, ref_embedding) in &reference_embeddings {
            let similarity = cosine_similarity(&error_embedding, ref_embedding);
            if similarity > best_similarity {
                best_similarity = similarity;
                best_category = category.clone();
            }
        }

        assert_eq!(best_category, "syntax");
        assert!((best_similarity - 1.0).abs() < 0.0001);
    }

    #[test]
    fn test_categorize_error_semantic_all_eight_categories() {
        // Verify we can distinguish all 8 categories with distinct reference vectors.
        let reference_embeddings: Vec<(String, Vec<f32>)> = vec![
            (
                "timeout".to_string(),
                vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ),
            (
                "permission".to_string(),
                vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ),
            (
                "syntax".to_string(),
                vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ),
            (
                "test_failure".to_string(),
                vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
            ),
            (
                "build_failure".to_string(),
                vec![0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
            ),
            (
                "not_found".to_string(),
                vec![0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            ),
            (
                "conflict".to_string(),
                vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            ),
            (
                "dependency".to_string(),
                vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            ),
        ];

        // Test each category by providing a vector aligned with it
        let expected = [
            "timeout",
            "permission",
            "syntax",
            "test_failure",
            "build_failure",
            "not_found",
            "conflict",
            "dependency",
        ];

        for (i, expected_cat) in expected.iter().enumerate() {
            let mut error_embedding = vec![0.0f32; 8];
            error_embedding[i] = 1.0;

            let mut best_category = "unknown".to_string();
            let mut best_similarity: f32 = 0.3;

            for (category, ref_embedding) in &reference_embeddings {
                let similarity = cosine_similarity(&error_embedding, ref_embedding);
                if similarity > best_similarity {
                    best_similarity = similarity;
                    best_category = category.clone();
                }
            }

            assert_eq!(
                best_category, *expected_cat,
                "expected {} for dimension {}, got {}",
                expected_cat, i, best_category
            );
        }
    }

    #[test]
    fn test_categorize_error_semantic_threshold_boundary() {
        // Test behavior right at the 0.3 threshold boundary.
        let reference_embeddings: Vec<(String, Vec<f32>)> =
            vec![("timeout".to_string(), vec![1.0, 0.0, 0.0])];

        // A vector that produces similarity just barely above 0.3 with [1, 0, 0]
        // cos(theta) = 0.3 means theta ~ 72.5 degrees
        // Use [0.3, 0.954, 0.0] which has cosine similarity ~0.3 with [1, 0, 0]
        let error_embedding = vec![0.3, 0.954, 0.0];
        let sim = cosine_similarity(&error_embedding, &reference_embeddings[0].1);

        let mut best_category = "unknown".to_string();
        let mut best_similarity: f32 = 0.3;

        for (category, ref_embedding) in &reference_embeddings {
            let similarity = cosine_similarity(&error_embedding, ref_embedding);
            if similarity > best_similarity {
                best_similarity = similarity;
                best_category = category.clone();
            }
        }

        // The similarity is approximately 0.3 - if it's exactly 0.3 it should NOT match
        // (we use > not >=), if slightly above it should match.
        if sim > 0.3 {
            assert_eq!(best_category, "timeout");
        } else {
            assert_eq!(best_category, "unknown");
        }
    }

    #[test]
    fn test_get_by_outcome_cannot_fix() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::CannotFix,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::CannotFix,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();

        let cannot_fix = tracker.get_by_outcome(Outcome::CannotFix);
        assert_eq!(cannot_fix.len(), 2);

        let merged = tracker.get_by_outcome(Outcome::Merged);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn test_get_by_outcome_all_variants_empty() {
        let tracker = OutcomeTracker::new();
        assert!(tracker.get_by_outcome(Outcome::Merged).is_empty());
        assert!(tracker.get_by_outcome(Outcome::Closed).is_empty());
        assert!(tracker.get_by_outcome(Outcome::Failed).is_empty());
        assert!(tracker.get_by_outcome(Outcome::CannotFix).is_empty());
    }

    #[test]
    fn test_outcome_serde_closed() {
        let outcome = Outcome::Closed;
        let json = serde_json::to_string(&outcome).unwrap();
        assert_eq!(json, "\"closed\"");
        let parsed: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Outcome::Closed);
    }

    #[test]
    fn test_outcome_serde_failed() {
        let outcome = Outcome::Failed;
        let json = serde_json::to_string(&outcome).unwrap();
        assert_eq!(json, "\"failed\"");
        let parsed: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Outcome::Failed);
    }

    #[test]
    fn test_from_attempt_error_message_with_merged_outcome() {
        // Edge case: attempt has error_message but outcome is Merged
        let mut attempt = create_test_attempt();
        attempt.error_message = Some("Connection timed out".to_string());
        let issue = create_test_issue("Test", "Test");

        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        // error_type is derived from attempt.error_message regardless of outcome
        assert_eq!(outcome.error_type, Some("timeout".to_string()));
        assert!(outcome.outcome.is_success());
    }

    #[test]
    fn test_from_attempt_empty_prompt() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        let outcome = FixOutcome::from_attempt(&attempt, &issue, "", Outcome::Merged);
        assert_eq!(outcome.prompt_used, "");
    }

    #[test]
    fn test_from_attempt_preserves_attempt_id() {
        let mut attempt = create_test_attempt();
        attempt.id = 42;
        let issue = create_test_issue("Test", "Test");
        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        assert_eq!(outcome.attempt_id, 42);
    }

    #[test]
    fn test_from_attempt_issue_text_format() {
        let issue = create_test_issue("My Title", "My Description");
        let attempt = create_test_attempt();
        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        // issue_text should be "title\n\ndescription"
        assert_eq!(outcome.issue_text, "My Title\n\nMy Description");
    }

    #[test]
    fn test_from_attempt_issue_text_no_description() {
        let issue = Issue {
            id: "1".to_string(),
            short_id: "1".to_string(),
            title: "Title Only".to_string(),
            description: None,
            url: "u".to_string(),
            source: "t".to_string(),
            priority: IssuePriority::Medium,
            status: IssueStatus::Open,
            metadata: Default::default(),
            created_at: None,
            updated_at: None,
        };
        let attempt = create_test_attempt();
        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        // With no description, it should use empty string
        assert_eq!(outcome.issue_text, "Title Only\n\n");
    }

    #[test]
    fn test_load_non_contiguous_ids() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        let mut o1 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        o1.id = 5;
        let mut o2 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Failed);
        o2.id = 100;
        let mut o3 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Closed);
        o3.id = 50;

        tracker.load(vec![o1, o2, o3]);
        assert_eq!(tracker.all().len(), 3);

        // next_id should be max(5, 100, 50) + 1 = 101
        let new_id = tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        assert_eq!(new_id, 101);
    }

    #[test]
    fn test_set_embedding_overwrites_existing() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        let id = tracker.record(outcome).unwrap();

        tracker.set_embedding(id, vec![1.0, 2.0, 3.0]).unwrap();
        assert_eq!(
            tracker.all()[0].embedding.as_ref().unwrap(),
            &vec![1.0, 2.0, 3.0]
        );

        // Overwrite with a new embedding
        tracker.set_embedding(id, vec![4.0, 5.0, 6.0]).unwrap();
        assert_eq!(
            tracker.all()[0].embedding.as_ref().unwrap(),
            &vec![4.0, 5.0, 6.0]
        );
    }

    #[test]
    fn test_similarity_only_second_has_embedding() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        let outcome1 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        let mut outcome2 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        outcome2.set_embedding(vec![1.0, 0.0, 0.0]);

        // Only outcome2 has embedding; should return 0.0
        assert_eq!(outcome1.similarity(&outcome2), 0.0);
    }

    #[test]
    fn test_add_learnings_overwrites_existing() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        let id = tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();

        tracker.add_learnings(id, "First learning").unwrap();
        assert_eq!(
            tracker.all()[0].learnings.as_ref().unwrap(),
            "First learning"
        );

        tracker.add_learnings(id, "Updated learning").unwrap();
        assert_eq!(
            tracker.all()[0].learnings.as_ref().unwrap(),
            "Updated learning"
        );
    }

    #[test]
    fn test_common_errors_single_type() {
        let mut tracker = OutcomeTracker::new();
        let issue = create_test_issue("Test", "Test");
        let mut attempt = create_test_attempt();
        attempt.error_message = Some("Permission denied".to_string());

        for _ in 0..3 {
            tracker
                .record(FixOutcome::from_attempt(
                    &attempt,
                    &issue,
                    "p",
                    Outcome::Failed,
                ))
                .unwrap();
        }

        let errors = tracker.common_errors(10);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "permission");
        assert_eq!(errors[0].1, 3);
    }

    #[test]
    fn test_success_rate_mixed_outcomes() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        // Merged = success; Closed, Failed, CannotFix = not success
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Closed,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::CannotFix,
            ))
            .unwrap();

        let rate = tracker.success_rate(None);
        // 1 out of 4 = 0.25
        assert!((rate - 0.25).abs() < 0.01);
    }

    #[test]
    fn test_fix_outcome_initial_id_is_zero() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        // from_attempt sets id = 0 (to be assigned by storage)
        assert_eq!(outcome.id, 0);
    }

    #[test]
    fn test_fix_outcome_initial_learnings_is_none() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        assert!(outcome.learnings.is_none());
    }

    #[test]
    fn test_fix_outcome_initial_embedding_is_none() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");
        let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        assert!(outcome.embedding.is_none());
    }

    #[test]
    fn test_extract_keywords_preserves_underscored_words() {
        // Underscore should not split words
        let keywords =
            FixOutcome::extract_keywords("null_pointer_exception occurred", "stack trace follows");
        assert!(
            keywords.contains(&"null_pointer_exception".to_string()),
            "should keep underscored compound words intact, got: {:?}",
            keywords
        );
    }

    #[test]
    fn test_extract_keywords_deduplication_not_guaranteed() {
        // Duplicate words in input appear as many times as they pass the filter
        let keywords =
            FixOutcome::extract_keywords("database database database", "database database");
        // All should be "database" - verify they exist
        assert!(keywords.iter().all(|k| k == "database"));
    }

    #[test]
    fn test_common_errors_ordering_is_descending() {
        let mut tracker = OutcomeTracker::new();
        let issue = create_test_issue("Test", "Test");
        let mut attempt = create_test_attempt();

        // 1x syntax
        attempt.error_message = Some("Syntax error".to_string());
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();

        // 3x timeout
        attempt.error_message = Some("Connection timed out".to_string());
        for _ in 0..3 {
            tracker
                .record(FixOutcome::from_attempt(
                    &attempt,
                    &issue,
                    "p",
                    Outcome::Failed,
                ))
                .unwrap();
        }

        // 2x permission
        attempt.error_message = Some("Permission denied".to_string());
        for _ in 0..2 {
            tracker
                .record(FixOutcome::from_attempt(
                    &attempt,
                    &issue,
                    "p",
                    Outcome::Failed,
                ))
                .unwrap();
        }

        let errors = tracker.common_errors(10);
        assert_eq!(errors.len(), 3);
        // Should be sorted descending by count
        assert!(errors[0].1 >= errors[1].1);
        assert!(errors[1].1 >= errors[2].1);
        assert_eq!(errors[0].0, "timeout");
        assert_eq!(errors[0].1, 3);
    }

    #[test]
    fn test_fix_outcome_clone() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Clone test", "Testing clone");
        let mut outcome = FixOutcome::from_attempt(&attempt, &issue, "prompt", Outcome::Merged);
        outcome.set_embedding(vec![1.0, 2.0]);
        outcome.learnings = Some("learned something".to_string());

        let cloned = outcome.clone();
        assert_eq!(cloned.source, outcome.source);
        assert_eq!(cloned.issue_id, outcome.issue_id);
        assert_eq!(cloned.prompt_used, outcome.prompt_used);
        assert_eq!(cloned.outcome, outcome.outcome);
        assert_eq!(cloned.embedding, outcome.embedding);
        assert_eq!(cloned.learnings, outcome.learnings);
        assert_eq!(cloned.keywords, outcome.keywords);
    }

    #[test]
    fn test_outcome_copy() {
        let outcome = Outcome::Merged;
        let copied = outcome;
        // Both should still be usable since Outcome is Copy
        assert_eq!(outcome, copied);
    }

    #[test]
    fn test_load_then_record_then_load_again() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        // Load initial data
        let mut o1 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        o1.id = 10;
        tracker.load(vec![o1]);
        assert_eq!(tracker.all().len(), 1);

        // Record a new one
        let id = tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();
        assert_eq!(id, 11);
        assert_eq!(tracker.all().len(), 2);

        // Load again should replace everything
        let mut o2 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Closed);
        o2.id = 50;
        tracker.load(vec![o2]);
        assert_eq!(tracker.all().len(), 1);
        assert_eq!(tracker.all()[0].id, 50);

        // Next record should be 51
        let id2 = tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        assert_eq!(id2, 51);
    }

    #[test]
    fn test_set_embedding_then_check_similarity() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        let mut o1 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        let mut o2 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);

        // Initially no embeddings -> 0.0
        assert_eq!(o1.similarity(&o2), 0.0);

        // Set embeddings
        o1.set_embedding(vec![1.0, 0.0, 0.0]);
        o2.set_embedding(vec![0.0, 1.0, 0.0]);

        // Orthogonal -> near 0.0
        let sim = o1.similarity(&o2);
        assert!(
            sim.abs() < 0.01,
            "orthogonal vectors should be near 0, got {}",
            sim
        );
    }

    #[test]
    fn test_categorize_error_test_without_fail() {
        // "test" alone (without "fail") should not match test_failure
        assert_eq!(
            FixOutcome::categorize_error("running test suite"),
            "unknown"
        );
    }

    #[test]
    fn test_categorize_error_build_without_fail() {
        // "build" alone (without "fail") should not match build_failure
        assert_eq!(FixOutcome::categorize_error("build started"), "unknown");
    }

    #[test]
    fn test_categorize_error_conflict_detection() {
        assert_eq!(
            FixOutcome::categorize_error("merge conflict in file.rs"),
            "conflict"
        );
        assert_eq!(
            FixOutcome::categorize_error("CONFLICT detected"),
            "conflict"
        );
    }

    #[test]
    fn test_extract_keywords_with_underscores() {
        let keywords =
            FixOutcome::extract_keywords("null_pointer_exception", "stack_overflow_error");
        // Underscores should be kept (split on non-alphanumeric except _)
        assert!(keywords
            .iter()
            .any(|k| k.contains("null") || k.contains("pointer")));
    }

    #[test]
    fn test_extract_keywords_numbers_included() {
        let keywords = FixOutcome::extract_keywords("error404 response", "http500 status");
        // Words with numbers should be included if > 3 chars
        assert!(keywords.iter().any(|k| k.contains("error404")
            || k.contains("http500")
            || k.contains("response")
            || k.contains("status")));
    }

    #[test]
    fn test_get_by_outcome_after_load() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        let mut o1 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Merged);
        o1.id = 10;
        let mut o2 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Failed);
        o2.id = 20;
        let mut o3 = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::CannotFix);
        o3.id = 30;

        tracker.load(vec![o1, o2, o3]);

        assert_eq!(tracker.get_by_outcome(Outcome::Merged).len(), 1);
        assert_eq!(tracker.get_by_outcome(Outcome::Failed).len(), 1);
        assert_eq!(tracker.get_by_outcome(Outcome::CannotFix).len(), 1);
        assert_eq!(tracker.get_by_outcome(Outcome::Closed).len(), 0);
    }

    #[test]
    fn test_common_errors_limit_less_than_total() {
        let mut tracker = OutcomeTracker::new();
        let issue = create_test_issue("Test", "Test");
        let mut attempt = create_test_attempt();

        // Create 5 different error types
        for (i, err) in [
            "Timeout",
            "Permission denied",
            "Syntax error",
            "Test failed",
            "Build failed",
        ]
        .iter()
        .enumerate()
        {
            attempt.error_message = Some(err.to_string());
            for _ in 0..(5 - i) {
                tracker
                    .record(FixOutcome::from_attempt(
                        &attempt,
                        &issue,
                        "p",
                        Outcome::Failed,
                    ))
                    .unwrap();
            }
        }

        // Request only top 2
        let errors = tracker.common_errors(2);
        assert_eq!(errors.len(), 2);
        // First should be the most common
        assert!(errors[0].1 >= errors[1].1);
    }

    #[test]
    fn test_outcome_as_str_matches_parse() {
        for outcome in [
            Outcome::Merged,
            Outcome::Closed,
            Outcome::Failed,
            Outcome::CannotFix,
        ] {
            let s = outcome.as_str();
            let parsed = Outcome::parse(s).unwrap();
            assert_eq!(parsed, outcome);
        }
    }

    #[test]
    fn test_from_attempt_with_various_error_messages() {
        let issue = create_test_issue("Test", "Test");

        let error_cases = vec![
            ("Connection timed out after 30s", "timeout"),
            ("Access denied for user root", "permission"),
            ("Parse error: unexpected token", "syntax"),
            ("2 tests failed in suite", "test_failure"),
            ("Build failed: linker error", "build_failure"),
            ("Module not found: foo_bar", "not_found"),
            ("Merge conflict in src/main.rs", "conflict"),
            ("Something completely unexpected", "unknown"),
        ];

        for (error_msg, expected_type) in error_cases {
            let mut attempt = create_test_attempt();
            attempt.error_message = Some(error_msg.to_string());
            let outcome = FixOutcome::from_attempt(&attempt, &issue, "p", Outcome::Failed);
            assert_eq!(
                outcome.error_type.as_deref(),
                Some(expected_type),
                "error '{}' should be categorized as '{}'",
                error_msg,
                expected_type
            );
        }
    }

    #[test]
    fn test_fix_outcome_serde_full_roundtrip() {
        let attempt = create_test_attempt();
        let issue = create_test_issue("Serde Test", "Full roundtrip");
        let mut outcome =
            FixOutcome::from_attempt(&attempt, &issue, "prompt text", Outcome::Failed);
        outcome.id = 42;
        outcome.learnings = Some("Always check retries".to_string());
        outcome.set_embedding(vec![0.1, 0.2, 0.3]);

        let json = serde_json::to_string(&outcome).unwrap();
        let parsed: FixOutcome = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, 42);
        assert_eq!(parsed.source, outcome.source);
        assert_eq!(parsed.issue_id, outcome.issue_id);
        assert_eq!(parsed.prompt_used, "prompt text");
        assert_eq!(parsed.outcome, Outcome::Failed);
        assert_eq!(parsed.learnings, Some("Always check retries".to_string()));
        assert_eq!(parsed.embedding, Some(vec![0.1, 0.2, 0.3]));
        assert_eq!(parsed.keywords, outcome.keywords);
    }

    #[test]
    fn test_success_rate_closed_and_cannot_fix_not_success() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        // Only Closed and CannotFix, both should NOT count as success
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Closed,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::CannotFix,
            ))
            .unwrap();

        assert_eq!(tracker.success_rate(None), 0.0);
    }

    #[test]
    fn test_get_by_outcome_returns_correct_ids() {
        let mut tracker = OutcomeTracker::new();
        let attempt = create_test_attempt();
        let issue = create_test_issue("Test", "Test");

        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Failed,
            ))
            .unwrap();
        tracker
            .record(FixOutcome::from_attempt(
                &attempt,
                &issue,
                "p",
                Outcome::Merged,
            ))
            .unwrap();

        let merged = tracker.get_by_outcome(Outcome::Merged);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, 1);
        assert_eq!(merged[1].id, 3);
    }
}
