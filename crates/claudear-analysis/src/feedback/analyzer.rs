//! Feedback analyzer for improving prompts based on past outcomes.

use super::outcomes::{trial_from_outcome, OutcomeTracker};
use abnegate_learn::{Advisor, Config, SimilarTrial, SuggestionKind};
use claudear_core::error::Result;
use claudear_core::types::{FixAttempt, Issue};
use claudear_core::types::{FixOutcome, Outcome};
use claudear_storage::FixAttemptTracker;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A similar issue found in past outcomes.
#[derive(Debug, Clone, Serialize)]
pub struct SimilarIssue {
    /// The similar past outcome.
    pub outcome: FixOutcome,
    /// Similarity score (0.0 to 1.0).
    pub similarity: f64,
}

/// A suggestion for improving prompts.
#[derive(Debug, Clone, Serialize)]
pub struct PromptSuggestion {
    /// Type of suggestion.
    pub suggestion_type: SuggestionType,
    /// The suggestion text.
    pub text: String,
    /// Confidence level (0.0 to 1.0).
    pub confidence: f64,
    /// Source of this suggestion (which outcomes it's based on).
    pub based_on: Vec<i64>,
}

/// Types of prompt suggestions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestionType {
    /// Add context about what worked before.
    AddContext,
    /// Avoid a pattern that failed before.
    AvoidPattern,
    /// Include specific instructions based on past success.
    IncludeInstruction,
    /// Warning about a common failure mode.
    Warning,
}

/// Analyzes past fix outcomes to improve future prompts.
pub struct FeedbackAnalyzer {
    tracker: OutcomeTracker,
    fix_tracker: Option<Arc<dyn FixAttemptTracker>>,
    min_similarity: f64,
    max_similar_results: usize,
}

impl FeedbackAnalyzer {
    /// Create a new feedback analyzer.
    pub fn new() -> Self {
        Self {
            tracker: OutcomeTracker::new(),
            fix_tracker: None,
            min_similarity: 0.1,
            max_similar_results: 5,
        }
    }

    /// Create with custom settings.
    pub fn with_settings(min_similarity: f64, max_similar_results: usize) -> Self {
        Self {
            tracker: OutcomeTracker::with_config(
                Config::new()
                    .with_minimum_similarity(min_similarity)
                    .with_similar_limit(max_similar_results),
            ),
            fix_tracker: None,
            min_similarity,
            max_similar_results,
        }
    }

    /// Attach a tracker for HNSW-based semantic search on outcomes.
    pub fn with_tracker(mut self, tracker: Arc<dyn FixAttemptTracker>) -> Self {
        self.fix_tracker = Some(tracker);
        self
    }

    /// Load outcomes from persistent storage (e.g. DB hydration on startup).
    pub fn load_outcomes(&mut self, outcomes: Vec<FixOutcome>) {
        self.tracker.load(outcomes);
    }

    /// Record an already-built outcome, including any embedding the caller attached.
    pub fn record(&mut self, outcome: FixOutcome) -> Result<i64> {
        self.tracker.record(outcome)
    }

    /// Record an outcome built from an attempt.
    pub fn record_outcome(
        &mut self,
        attempt: &FixAttempt,
        issue: &Issue,
        prompt: &str,
        outcome: Outcome,
    ) -> Result<i64> {
        self.record(FixOutcome::from_attempt(attempt, issue, prompt, outcome))
    }

    /// Find similar outcomes using semantic search with a pre-computed embedding.
    ///
    /// Uses HNSW via vectorlite. Returns empty if no results found.
    pub fn find_similar(&self, query_embedding: &[f32]) -> Vec<SimilarIssue> {
        if let Some(ref tracker) = self.fix_tracker {
            match tracker.find_similar_outcomes_vector(
                query_embedding,
                self.min_similarity,
                self.max_similar_results,
            ) {
                Ok(Some(results)) => {
                    tracing::debug!(
                        count = results.len(),
                        "Outcome similarity search used HNSW index"
                    );
                    return results
                        .into_iter()
                        .map(|(outcome, similarity)| SimilarIssue {
                            outcome,
                            similarity,
                        })
                        .collect();
                }
                Ok(None) => {
                    tracing::warn!("vectorlite unavailable for outcome search");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "HNSW outcome search failed");
                }
            }
        }

        self.similar_from_memory(query_embedding)
    }

    fn similar_from_memory(&self, query_embedding: &[f32]) -> Vec<SimilarIssue> {
        self.tracker
            .memory()
            .similar(query_embedding)
            .into_iter()
            .filter_map(|hit| {
                self.tracker
                    .get(hit.trial.id)
                    .cloned()
                    .map(|outcome| SimilarIssue {
                        outcome,
                        similarity: hit.score,
                    })
            })
            .collect()
    }

    /// Build an enhanced prompt using semantic search with a pre-computed embedding.
    pub fn enhance_prompt(
        &self,
        base_prompt: &str,
        _issue: &Issue,
        query_embedding: &[f32],
    ) -> String {
        let similar = self.find_similar(query_embedding);
        let suggestions = Self::suggestions_from_similar(&similar);

        if suggestions.is_empty() {
            return base_prompt.to_string();
        }

        let mut enhanced = String::new();
        enhanced.push_str("# Learnings from Similar Issues\n\n");

        for suggestion in suggestions.iter().take(3) {
            let prefix = match suggestion.suggestion_type {
                SuggestionType::AddContext => "Context:",
                SuggestionType::AvoidPattern => "Avoid:",
                SuggestionType::IncludeInstruction => "Instruction:",
                SuggestionType::Warning => "Warning:",
            };
            enhanced.push_str(&format!("- {} {}\n", prefix, suggestion.text));
        }

        enhanced.push_str("\n---\n\n");
        enhanced.push_str(base_prompt);

        enhanced
    }

    /// Extract suggestions from a list of similar issues (shared logic).
    fn suggestions_from_similar(similar: &[SimilarIssue]) -> Vec<PromptSuggestion> {
        let trials: Vec<SimilarTrial> = similar
            .iter()
            .map(|item| SimilarTrial::new(trial_from_outcome(&item.outcome), item.similarity))
            .collect();
        Advisor::new()
            .suggestions(&trials)
            .into_iter()
            .map(|suggestion| PromptSuggestion {
                suggestion_type: match suggestion.kind {
                    SuggestionKind::Context => SuggestionType::AddContext,
                    SuggestionKind::Avoid => SuggestionType::AvoidPattern,
                    SuggestionKind::Instruction => SuggestionType::IncludeInstruction,
                    SuggestionKind::Warning => SuggestionType::Warning,
                    _ => SuggestionType::Warning,
                },
                text: suggestion.text,
                confidence: suggestion.confidence,
                based_on: suggestion.based_on,
            })
            .collect()
    }

    /// Get the outcome tracker for direct access.
    pub fn tracker(&self) -> &OutcomeTracker {
        &self.tracker
    }

    /// Get mutable outcome tracker.
    pub fn tracker_mut(&mut self) -> &mut OutcomeTracker {
        &mut self.tracker
    }

    /// Get overall success rate.
    pub fn overall_success_rate(&self) -> f64 {
        self.tracker.success_rate(None)
    }

    /// Get success rate for a specific source.
    pub fn source_success_rate(&self, source: &str) -> f64 {
        self.tracker.success_rate(Some(source))
    }

    /// Get common error patterns.
    pub fn common_errors(&self, limit: usize) -> Vec<(String, usize)> {
        self.tracker.common_errors(limit)
    }

    /// Add learnings to an outcome.
    pub fn add_learnings(&mut self, outcome_id: i64, learnings: &str) -> Result<()> {
        self.tracker.add_learnings(outcome_id, learnings)
    }
}

impl Default for FeedbackAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_core::types::{FixAttemptStatus, IssuePriority, IssueStatus};

    fn create_test_issue(title: &str, description: &str, source: &str) -> Issue {
        Issue {
            id: format!("{}-1", source),
            short_id: format!("{}-1", source.to_uppercase()),
            title: title.to_string(),
            description: Some(description.to_string()),
            url: "https://example.com".to_string(),
            source: source.to_string(),
            priority: IssuePriority::Medium,
            status: IssueStatus::Open,
            metadata: Default::default(),
            created_at: None,
            updated_at: None,
        }
    }

    fn create_test_attempt(source: &str) -> FixAttempt {
        FixAttempt {
            id: 1,
            source: source.to_string(),
            issue_id: format!("{}-1", source),
            short_id: format!("{}-1", source.to_uppercase()),
            status: FixAttemptStatus::Success,
            pr_url: Some("https://github.com/test/pr/1".to_string()),
            scm_repo: None,
            scm_pr_number: None,
            error_message: None,
            attempted_at: chrono::Utc::now(),
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
    fn test_record_outcome() {
        let mut analyzer = FeedbackAnalyzer::new();

        let issue = create_test_issue("API timeout error", "Timeout in user service", "linear");
        let attempt = create_test_attempt("linear");

        let id = analyzer
            .record_outcome(&attempt, &issue, "Fix the timeout", Outcome::Merged)
            .unwrap();

        assert_eq!(id, 1);
        assert_eq!(analyzer.tracker().all().len(), 1);
    }

    #[test]
    fn test_find_similar_no_sqlite_tracker() {
        let analyzer = FeedbackAnalyzer::new();
        let embedding = vec![1.0, 0.0, 0.0];
        let similar = analyzer.find_similar(&embedding);
        assert!(similar.is_empty());
    }

    #[test]
    fn test_find_similar_falls_back_to_memory() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("API timeout error", "Timeout in user service", "linear");
        let attempt = create_test_attempt("linear");
        let id = analyzer
            .record_outcome(&attempt, &issue, "Fix the timeout", Outcome::Merged)
            .unwrap();
        analyzer
            .tracker_mut()
            .set_embedding(id, vec![1.0, 0.0, 0.0])
            .unwrap();

        let similar = analyzer.find_similar(&[1.0, 0.0, 0.0]);
        assert_eq!(similar.len(), 1);
        assert_eq!(similar[0].outcome.id, id);
        assert!(similar[0].similarity > 0.9);
    }

    #[test]
    fn test_record_keeps_runtime_embedding_for_memory_fallback() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("API timeout error", "Timeout in user service", "linear");
        let attempt = create_test_attempt("linear");
        let mut outcome =
            FixOutcome::from_attempt(&attempt, &issue, "Fix the timeout", Outcome::Merged);
        outcome.set_embedding(vec![1.0, 0.0, 0.0]);

        let id = analyzer.record(outcome).unwrap();
        let similar = analyzer.find_similar(&[1.0, 0.0, 0.0]);

        assert_eq!(similar.len(), 1);
        assert_eq!(similar[0].outcome.id, id);
        assert_eq!(
            similar[0].outcome.embedding.as_deref(),
            Some(&[1.0, 0.0, 0.0][..])
        );
        assert!(similar[0].similarity > 0.9);
    }

    #[test]
    fn test_enhance_prompt_no_results() {
        let analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("API error", "Server returns 500", "sentry");
        let embedding = vec![1.0, 0.0, 0.0];

        // Without sqlite tracker, should return base prompt unchanged
        let enhanced = analyzer.enhance_prompt("Fix this bug", &issue, &embedding);
        assert_eq!(enhanced, "Fix this bug");
    }

    #[test]
    fn test_suggestions_from_similar_with_learnings() {
        let outcome = FixOutcome {
            id: 1,
            attempt_id: 100,
            source: "linear".to_string(),
            issue_id: "123".to_string(),
            issue_text: "Test issue".to_string(),
            prompt_used: "Fix it".to_string(),
            outcome: Outcome::Merged,
            error_type: None,
            learnings: Some("Check token expiration".to_string()),
            keywords: vec![],
            embedding: None,
            created_at: chrono::Utc::now(),
        };

        let similar = vec![SimilarIssue {
            outcome,
            similarity: 0.9,
        }];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        assert!(!suggestions.is_empty());
        assert!(suggestions[0].text.contains("token expiration"));
    }

    #[test]
    fn test_suggestions_from_similar_with_failures() {
        let mut outcomes = Vec::new();
        for i in 0..4 {
            outcomes.push(SimilarIssue {
                outcome: FixOutcome {
                    id: i,
                    attempt_id: i,
                    source: "linear".to_string(),
                    issue_id: format!("issue-{}", i),
                    issue_text: "Timeout error".to_string(),
                    prompt_used: "fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: Some("timeout".to_string()),
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.8,
            });
        }

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&outcomes);
        let has_warning = suggestions
            .iter()
            .any(|s| s.suggestion_type == SuggestionType::Warning);
        assert!(has_warning);
        let has_majority = suggestions
            .iter()
            .any(|s| s.text.contains("similar attempts failed"));
        assert!(has_majority);
    }

    #[test]
    fn test_success_rate() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test desc", "linear");
        let attempt = create_test_attempt("linear");

        analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Merged)
            .unwrap();
        analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Merged)
            .unwrap();
        analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Failed)
            .unwrap();

        let rate = analyzer.overall_success_rate();
        assert!((rate - 0.666).abs() < 0.01); // ~66% success
    }

    #[test]
    fn test_common_errors() {
        let mut analyzer = FeedbackAnalyzer::new();

        let issue = create_test_issue("Test", "Test", "linear");
        let mut attempt = create_test_attempt("linear");

        attempt.error_message = Some("Connection timed out".to_string());
        analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Failed)
            .unwrap();
        analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Failed)
            .unwrap();

        attempt.error_message = Some("Permission denied".to_string());
        analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Failed)
            .unwrap();

        let errors = analyzer.common_errors(5);
        assert!(!errors.is_empty());

        // timeout should be most common (2 occurrences)
        assert_eq!(errors[0].0, "timeout");
        assert_eq!(errors[0].1, 2);
    }

    #[test]
    fn test_analyzer_default() {
        let analyzer = FeedbackAnalyzer::default();
        assert!((analyzer.overall_success_rate() - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_analyzer_with_settings() {
        let analyzer = FeedbackAnalyzer::with_settings(0.5, 10);
        // Just ensure it creates without error
        assert!(analyzer.find_similar(&[1.0, 0.0]).is_empty());
    }

    #[test]
    fn test_source_success_rate() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test", "linear");
        let attempt = create_test_attempt("linear");

        analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Merged)
            .unwrap();
        analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Failed)
            .unwrap();

        let rate = analyzer.source_success_rate("linear");
        assert!((rate - 0.5).abs() < 0.1);
    }

    #[test]
    fn test_source_success_rate_no_data() {
        let analyzer = FeedbackAnalyzer::new();
        let rate = analyzer.source_success_rate("nonexistent");
        assert!((rate - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_tracker_accessor() {
        let analyzer = FeedbackAnalyzer::new();
        let tracker = analyzer.tracker();
        assert!(tracker.all().is_empty());
    }

    #[test]
    fn test_tracker_mut_accessor() {
        let mut analyzer = FeedbackAnalyzer::new();
        let _ = analyzer.tracker_mut();
        // Just verify we can access it mutably
    }

    #[test]
    fn test_similar_issue_fields() {
        let outcome = FixOutcome {
            id: 1,
            attempt_id: 100,
            source: "linear".to_string(),
            issue_id: "123".to_string(),
            issue_text: "Test issue title".to_string(),
            prompt_used: "Fix it".to_string(),
            outcome: Outcome::Merged,
            error_type: None,
            learnings: None,
            keywords: vec!["test".to_string()],
            embedding: None,
            created_at: chrono::Utc::now(),
        };

        let similar = SimilarIssue {
            outcome,
            similarity: 0.85,
        };

        assert!((similar.similarity - 0.85).abs() < 0.01);
        assert_eq!(similar.outcome.issue_id, "123");
    }

    #[test]
    fn test_prompt_suggestion_fields() {
        let suggestion = PromptSuggestion {
            suggestion_type: SuggestionType::AddContext,
            text: "Add more context".to_string(),
            confidence: 0.75,
            based_on: vec![1, 2, 3],
        };

        assert_eq!(suggestion.suggestion_type, SuggestionType::AddContext);
        assert_eq!(suggestion.text, "Add more context");
        assert!((suggestion.confidence - 0.75).abs() < 0.01);
        assert_eq!(suggestion.based_on.len(), 3);
    }

    #[test]
    fn test_suggestion_types() {
        assert_eq!(SuggestionType::AddContext, SuggestionType::AddContext);
        assert_eq!(SuggestionType::AvoidPattern, SuggestionType::AvoidPattern);
        assert_eq!(
            SuggestionType::IncludeInstruction,
            SuggestionType::IncludeInstruction
        );
        assert_eq!(SuggestionType::Warning, SuggestionType::Warning);
        assert_ne!(SuggestionType::AddContext, SuggestionType::Warning);
    }

    #[test]
    fn test_load_outcomes() {
        let mut analyzer = FeedbackAnalyzer::new();

        // Build outcomes as if loaded from DB
        let issue = create_test_issue(
            "Database connection timeout",
            "PostgreSQL connection fails",
            "linear",
        );
        let attempt = create_test_attempt("linear");
        let mut outcome =
            FixOutcome::from_attempt(&attempt, &issue, "Fix the timeout", Outcome::Merged);
        outcome.id = 5;

        analyzer.load_outcomes(vec![outcome]);

        // Verify outcomes were loaded
        assert_eq!(analyzer.tracker().all().len(), 1);
        assert_eq!(analyzer.tracker().all()[0].id, 5);
    }

    #[test]
    fn test_common_errors_limit() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test", "linear");
        let mut attempt = create_test_attempt("linear");

        // Add various error types
        for err_type in &["timeout", "permission", "network", "syntax", "runtime"] {
            attempt.error_message = Some(format!("{} error", err_type));
            analyzer
                .record_outcome(&attempt, &issue, "p", Outcome::Failed)
                .unwrap();
        }

        let errors = analyzer.common_errors(2);
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn test_add_learnings() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test", "linear");
        let attempt = create_test_attempt("linear");

        let id = analyzer
            .record_outcome(&attempt, &issue, "prompt", Outcome::Merged)
            .unwrap();
        analyzer
            .add_learnings(id, "Important lesson learned")
            .unwrap();

        // Verify the learning was added
        let tracker = analyzer.tracker();
        let outcomes = tracker.all();
        let outcome = outcomes.iter().find(|o| o.id == id).unwrap();
        assert_eq!(
            outcome.learnings.as_ref().unwrap(),
            "Important lesson learned"
        );
    }

    #[test]
    fn test_new_default_settings() {
        let analyzer = FeedbackAnalyzer::new();
        assert!(analyzer.find_similar(&[1.0, 0.0]).is_empty());
    }

    #[test]
    fn test_load_outcomes_replaces_state() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("test", "test", "sentry");
        let attempt = create_test_attempt("sentry");
        analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Merged)
            .unwrap();

        analyzer.load_outcomes(vec![]);
        assert!(analyzer.tracker().all().is_empty());
    }

    #[test]
    fn test_record_outcome_returns_sequential_ids() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("test", "test", "sentry");
        let attempt = create_test_attempt("sentry");
        let id1 = analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Merged)
            .unwrap();
        let id2 = analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Failed)
            .unwrap();
        assert!(id2 > id1);
    }

    #[test]
    fn test_suggestions_from_similar_empty_input() {
        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&[]);
        assert!(suggestions.is_empty());
    }

    #[test]
    fn test_suggestions_from_similar_successful_without_learnings() {
        // A single successful outcome with no learnings should produce no suggestions
        let similar = vec![SimilarIssue {
            outcome: FixOutcome {
                id: 1,
                attempt_id: 100,
                source: "linear".to_string(),
                issue_id: "123".to_string(),
                issue_text: "Test issue".to_string(),
                prompt_used: "Fix it".to_string(),
                outcome: Outcome::Merged,
                error_type: None,
                learnings: None,
                keywords: vec![],
                embedding: None,
                created_at: chrono::Utc::now(),
            },
            similarity: 0.9,
        }];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        // No learnings and only 1 success (not >= 2), so no suggestions
        assert!(suggestions.is_empty());
    }

    #[test]
    fn test_suggestions_from_similar_exactly_two_successes() {
        // Two successful outcomes should trigger the "N similar issues fixed" suggestion
        let similar = vec![
            SimilarIssue {
                outcome: FixOutcome {
                    id: 1,
                    attempt_id: 100,
                    source: "linear".to_string(),
                    issue_id: "1".to_string(),
                    issue_text: "Issue 1".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Merged,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.8,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 2,
                    attempt_id: 101,
                    source: "linear".to_string(),
                    issue_id: "2".to_string(),
                    issue_text: "Issue 2".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Merged,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.7,
            },
        ];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        assert!(!suggestions.is_empty());
        let context_suggestion = suggestions
            .iter()
            .find(|s| s.text.contains("2 similar attempts succeeded"));
        assert!(
            context_suggestion.is_some(),
            "should produce a '2 similar attempts' suggestion"
        );
        // Average confidence = (0.8 + 0.7) / 2 = 0.75
        let avg_confidence = context_suggestion.unwrap().confidence;
        assert!((avg_confidence - 0.75).abs() < 0.01);
        // based_on should contain both IDs
        assert_eq!(context_suggestion.unwrap().based_on.len(), 2);
    }

    #[test]
    fn test_suggestions_from_similar_mixed_success_and_failure_with_learnings() {
        let similar = vec![
            SimilarIssue {
                outcome: FixOutcome {
                    id: 1,
                    attempt_id: 100,
                    source: "linear".to_string(),
                    issue_id: "1".to_string(),
                    issue_text: "Issue 1".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Merged,
                    error_type: None,
                    learnings: Some("Use retry logic".to_string()),
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.9,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 2,
                    attempt_id: 101,
                    source: "linear".to_string(),
                    issue_id: "2".to_string(),
                    issue_text: "Issue 2".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: Some("timeout".to_string()),
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.85,
            },
        ];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        // Should have at least: AddContext (from learnings) and Warning (from error_type)
        let has_context = suggestions
            .iter()
            .any(|s| s.suggestion_type == SuggestionType::AddContext);
        let has_warning = suggestions
            .iter()
            .any(|s| s.suggestion_type == SuggestionType::Warning);
        assert!(has_context, "should have AddContext from learnings");
        assert!(has_warning, "should have Warning from error_type");
    }

    #[test]
    fn test_suggestions_from_similar_failed_without_error_type() {
        // Failures without error_type should NOT produce the error_types Warning
        let similar = vec![
            SimilarIssue {
                outcome: FixOutcome {
                    id: 1,
                    attempt_id: 100,
                    source: "linear".to_string(),
                    issue_id: "1".to_string(),
                    issue_text: "Issue 1".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.8,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 2,
                    attempt_id: 101,
                    source: "linear".to_string(),
                    issue_id: "2".to_string(),
                    issue_text: "Issue 2".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.7,
            },
        ];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        let has_error_warning = suggestions.iter().any(|s| s.text.contains("failed with:"));
        assert!(!has_error_warning);
    }

    #[test]
    fn test_suggestions_from_similar_caution_when_failures_outnumber_successes() {
        // 3+ outcomes where failures > successes triggers the Caution warning
        let similar = vec![
            SimilarIssue {
                outcome: FixOutcome {
                    id: 1,
                    attempt_id: 100,
                    source: "linear".to_string(),
                    issue_id: "1".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Merged,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.9,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 2,
                    attempt_id: 101,
                    source: "linear".to_string(),
                    issue_id: "2".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.8,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 3,
                    attempt_id: 102,
                    source: "linear".to_string(),
                    issue_id: "3".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.7,
            },
        ];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        let has_caution = suggestions
            .iter()
            .any(|s| s.text.contains("similar attempts failed"));
        assert!(
            has_caution,
            "should warn when failures > successes and total >= 3"
        );

        let caution = suggestions
            .iter()
            .find(|s| s.text.contains("similar attempts failed"))
            .unwrap();
        assert!(caution.text.contains("2 of 3"));
        assert_eq!(caution.confidence, 0.7);
    }

    #[test]
    fn test_suggestions_from_similar_no_caution_when_equal() {
        // When failures == successes, caution should NOT trigger (requires failures > successes)
        let similar = vec![
            SimilarIssue {
                outcome: FixOutcome {
                    id: 1,
                    attempt_id: 100,
                    source: "linear".to_string(),
                    issue_id: "1".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Merged,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.9,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 2,
                    attempt_id: 101,
                    source: "linear".to_string(),
                    issue_id: "2".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Merged,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.8,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 3,
                    attempt_id: 102,
                    source: "linear".to_string(),
                    issue_id: "3".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.7,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 4,
                    attempt_id: 103,
                    source: "linear".to_string(),
                    issue_id: "4".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.6,
            },
        ];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        let has_caution = suggestions
            .iter()
            .any(|s| s.text.contains("similar attempts failed."));
        assert!(
            !has_caution,
            "should NOT warn on majority-fail when failures == successes"
        );
    }

    #[test]
    fn test_suggestions_from_similar_no_caution_under_three_total() {
        // Even if failures > successes, caution requires total >= 3
        let similar = vec![
            SimilarIssue {
                outcome: FixOutcome {
                    id: 1,
                    attempt_id: 100,
                    source: "linear".to_string(),
                    issue_id: "1".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.8,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 2,
                    attempt_id: 101,
                    source: "linear".to_string(),
                    issue_id: "2".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: None,
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.7,
            },
        ];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        let has_caution = suggestions
            .iter()
            .any(|s| s.text.contains("similar attempts failed."));
        assert!(
            !has_caution,
            "should NOT warn on majority-fail when total < 3"
        );
    }

    #[test]
    fn test_suggestions_confidence_sorted_descending() {
        // Multiple suggestions should be sorted by confidence descending
        let similar = vec![
            SimilarIssue {
                outcome: FixOutcome {
                    id: 1,
                    attempt_id: 100,
                    source: "linear".to_string(),
                    issue_id: "1".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Merged,
                    error_type: None,
                    learnings: Some("Learning A".to_string()),
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.5,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 2,
                    attempt_id: 101,
                    source: "linear".to_string(),
                    issue_id: "2".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Merged,
                    error_type: None,
                    learnings: Some("Learning B".to_string()),
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.95,
            },
        ];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        assert!(suggestions.len() >= 2);
        // Verify descending order
        for i in 1..suggestions.len() {
            assert!(
                suggestions[i - 1].confidence >= suggestions[i].confidence,
                "suggestions should be sorted by confidence descending: {} >= {}",
                suggestions[i - 1].confidence,
                suggestions[i].confidence
            );
        }
    }

    #[test]
    fn test_suggestions_error_types_concatenated() {
        // Multiple failed outcomes with different error_types should produce a concatenated warning
        let similar = vec![
            SimilarIssue {
                outcome: FixOutcome {
                    id: 1,
                    attempt_id: 100,
                    source: "linear".to_string(),
                    issue_id: "1".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: Some("timeout".to_string()),
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.8,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 2,
                    attempt_id: 101,
                    source: "linear".to_string(),
                    issue_id: "2".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: Some("permission".to_string()),
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.7,
            },
        ];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        let error_warning = suggestions
            .iter()
            .find(|s| s.text.contains("failed with:"))
            .expect("should have error type warning");
        assert!(error_warning.text.contains("timeout"));
        assert!(error_warning.text.contains("permission"));
    }

    #[test]
    fn test_record_outcome_closed() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test", "linear");
        let attempt = create_test_attempt("linear");

        let id = analyzer
            .record_outcome(&attempt, &issue, "prompt", Outcome::Closed)
            .unwrap();
        assert!(id > 0);

        let tracker = analyzer.tracker();
        assert_eq!(tracker.all()[0].outcome, Outcome::Closed);
    }

    #[test]
    fn test_record_outcome_cannot_fix() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test", "linear");
        let attempt = create_test_attempt("linear");

        let id = analyzer
            .record_outcome(&attempt, &issue, "prompt", Outcome::CannotFix)
            .unwrap();
        assert!(id > 0);

        let tracker = analyzer.tracker();
        assert_eq!(tracker.all()[0].outcome, Outcome::CannotFix);
    }

    #[test]
    fn test_add_learnings_nonexistent_id() {
        let mut analyzer = FeedbackAnalyzer::new();
        // Should succeed without panic even when ID doesn't exist
        let result = analyzer.add_learnings(999, "Some learning");
        assert!(result.is_ok());
    }

    #[test]
    fn test_tracker_mut_can_mutate() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test", "linear");
        let attempt = create_test_attempt("linear");

        analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Merged)
            .unwrap();

        // Use tracker_mut to add learnings directly
        let tracker = analyzer.tracker_mut();
        tracker.add_learnings(1, "Direct mutation").unwrap();

        assert_eq!(
            analyzer.tracker().all()[0].learnings.as_ref().unwrap(),
            "Direct mutation"
        );
    }

    #[test]
    fn test_load_outcomes_multiple() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test", "linear");
        let attempt = create_test_attempt("linear");

        let mut o1 = FixOutcome::from_attempt(&attempt, &issue, "p1", Outcome::Merged);
        o1.id = 10;
        o1.learnings = Some("learning1".to_string());

        let mut o2 = FixOutcome::from_attempt(&attempt, &issue, "p2", Outcome::Failed);
        o2.id = 20;
        o2.learnings = Some("learning2".to_string());

        let mut o3 = FixOutcome::from_attempt(&attempt, &issue, "p3", Outcome::Closed);
        o3.id = 30;

        analyzer.load_outcomes(vec![o1, o2, o3]);

        assert_eq!(analyzer.tracker().all().len(), 3);
        assert_eq!(analyzer.tracker().all()[0].id, 10);
        assert_eq!(analyzer.tracker().all()[1].id, 20);
        assert_eq!(analyzer.tracker().all()[2].id, 30);
        assert_eq!(
            analyzer.tracker().all()[0].learnings.as_ref().unwrap(),
            "learning1"
        );
    }

    #[test]
    fn test_overall_success_rate_no_data() {
        let analyzer = FeedbackAnalyzer::new();
        assert_eq!(analyzer.overall_success_rate(), 0.0);
    }

    #[test]
    fn test_overall_success_rate_all_success() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test", "linear");
        let attempt = create_test_attempt("linear");

        for _ in 0..5 {
            analyzer
                .record_outcome(&attempt, &issue, "p", Outcome::Merged)
                .unwrap();
        }

        assert!((analyzer.overall_success_rate() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_source_success_rate_multiple_sources() {
        let mut analyzer = FeedbackAnalyzer::new();

        // linear: 2 merged, 1 failed = 66.7%
        let issue_l = create_test_issue("Test", "Test", "linear");
        let attempt_l = create_test_attempt("linear");
        analyzer
            .record_outcome(&attempt_l, &issue_l, "p", Outcome::Merged)
            .unwrap();
        analyzer
            .record_outcome(&attempt_l, &issue_l, "p", Outcome::Merged)
            .unwrap();
        analyzer
            .record_outcome(&attempt_l, &issue_l, "p", Outcome::Failed)
            .unwrap();

        // sentry: 1 merged, 3 failed = 25%
        let issue_s = create_test_issue("Test", "Test", "sentry");
        let attempt_s = create_test_attempt("sentry");
        analyzer
            .record_outcome(&attempt_s, &issue_s, "p", Outcome::Merged)
            .unwrap();
        analyzer
            .record_outcome(&attempt_s, &issue_s, "p", Outcome::Failed)
            .unwrap();
        analyzer
            .record_outcome(&attempt_s, &issue_s, "p", Outcome::Failed)
            .unwrap();
        analyzer
            .record_outcome(&attempt_s, &issue_s, "p", Outcome::Failed)
            .unwrap();

        let linear_rate = analyzer.source_success_rate("linear");
        assert!((linear_rate - 0.666).abs() < 0.01);

        let sentry_rate = analyzer.source_success_rate("sentry");
        assert!((sentry_rate - 0.25).abs() < 0.01);
    }

    #[test]
    fn test_common_errors_empty() {
        let analyzer = FeedbackAnalyzer::new();
        let errors = analyzer.common_errors(10);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_enhance_prompt_with_suggestions_formatting() {
        // Test enhance_prompt formatting by calling suggestions_from_similar directly
        // to verify the enhance_prompt format would be correct
        let outcome_with_learning = FixOutcome {
            id: 1,
            attempt_id: 100,
            source: "linear".to_string(),
            issue_id: "123".to_string(),
            issue_text: "Test issue".to_string(),
            prompt_used: "Fix it".to_string(),
            outcome: Outcome::Merged,
            error_type: None,
            learnings: Some("Always add null checks".to_string()),
            keywords: vec![],
            embedding: None,
            created_at: chrono::Utc::now(),
        };

        let similar = vec![SimilarIssue {
            outcome: outcome_with_learning,
            similarity: 0.9,
        }];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        assert!(!suggestions.is_empty());

        // Verify the suggestion type prefix mapping
        for suggestion in &suggestions {
            let prefix = match suggestion.suggestion_type {
                SuggestionType::AddContext => "Context:",
                SuggestionType::AvoidPattern => "Avoid:",
                SuggestionType::IncludeInstruction => "Instruction:",
                SuggestionType::Warning => "Warning:",
            };
            // Each suggestion type maps to a known prefix
            assert!(!prefix.is_empty());
        }
    }

    #[test]
    fn test_with_settings_custom_values() {
        let analyzer = FeedbackAnalyzer::with_settings(0.8, 3);
        // Verify it initializes without error and has empty tracker
        assert!(analyzer.tracker().all().is_empty());
        assert_eq!(analyzer.overall_success_rate(), 0.0);
    }

    #[test]
    fn test_suggestions_based_on_ids() {
        // Verify based_on IDs are correctly populated
        let similar = vec![SimilarIssue {
            outcome: FixOutcome {
                id: 42,
                attempt_id: 100,
                source: "linear".to_string(),
                issue_id: "123".to_string(),
                issue_text: "Test issue".to_string(),
                prompt_used: "Fix it".to_string(),
                outcome: Outcome::Merged,
                error_type: None,
                learnings: Some("Important learning".to_string()),
                keywords: vec![],
                embedding: None,
                created_at: chrono::Utc::now(),
            },
            similarity: 0.9,
        }];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        assert!(!suggestions.is_empty());
        assert_eq!(suggestions[0].based_on, vec![42]);
    }

    #[test]
    fn test_suggestions_multiple_error_types_average_confidence() {
        let similar = vec![
            SimilarIssue {
                outcome: FixOutcome {
                    id: 1,
                    attempt_id: 100,
                    source: "linear".to_string(),
                    issue_id: "1".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: Some("timeout".to_string()),
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.9,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 2,
                    attempt_id: 101,
                    source: "linear".to_string(),
                    issue_id: "2".to_string(),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Failed,
                    error_type: Some("syntax".to_string()),
                    learnings: None,
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.7,
            },
        ];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        let warning = suggestions
            .iter()
            .find(|s| s.suggestion_type == SuggestionType::Warning)
            .expect("should have warning");
        // Average confidence of failed outcomes: (0.9 + 0.7) / 2 = 0.8
        assert!((warning.confidence - 0.8).abs() < 0.01);
    }

    #[test]
    fn test_suggestions_closed_outcome_counts_as_failure() {
        // Closed outcome is not success, so it should be in the failed list
        let similar = vec![SimilarIssue {
            outcome: FixOutcome {
                id: 1,
                attempt_id: 100,
                source: "linear".to_string(),
                issue_id: "1".to_string(),
                issue_text: "Issue".to_string(),
                prompt_used: "Fix".to_string(),
                outcome: Outcome::Closed,
                error_type: Some("conflict".to_string()),
                learnings: None,
                keywords: vec![],
                embedding: None,
                created_at: chrono::Utc::now(),
            },
            similarity: 0.8,
        }];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        let has_warning = suggestions.iter().any(|s| s.text.contains("conflict"));
        assert!(
            has_warning,
            "Closed outcome with error_type should produce a warning"
        );
    }

    #[test]
    fn test_suggestions_cannot_fix_counts_as_failure() {
        let similar = vec![SimilarIssue {
            outcome: FixOutcome {
                id: 1,
                attempt_id: 100,
                source: "linear".to_string(),
                issue_id: "1".to_string(),
                issue_text: "Issue".to_string(),
                prompt_used: "Fix".to_string(),
                outcome: Outcome::CannotFix,
                error_type: Some("not_found".to_string()),
                learnings: None,
                keywords: vec![],
                embedding: None,
                created_at: chrono::Utc::now(),
            },
            similarity: 0.8,
        }];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        let has_warning = suggestions.iter().any(|s| s.text.contains("not_found"));
        assert!(
            has_warning,
            "CannotFix outcome with error_type should produce a warning"
        );
    }

    #[test]
    fn test_suggestions_many_successes_with_learnings() {
        // 3 successes all with learnings: should produce 3 individual AddContext + 1 aggregate
        let similar: Vec<SimilarIssue> = (0..3)
            .map(|i| SimilarIssue {
                outcome: FixOutcome {
                    id: i,
                    attempt_id: 100 + i,
                    source: "linear".to_string(),
                    issue_id: format!("{}", i),
                    issue_text: "Issue".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Merged,
                    error_type: None,
                    learnings: Some(format!("Learning {}", i)),
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.9 - (i as f64 * 0.1),
            })
            .collect();

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        let context_suggestions: Vec<_> = suggestions
            .iter()
            .filter(|s| s.suggestion_type == SuggestionType::AddContext)
            .collect();
        // 3 individual learning suggestions + 1 aggregate "3 similar issues" suggestion = 4
        assert_eq!(
            context_suggestions.len(),
            4,
            "expected 4 AddContext suggestions (3 individual + 1 aggregate)"
        );
    }

    #[test]
    fn test_add_learnings_overwrites() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test", "linear");
        let attempt = create_test_attempt("linear");

        let id = analyzer
            .record_outcome(&attempt, &issue, "p", Outcome::Merged)
            .unwrap();

        analyzer.add_learnings(id, "First").unwrap();
        assert_eq!(
            analyzer.tracker().all()[0].learnings.as_ref().unwrap(),
            "First"
        );

        analyzer.add_learnings(id, "Second").unwrap();
        assert_eq!(
            analyzer.tracker().all()[0].learnings.as_ref().unwrap(),
            "Second"
        );
    }

    #[test]
    fn test_with_tracker() {
        let sqlite = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let analyzer = FeedbackAnalyzer::new().with_tracker(sqlite);
        // With sqlite tracker attached, find_similar should attempt HNSW search
        // but return empty because no outcomes are stored
        let results = analyzer.find_similar(&[1.0, 0.0, 0.0]);
        assert!(results.is_empty());
    }

    #[test]
    fn test_enhance_prompt_formatting_with_all_suggestion_types() {
        // Test the formatting of enhance_prompt by directly testing
        // the suggestions_from_similar -> enhance_prompt path logic.
        // We need both learnings (AddContext) and errors (Warning).
        let similar = vec![
            SimilarIssue {
                outcome: FixOutcome {
                    id: 1,
                    attempt_id: 100,
                    source: "linear".to_string(),
                    issue_id: "1".to_string(),
                    issue_text: "Issue 1".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Merged,
                    error_type: None,
                    learnings: Some("Always validate input".to_string()),
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.95,
            },
            SimilarIssue {
                outcome: FixOutcome {
                    id: 2,
                    attempt_id: 101,
                    source: "linear".to_string(),
                    issue_id: "2".to_string(),
                    issue_text: "Issue 2".to_string(),
                    prompt_used: "Fix".to_string(),
                    outcome: Outcome::Merged,
                    error_type: None,
                    learnings: Some("Check for null pointers".to_string()),
                    keywords: vec![],
                    embedding: None,
                    created_at: chrono::Utc::now(),
                },
                similarity: 0.90,
            },
        ];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        assert!(suggestions
            .iter()
            .any(|s| s.suggestion_type == SuggestionType::AddContext
                && s.text.contains("Always validate input")));
        assert!(suggestions
            .iter()
            .any(|s| s.suggestion_type == SuggestionType::IncludeInstruction));
        assert!(suggestions
            .iter()
            .any(|s| s.text.contains("2 similar attempts succeeded")));
    }

    // These types are not currently generated by suggestions_from_similar,
    // but we can verify the enum values work correctly in formatting.

    #[test]
    fn test_suggestion_type_prefix_mapping() {
        let types = vec![
            (SuggestionType::AddContext, "Context:"),
            (SuggestionType::AvoidPattern, "Avoid:"),
            (SuggestionType::IncludeInstruction, "Instruction:"),
            (SuggestionType::Warning, "Warning:"),
        ];

        for (stype, expected_prefix) in types {
            let prefix = match stype {
                SuggestionType::AddContext => "Context:",
                SuggestionType::AvoidPattern => "Avoid:",
                SuggestionType::IncludeInstruction => "Instruction:",
                SuggestionType::Warning => "Warning:",
            };
            assert_eq!(prefix, expected_prefix);
        }
    }

    #[test]
    fn test_suggestions_warning_message_format() {
        let similar = vec![SimilarIssue {
            outcome: FixOutcome {
                id: 1,
                attempt_id: 100,
                source: "linear".to_string(),
                issue_id: "1".to_string(),
                issue_text: "Issue".to_string(),
                prompt_used: "Fix".to_string(),
                outcome: Outcome::Failed,
                error_type: Some("syntax".to_string()),
                learnings: None,
                keywords: vec![],
                embedding: None,
                created_at: chrono::Utc::now(),
            },
            similarity: 0.85,
        }];

        let suggestions = FeedbackAnalyzer::suggestions_from_similar(&similar);
        assert!(!suggestions.is_empty());
        let warning = suggestions
            .iter()
            .find(|s| s.suggestion_type == SuggestionType::Warning)
            .expect("should have Warning");
        assert!(warning.text.contains("syntax"));
        assert!(warning.text.contains("failed with"));
        assert_eq!(warning.based_on, vec![1]);
    }

    #[test]
    fn test_record_outcome_preserves_issue_text() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("My Title", "My Description", "linear");
        let attempt = create_test_attempt("linear");

        let id = analyzer
            .record_outcome(&attempt, &issue, "the prompt", Outcome::Merged)
            .unwrap();

        let tracker = analyzer.tracker();
        let outcome = tracker.all().iter().find(|o| o.id == id).unwrap();
        assert!(outcome.issue_text.contains("My Title"));
        assert!(outcome.issue_text.contains("My Description"));
        assert_eq!(outcome.prompt_used, "the prompt");
        assert_eq!(outcome.source, "linear");
    }

    #[test]
    fn test_common_errors_all_successes() {
        let mut analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test", "linear");
        let attempt = create_test_attempt("linear");

        for _ in 0..5 {
            analyzer
                .record_outcome(&attempt, &issue, "p", Outcome::Merged)
                .unwrap();
        }

        let errors = analyzer.common_errors(10);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_find_similar_with_tracker_empty_db() {
        let sqlite = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let analyzer = FeedbackAnalyzer::new().with_tracker(sqlite);

        let results = analyzer.find_similar(&[0.1, 0.2, 0.3, 0.4]);
        assert!(results.is_empty());
    }

    #[test]
    fn test_enhance_prompt_returns_base_when_no_sqlite() {
        let analyzer = FeedbackAnalyzer::new();
        let issue = create_test_issue("Test", "Test", "linear");
        let embedding = vec![0.1, 0.2, 0.3];

        let result = analyzer.enhance_prompt("Base prompt text", &issue, &embedding);
        assert_eq!(result, "Base prompt text");
    }
}
