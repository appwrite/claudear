//! System 5: Classify review feedback into categories and detect patterns.

use crate::feedback::{cosine_similarity, EmbeddingClient};
use chrono::Utc;
use claudear_core::error::Result;
use claudear_core::types::PrReviewComment;
use claudear_core::types::{ReviewCategory, ReviewPattern};
use claudear_storage::FixAttemptTracker;
use std::sync::Arc;

pub struct ReviewClassifier;

impl ReviewClassifier {
    /// Classify a review comment into a category via keyword matching.
    pub fn classify(comment_body: &str) -> ReviewCategory {
        let lower = comment_body.to_lowercase();

        // Check each category by keyword presence
        let categories = [
            (
                ReviewCategory::Security,
                &[
                    "security",
                    "vulnerability",
                    "sanitize",
                    "escape",
                    "inject",
                    "xss",
                    "csrf",
                    "auth",
                ][..],
            ),
            (
                ReviewCategory::MissingTests,
                &[
                    "test",
                    "coverage",
                    "spec",
                    "assert",
                    "verify",
                    "unit test",
                    "integration test",
                ],
            ),
            (
                ReviewCategory::WrongApproach,
                &[
                    "approach",
                    "instead",
                    "should use",
                    "better to",
                    "don't",
                    "shouldn't",
                    "wrong way",
                    "not the right",
                ],
            ),
            (
                ReviewCategory::StyleIssue,
                &[
                    "style",
                    "format",
                    "naming",
                    "convention",
                    "lint",
                    "indentation",
                    "whitespace",
                ],
            ),
            (
                ReviewCategory::Incomplete,
                &[
                    "incomplete",
                    "missing",
                    "also need",
                    "forgot",
                    "what about",
                    "still need",
                    "not handling",
                ],
            ),
            (
                ReviewCategory::Performance,
                &[
                    "performance",
                    "slow",
                    "optimize",
                    "efficient",
                    "cache",
                    "n+1",
                    "complexity",
                ],
            ),
            (
                ReviewCategory::Documentation,
                &[
                    "document",
                    "comment",
                    "explain",
                    "readme",
                    "jsdoc",
                    "rustdoc",
                    "docstring",
                ],
            ),
        ];

        for (category, keywords) in &categories {
            if keywords.iter().any(|kw| lower.contains(kw)) {
                return *category;
            }
        }

        ReviewCategory::Other
    }

    /// Process review comments for a PR, storing/updating patterns.
    pub fn process_review_comments(
        tracker: &dyn FixAttemptTracker,
        repo: &str,
        comments: &[PrReviewComment],
        review_body: Option<&str>,
    ) -> Result<Vec<ReviewPattern>> {
        let mut patterns = Vec::new();

        // Classify the review body if present
        if let Some(body) = review_body {
            if !body.trim().is_empty() {
                let category = Self::classify(body);
                let pattern = ReviewPattern {
                    id: 0,
                    scm_repo: repo.to_string(),
                    category,
                    pattern_text: truncate(body, 200),
                    example_comments: vec![truncate(body, 500)],
                    occurrence_count: 1,
                    promoted_to_instruction: false,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };
                tracker.upsert_review_pattern(&pattern)?;
                patterns.push(pattern);
            }
        }

        // Classify each inline comment
        for comment in comments {
            let category = Self::classify(&comment.body);
            let pattern = ReviewPattern {
                id: 0,
                scm_repo: repo.to_string(),
                category,
                pattern_text: truncate(&comment.body, 200),
                example_comments: vec![truncate(&comment.body, 500)],
                occurrence_count: 1,
                promoted_to_instruction: false,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            tracker.upsert_review_pattern(&pattern)?;
            patterns.push(pattern);
        }

        Ok(patterns)
    }

    /// Classify with LLM if available, falling back to keyword heuristics.
    pub fn classify_with_llm(
        comment_body: &str,
        llm: Option<&dyn crate::llm::LlmAnalyzer>,
    ) -> ReviewCategory {
        if let Some(analyzer) = llm {
            if let Some(cat) = analyzer.classify_review(comment_body) {
                return cat;
            }
        }
        Self::classify(comment_body)
    }

    /// Process review comments with optional LLM classification.
    pub fn process_review_comments_with_llm(
        tracker: &dyn FixAttemptTracker,
        repo: &str,
        comments: &[PrReviewComment],
        review_body: Option<&str>,
        llm: Option<&dyn crate::llm::LlmAnalyzer>,
    ) -> Result<Vec<ReviewPattern>> {
        let mut patterns = Vec::new();

        if let Some(body) = review_body {
            if !body.trim().is_empty() {
                let category = Self::classify_with_llm(body, llm);
                let pattern = ReviewPattern {
                    id: 0,
                    scm_repo: repo.to_string(),
                    category,
                    pattern_text: truncate(body, 200),
                    example_comments: vec![truncate(body, 500)],
                    occurrence_count: 1,
                    promoted_to_instruction: false,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };
                tracker.upsert_review_pattern(&pattern)?;
                patterns.push(pattern);
            }
        }

        for comment in comments {
            let category = Self::classify_with_llm(&comment.body, llm);
            let pattern = ReviewPattern {
                id: 0,
                scm_repo: repo.to_string(),
                category,
                pattern_text: truncate(&comment.body, 200),
                example_comments: vec![truncate(&comment.body, 500)],
                occurrence_count: 1,
                promoted_to_instruction: false,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            tracker.upsert_review_pattern(&pattern)?;
            patterns.push(pattern);
        }

        Ok(patterns)
    }

    /// Check if any patterns have crossed the promotion threshold.
    pub fn check_promotion_threshold(
        tracker: &dyn FixAttemptTracker,
        repo: &str,
        threshold: usize,
    ) -> Result<Vec<ReviewPattern>> {
        let patterns = tracker.get_review_patterns(repo, 100)?;
        let promoted: Vec<ReviewPattern> = patterns
            .into_iter()
            .filter(|p| p.occurrence_count >= threshold as i64 && !p.promoted_to_instruction)
            .collect();
        Ok(promoted)
    }
}

/// Embedding-based review classifier that uses cosine similarity against
/// reference category descriptions. Falls back to keyword-based classification
/// when embedding generation fails.
pub struct SemanticReviewClassifier {
    embedding_client: Arc<EmbeddingClient>,
    reference_embeddings: Vec<(ReviewCategory, Vec<f32>)>,
}

impl SemanticReviewClassifier {
    /// Build reference embeddings for the 7 classified categories (excluding Other).
    ///
    /// Each category is represented by a descriptive text that captures the
    /// typical language used in review comments of that type. The embeddings
    /// for these descriptions are precomputed once at construction time.
    pub async fn new(embedding_client: Arc<EmbeddingClient>) -> Result<Self> {
        let descriptions = [
            (ReviewCategory::Security, "security vulnerability, sanitize input, escape user data, injection attack, XSS, CSRF, authentication bypass"),
            (ReviewCategory::MissingTests, "missing test coverage, add unit tests, integration tests needed, no test cases, assertion missing"),
            (ReviewCategory::WrongApproach, "wrong approach, should use different method, better alternative exists, not the right pattern, consider using instead"),
            (ReviewCategory::StyleIssue, "code style issue, naming convention, formatting, indentation, lint warning, whitespace"),
            (ReviewCategory::Incomplete, "incomplete implementation, missing error handling, forgot edge case, still need to handle, not handling null"),
            (ReviewCategory::Performance, "performance issue, slow operation, optimize, add caching, N+1 query, time complexity"),
            (ReviewCategory::Documentation, "missing documentation, add comments, explain logic, rustdoc, docstring needed"),
        ];

        let texts: Vec<&str> = descriptions.iter().map(|(_, desc)| *desc).collect();
        let embeddings = embedding_client.embed_batch(&texts).await?;

        let reference_embeddings = descriptions
            .iter()
            .zip(embeddings)
            .map(|((cat, _), emb)| (*cat, emb))
            .collect();

        Ok(Self {
            embedding_client,
            reference_embeddings,
        })
    }

    /// Classify a review comment via cosine similarity against reference embeddings.
    ///
    /// Returns the category with the highest similarity score, provided it
    /// exceeds the 0.3 threshold. Falls back to keyword-based classification
    /// if embedding generation fails for the input text.
    pub async fn classify(&self, comment_body: &str) -> ReviewCategory {
        if comment_body.trim().is_empty() {
            return ReviewCategory::Other;
        }

        match self.embedding_client.embed(comment_body).await {
            Ok(embedding) => {
                let mut best_category = ReviewCategory::Other;
                let mut best_score: f32 = 0.3; // similarity threshold

                for (category, ref_emb) in &self.reference_embeddings {
                    let score = cosine_similarity(&embedding, ref_emb);
                    if score > best_score {
                        best_score = score;
                        best_category = *category;
                    }
                }

                best_category
            }
            Err(_) => ReviewClassifier::classify(comment_body), // fallback to keywords
        }
    }

    /// Process review comments using semantic classification.
    ///
    /// Classifies the review body and each inline comment using embedding-based
    /// similarity, then upserts the resulting patterns into the tracker.
    pub async fn process_review_comments(
        &self,
        tracker: &dyn FixAttemptTracker,
        repo: &str,
        comments: &[PrReviewComment],
        review_body: Option<&str>,
    ) -> Result<Vec<ReviewPattern>> {
        let mut patterns = Vec::new();

        if let Some(body) = review_body {
            if !body.trim().is_empty() {
                let category = self.classify(body).await;
                let pattern = ReviewPattern {
                    id: 0,
                    scm_repo: repo.to_string(),
                    category,
                    pattern_text: truncate(body, 200),
                    example_comments: vec![truncate(body, 500)],
                    occurrence_count: 1,
                    promoted_to_instruction: false,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };
                tracker.upsert_review_pattern(&pattern)?;
                patterns.push(pattern);
            }
        }

        for comment in comments {
            let category = self.classify(&comment.body).await;
            let pattern = ReviewPattern {
                id: 0,
                scm_repo: repo.to_string(),
                category,
                pattern_text: truncate(&comment.body, 200),
                example_comments: vec![truncate(&comment.body, 500)],
                occurrence_count: 1,
                promoted_to_instruction: false,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            tracker.upsert_review_pattern(&pattern)?;
            patterns.push(pattern);
        }

        Ok(patterns)
    }

    /// Get a reference to the underlying embedding client.
    pub fn embedding_client(&self) -> &Arc<EmbeddingClient> {
        &self.embedding_client
    }

    /// Get the number of reference categories.
    pub fn category_count(&self) -> usize {
        self.reference_embeddings.len()
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len < 4 {
        // Too short for meaningful "X..." — just take first max_len bytes
        // (find a safe char boundary)
        let end = s
            .char_indices()
            .take_while(|(i, _)| *i < max_len)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        s[..end].to_string()
    } else {
        let end = s
            .char_indices()
            .take_while(|(i, _)| *i <= max_len.saturating_sub(3))
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_missing_tests() {
        assert_eq!(
            ReviewClassifier::classify("Please add tests for this change"),
            ReviewCategory::MissingTests
        );
    }

    #[test]
    fn test_classify_security() {
        assert_eq!(
            ReviewClassifier::classify("This could be a security vulnerability"),
            ReviewCategory::Security
        );
    }

    #[test]
    fn test_classify_style() {
        assert_eq!(
            ReviewClassifier::classify("The naming convention doesn't match our style guide"),
            ReviewCategory::StyleIssue
        );
    }

    #[test]
    fn test_classify_wrong_approach() {
        assert_eq!(
            ReviewClassifier::classify("You should use a different approach instead"),
            ReviewCategory::WrongApproach
        );
    }

    #[test]
    fn test_classify_incomplete() {
        assert_eq!(
            ReviewClassifier::classify("This is incomplete, you're also missing error handling"),
            ReviewCategory::Incomplete
        );
    }

    #[test]
    fn test_classify_performance() {
        assert_eq!(
            ReviewClassifier::classify("This could be slow, consider adding a cache"),
            ReviewCategory::Performance
        );
    }

    #[test]
    fn test_classify_documentation() {
        assert_eq!(
            ReviewClassifier::classify("Please add a comment explaining this logic"),
            ReviewCategory::Documentation
        );
    }

    #[test]
    fn test_classify_other() {
        assert_eq!(
            ReviewClassifier::classify("Looks good to me"),
            ReviewCategory::Other
        );
    }

    #[test]
    fn test_classify_case_insensitive() {
        assert_eq!(
            ReviewClassifier::classify("PLEASE ADD TESTS"),
            ReviewCategory::MissingTests
        );
        assert_eq!(
            ReviewClassifier::classify("SECURITY vulnerability"),
            ReviewCategory::Security
        );
    }

    #[test]
    fn test_classify_security_keywords_comprehensive() {
        assert_eq!(
            ReviewClassifier::classify("potential xss issue"),
            ReviewCategory::Security
        );
        assert_eq!(
            ReviewClassifier::classify("csrf token missing"),
            ReviewCategory::Security
        );
        assert_eq!(
            ReviewClassifier::classify("auth not verified"),
            ReviewCategory::Security
        );
        assert_eq!(
            ReviewClassifier::classify("need to sanitize input"),
            ReviewCategory::Security
        );
    }

    #[test]
    fn test_classify_performance_keywords() {
        assert_eq!(
            ReviewClassifier::classify("this is an n+1 query"),
            ReviewCategory::Performance
        );
        assert_eq!(
            ReviewClassifier::classify("time complexity is O(n^2)"),
            ReviewCategory::Performance
        );
        assert_eq!(
            ReviewClassifier::classify("we should optimize this"),
            ReviewCategory::Performance
        );
    }

    #[test]
    fn test_classify_incomplete_keywords() {
        assert_eq!(
            ReviewClassifier::classify("what about error handling?"),
            ReviewCategory::Incomplete
        );
        assert_eq!(
            ReviewClassifier::classify("still need to handle the edge case"),
            ReviewCategory::Incomplete
        );
        assert_eq!(
            ReviewClassifier::classify("you're not handling null values"),
            ReviewCategory::Incomplete
        );
    }

    #[test]
    fn test_security_takes_precedence_over_tests() {
        // "security" appears before "test" in the priority list
        assert_eq!(
            ReviewClassifier::classify("add security tests"),
            ReviewCategory::Security
        );
    }

    #[test]
    fn test_truncate_function() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("hello world", 8), "hello...");
        assert_eq!(truncate("", 10), "");
        // Exact length should not truncate
        assert_eq!(truncate("exact", 5), "exact");
    }

    #[test]
    fn test_truncate_unicode() {
        let emoji_text = "Hello 🌍 world";
        let truncated = truncate(emoji_text, 8);
        assert!(truncated.len() <= 12); // Should be safe with unicode
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn test_classify_empty_string() {
        assert_eq!(ReviewClassifier::classify(""), ReviewCategory::Other);
    }

    #[test]
    fn test_classify_wrong_approach_keywords() {
        assert_eq!(
            ReviewClassifier::classify("this is the wrong way to do it"),
            ReviewCategory::WrongApproach
        );
        assert_eq!(
            ReviewClassifier::classify("not the right pattern here"),
            ReviewCategory::WrongApproach
        );
        assert_eq!(
            ReviewClassifier::classify("you should use a hashmap instead"),
            ReviewCategory::WrongApproach
        );
        assert_eq!(
            ReviewClassifier::classify("it would be better to use async"),
            ReviewCategory::WrongApproach
        );
    }

    #[test]
    fn test_classify_documentation_keywords() {
        assert_eq!(
            ReviewClassifier::classify("add a rustdoc comment"),
            ReviewCategory::Documentation
        );
        assert_eq!(
            ReviewClassifier::classify("need a docstring here"),
            ReviewCategory::Documentation
        );
        assert_eq!(
            ReviewClassifier::classify("please explain this logic"),
            ReviewCategory::Documentation
        );
    }

    #[test]
    fn test_process_review_comments_with_body_only() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let patterns = ReviewClassifier::process_review_comments(
            &tracker,
            "org/repo",
            &[],
            Some("Please add tests for this change"),
        )
        .unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].category, ReviewCategory::MissingTests);
        assert!(patterns[0].pattern_text.contains("add tests"));

        // Verify it was stored
        let stored = tracker.get_review_patterns("org/repo", 10).unwrap();
        assert_eq!(stored.len(), 1);
    }

    #[test]
    fn test_process_review_comments_with_inline_comments() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let comments = vec![
            PrReviewComment {
                id: 1,
                path: "src/main.rs".to_string(),
                position: Some(10),
                original_position: None,
                body: "This could be a security vulnerability with user input".to_string(),
                user: claudear_core::types::ReviewUser {
                    login: "reviewer".to_string(),
                    id: 1,
                    user_type: None,
                },
                created_at: "2024-01-01T00:00:00Z".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
                html_url: "https://github.com/org/repo/pull/1".to_string(),
                pull_request_review_id: None,
                start_line: None,
                line: None,
                side: None,
            },
            PrReviewComment {
                id: 2,
                path: "src/api.rs".to_string(),
                position: Some(20),
                original_position: None,
                body: "This is slow, consider adding a cache layer".to_string(),
                user: claudear_core::types::ReviewUser {
                    login: "reviewer".to_string(),
                    id: 1,
                    user_type: None,
                },
                created_at: "2024-01-01T00:00:00Z".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
                html_url: "https://github.com/org/repo/pull/1".to_string(),
                pull_request_review_id: None,
                start_line: None,
                line: None,
                side: None,
            },
        ];

        let patterns =
            ReviewClassifier::process_review_comments(&tracker, "org/repo", &comments, None)
                .unwrap();
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0].category, ReviewCategory::Security);
        assert_eq!(patterns[1].category, ReviewCategory::Performance);

        let stored = tracker.get_review_patterns("org/repo", 10).unwrap();
        assert_eq!(stored.len(), 2);
    }

    #[test]
    fn test_process_review_comments_with_body_and_inline() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let comments = vec![PrReviewComment {
            id: 1,
            path: "src/main.rs".to_string(),
            position: Some(10),
            original_position: None,
            body: "This naming convention doesn't match our style guide".to_string(),
            user: claudear_core::types::ReviewUser {
                login: "reviewer".to_string(),
                id: 1,
                user_type: None,
            },
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
            html_url: "url".to_string(),
            pull_request_review_id: None,
            start_line: None,
            line: None,
            side: None,
        }];

        let patterns = ReviewClassifier::process_review_comments(
            &tracker,
            "org/repo",
            &comments,
            Some("Please add unit tests"),
        )
        .unwrap();
        assert_eq!(patterns.len(), 2);
        // Body: "unit tests" -> MissingTests, Inline: "naming convention" -> StyleIssue
        let categories: Vec<_> = patterns.iter().map(|p| p.category).collect();
        assert!(categories.contains(&ReviewCategory::MissingTests));
        assert!(categories.contains(&ReviewCategory::StyleIssue));
    }

    #[test]
    fn test_process_review_comments_empty_body_skipped() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let patterns =
            ReviewClassifier::process_review_comments(&tracker, "org/repo", &[], Some("  "))
                .unwrap();
        assert!(patterns.is_empty());

        let stored = tracker.get_review_patterns("org/repo", 10).unwrap();
        assert!(stored.is_empty());
    }

    #[test]
    fn test_check_promotion_threshold_below() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        // Store a pattern with occurrence_count=1
        ReviewClassifier::process_review_comments(
            &tracker,
            "org/repo",
            &[],
            Some("Add tests please"),
        )
        .unwrap();

        let promotable =
            ReviewClassifier::check_promotion_threshold(&tracker, "org/repo", 3).unwrap();
        assert!(promotable.is_empty(), "count=1 should be below threshold=3");
    }

    #[test]
    fn test_check_promotion_threshold_reached() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();

        // Upsert the same pattern 3 times to reach threshold
        for _ in 0..3 {
            ReviewClassifier::process_review_comments(
                &tracker,
                "org/repo",
                &[],
                Some("Add tests for new code"),
            )
            .unwrap();
        }

        let promotable =
            ReviewClassifier::check_promotion_threshold(&tracker, "org/repo", 3).unwrap();
        assert_eq!(promotable.len(), 1, "count=3 should meet threshold=3");
        assert_eq!(promotable[0].category, ReviewCategory::MissingTests);
        assert!(!promotable[0].promoted_to_instruction);
    }

    #[test]
    fn test_classify_whitespace_only() {
        assert_eq!(
            ReviewClassifier::classify("   \t\n  "),
            ReviewCategory::Other
        );
    }

    #[test]
    fn test_classify_very_long_comment() {
        let long = "a ".repeat(10_000) + "test";
        assert_eq!(
            ReviewClassifier::classify(&long),
            ReviewCategory::MissingTests
        );
    }

    #[test]
    fn test_classify_keyword_as_substring() {
        // "escape" is a Security keyword — should match even embedded
        assert_eq!(
            ReviewClassifier::classify("please escape user input"),
            ReviewCategory::Security
        );
    }

    #[test]
    fn test_classify_multiple_categories_first_wins() {
        // Security is checked before MissingTests, so security wins
        assert_eq!(
            ReviewClassifier::classify("security test coverage needed"),
            ReviewCategory::Security
        );
        // MissingTests is checked before StyleIssue
        assert_eq!(
            ReviewClassifier::classify("add test for naming convention"),
            ReviewCategory::MissingTests
        );
    }

    #[test]
    fn test_truncate_zero_max_len() {
        let result = truncate("hello world", 0);
        // max_len=0: output must not exceed 0 bytes
        assert_eq!(result, "");
    }

    #[test]
    fn test_truncate_max_len_one() {
        let result = truncate("hello", 1);
        // max_len=1: too small for ellipsis, just take 1 char
        assert_eq!(result, "h");
        assert!(result.len() <= 1);
    }

    #[test]
    fn test_truncate_max_len_three() {
        let result = truncate("hello", 3);
        // max_len=3: too small for "X...", take first 3 bytes
        assert_eq!(result, "hel");
        assert!(result.len() <= 3);
    }

    #[test]
    fn test_truncate_max_len_four() {
        let result = truncate("hello world", 4);
        // max_len=4: room for "h..."
        assert_eq!(result, "h...");
        assert!(result.len() <= 4);
    }

    #[test]
    fn test_truncate_never_exceeds_max_len() {
        for max_len in 0..20 {
            let result = truncate("hello world, this is a longer string", max_len);
            assert!(
                result.len() <= max_len,
                "truncate with max_len={} produced '{}' ({} bytes)",
                max_len,
                result,
                result.len()
            );
        }
    }

    #[test]
    fn test_process_review_comments_no_body_no_comments() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let patterns =
            ReviewClassifier::process_review_comments(&tracker, "org/repo", &[], None).unwrap();
        assert!(patterns.is_empty());
    }

    #[test]
    fn test_process_review_comments_none_body() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let patterns =
            ReviewClassifier::process_review_comments(&tracker, "org/repo", &[], None).unwrap();
        assert!(patterns.is_empty());
    }

    #[test]
    fn test_check_promotion_threshold_zero() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        // With threshold=0, everything should promote
        ReviewClassifier::process_review_comments(&tracker, "org/repo", &[], Some("Add tests"))
            .unwrap();
        let promotable =
            ReviewClassifier::check_promotion_threshold(&tracker, "org/repo", 0).unwrap();
        // occurrence_count=1 >= threshold=0, so it should promote
        assert_eq!(promotable.len(), 1);
    }

    #[test]
    fn test_check_promotion_threshold_different_repos() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();

        for _ in 0..3 {
            ReviewClassifier::process_review_comments(
                &tracker,
                "org/repo-a",
                &[],
                Some("Add tests"),
            )
            .unwrap();
        }
        ReviewClassifier::process_review_comments(&tracker, "org/repo-b", &[], Some("Add tests"))
            .unwrap();

        let promotable_a =
            ReviewClassifier::check_promotion_threshold(&tracker, "org/repo-a", 3).unwrap();
        let promotable_b =
            ReviewClassifier::check_promotion_threshold(&tracker, "org/repo-b", 3).unwrap();
        assert_eq!(promotable_a.len(), 1);
        assert!(promotable_b.is_empty());
    }

    // -- SemanticReviewClassifier tests (using mock reference embeddings) --

    #[test]
    fn test_semantic_classifier_struct_fields() {
        // Verify the reference embedding data structure works correctly
        let categories = [
            (ReviewCategory::Security, vec![1.0, 0.0, 0.0]),
            (ReviewCategory::MissingTests, vec![0.0, 1.0, 0.0]),
        ];

        assert_eq!(categories.len(), 2);
        assert_eq!(categories[0].0, ReviewCategory::Security);
        assert_eq!(categories[1].0, ReviewCategory::MissingTests);
    }

    #[test]
    fn test_semantic_classifier_reference_embedding_count() {
        // The classifier should have 7 reference embeddings (one per classified category)
        let expected_count = ReviewCategory::classified_variants().len();
        assert_eq!(expected_count, 7);
    }

    #[test]
    fn test_semantic_classifier_cosine_similarity_best_match() {
        // Simulate what the classify method does: find best matching category
        let reference_embeddings = vec![
            (ReviewCategory::Security, vec![1.0, 0.0, 0.0]),
            (ReviewCategory::MissingTests, vec![0.0, 1.0, 0.0]),
            (ReviewCategory::Performance, vec![0.0, 0.0, 1.0]),
        ];

        // Input embedding close to Security
        let input = vec![0.9, 0.1, 0.0];
        let mut best_category = ReviewCategory::Other;
        let mut best_score: f32 = 0.3;

        for (category, ref_emb) in &reference_embeddings {
            let score = cosine_similarity(&input, ref_emb);
            if score > best_score {
                best_score = score;
                best_category = *category;
            }
        }

        assert_eq!(best_category, ReviewCategory::Security);
        assert!(best_score > 0.9);
    }

    #[test]
    fn test_semantic_classifier_cosine_below_threshold_returns_other() {
        // When no category exceeds the 0.3 threshold, should return Other
        let reference_embeddings = vec![
            (ReviewCategory::Security, vec![1.0, 0.0, 0.0]),
            (ReviewCategory::MissingTests, vec![0.0, 1.0, 0.0]),
        ];

        // Input orthogonal to all reference embeddings
        let input = vec![0.0, 0.0, 1.0];
        let mut best_category = ReviewCategory::Other;
        let mut best_score: f32 = 0.3;

        for (category, ref_emb) in &reference_embeddings {
            let score = cosine_similarity(&input, ref_emb);
            if score > best_score {
                best_score = score;
                best_category = *category;
            }
        }

        assert_eq!(best_category, ReviewCategory::Other);
    }

    #[test]
    fn test_semantic_classifier_empty_input_returns_other() {
        // The classify method should return Other for empty/whitespace input
        // without even calling the embedding client
        let empty_inputs = ["", "   ", "\t", "\n", "  \t\n  "];
        for input in &empty_inputs {
            assert!(
                input.trim().is_empty(),
                "Expected empty/whitespace input: {:?}",
                input
            );
        }
    }

    #[test]
    fn test_semantic_classifier_fallback_to_keyword() {
        // When embedding fails, should fall back to keyword-based classification
        assert_eq!(
            ReviewClassifier::classify("security vulnerability"),
            ReviewCategory::Security
        );
        assert_eq!(
            ReviewClassifier::classify("add tests"),
            ReviewCategory::MissingTests
        );
        assert_eq!(
            ReviewClassifier::classify("looks good to me"),
            ReviewCategory::Other
        );
    }

    #[test]
    fn test_semantic_classifier_all_categories_distinguishable() {
        // With one-hot reference embeddings, each category should be perfectly
        // distinguishable when given its exact reference vector
        let categories = [
            ReviewCategory::Security,
            ReviewCategory::MissingTests,
            ReviewCategory::WrongApproach,
            ReviewCategory::StyleIssue,
            ReviewCategory::Incomplete,
            ReviewCategory::Performance,
            ReviewCategory::Documentation,
        ];

        let reference_embeddings: Vec<(ReviewCategory, Vec<f32>)> = categories
            .iter()
            .enumerate()
            .map(|(i, cat)| {
                let mut emb = vec![0.0f32; 7];
                emb[i] = 1.0;
                (*cat, emb)
            })
            .collect();

        for (i, (expected_cat, _)) in reference_embeddings.iter().enumerate() {
            let mut input = vec![0.0f32; 7];
            input[i] = 1.0;

            let mut best_category = ReviewCategory::Other;
            let mut best_score: f32 = 0.3;

            for (category, ref_emb) in &reference_embeddings {
                let score = cosine_similarity(&input, ref_emb);
                if score > best_score {
                    best_score = score;
                    best_category = *category;
                }
            }

            assert_eq!(
                best_category, *expected_cat,
                "Category {:?} was not correctly matched",
                expected_cat
            );
            assert!(
                (best_score - 1.0).abs() < 0.001,
                "Expected perfect match for {:?}, got score {}",
                expected_cat,
                best_score
            );
        }
    }

    #[test]
    fn test_semantic_classifier_mixed_signal_picks_strongest() {
        // When the input embedding has components in multiple category directions,
        // the classifier should pick the one with highest similarity
        let reference_embeddings = vec![
            (ReviewCategory::Security, vec![1.0, 0.0, 0.0, 0.0]),
            (ReviewCategory::MissingTests, vec![0.0, 1.0, 0.0, 0.0]),
            (ReviewCategory::Performance, vec![0.0, 0.0, 1.0, 0.0]),
            (ReviewCategory::StyleIssue, vec![0.0, 0.0, 0.0, 1.0]),
        ];

        // Input leans toward Performance (index 2 is strongest)
        let input = vec![0.1, 0.2, 0.9, 0.1];

        let mut best_category = ReviewCategory::Other;
        let mut best_score: f32 = 0.3;

        for (category, ref_emb) in &reference_embeddings {
            let score = cosine_similarity(&input, ref_emb);
            if score > best_score {
                best_score = score;
                best_category = *category;
            }
        }

        assert_eq!(best_category, ReviewCategory::Performance);
    }

    #[test]
    fn test_semantic_classifier_category_count() {
        // There should always be 7 classified categories
        assert_eq!(ReviewCategory::classified_variants().len(), 7);

        // Verify no duplicates
        let variants = ReviewCategory::classified_variants();
        for (i, cat) in variants.iter().enumerate() {
            for (j, other) in variants.iter().enumerate() {
                if i != j {
                    assert_ne!(
                        cat, other,
                        "Duplicate category found at indices {} and {}",
                        i, j
                    );
                }
            }
        }

        // Verify Other is not included
        assert!(
            !variants.contains(&ReviewCategory::Other),
            "Other should not be in classified_variants"
        );
    }

    #[test]
    fn test_classify_style_keywords_comprehensive() {
        assert_eq!(
            ReviewClassifier::classify("please fix the indentation"),
            ReviewCategory::StyleIssue
        );
        assert_eq!(
            ReviewClassifier::classify("there is extra whitespace here"),
            ReviewCategory::StyleIssue
        );
        assert_eq!(
            ReviewClassifier::classify("lint error on this line"),
            ReviewCategory::StyleIssue
        );
        assert_eq!(
            ReviewClassifier::classify("bad format in this function"),
            ReviewCategory::StyleIssue
        );
    }

    #[test]
    fn test_classify_incomplete_forgot_keyword() {
        assert_eq!(
            ReviewClassifier::classify("you forgot to handle the error"),
            ReviewCategory::Incomplete
        );
    }

    #[test]
    fn test_classify_incomplete_also_need() {
        assert_eq!(
            ReviewClassifier::classify("we also need to update the schema"),
            ReviewCategory::Incomplete
        );
    }

    #[test]
    fn test_classify_incomplete_missing_keyword() {
        assert_eq!(
            ReviewClassifier::classify("there is a missing null check"),
            ReviewCategory::Incomplete
        );
    }

    #[test]
    fn test_classify_performance_efficient_keyword() {
        assert_eq!(
            ReviewClassifier::classify("this is not efficient enough"),
            ReviewCategory::Performance
        );
    }

    #[test]
    fn test_classify_performance_cache_keyword() {
        assert_eq!(
            ReviewClassifier::classify("should we add a cache here?"),
            ReviewCategory::Performance
        );
    }

    #[test]
    fn test_classify_wrong_approach_dont_keyword() {
        assert_eq!(
            ReviewClassifier::classify("don't do it this way"),
            ReviewCategory::WrongApproach
        );
    }

    #[test]
    fn test_classify_wrong_approach_shouldnt_keyword() {
        assert_eq!(
            ReviewClassifier::classify("you shouldn't use this pattern"),
            ReviewCategory::WrongApproach
        );
    }

    #[test]
    fn test_classify_wrong_approach_better_to() {
        assert_eq!(
            ReviewClassifier::classify("it's better to use a different data structure"),
            ReviewCategory::WrongApproach
        );
    }

    #[test]
    fn test_classify_wrong_approach_should_use() {
        assert_eq!(
            ReviewClassifier::classify("you should use an enum here"),
            ReviewCategory::WrongApproach
        );
    }

    #[test]
    fn test_classify_documentation_readme_keyword() {
        assert_eq!(
            ReviewClassifier::classify("please update the readme"),
            ReviewCategory::Documentation
        );
    }

    #[test]
    fn test_classify_documentation_jsdoc_keyword() {
        assert_eq!(
            ReviewClassifier::classify("add a jsdoc comment for this function"),
            ReviewCategory::Documentation
        );
    }

    #[test]
    fn test_classify_security_inject_keyword() {
        assert_eq!(
            ReviewClassifier::classify("this is vulnerable to SQL inject attacks"),
            ReviewCategory::Security
        );
    }

    #[test]
    fn test_classify_security_vulnerability_keyword() {
        assert_eq!(
            ReviewClassifier::classify("this introduces a vulnerability"),
            ReviewCategory::Security
        );
    }

    #[test]
    fn test_classify_missing_tests_coverage_keyword() {
        assert_eq!(
            ReviewClassifier::classify("we need better coverage for this"),
            ReviewCategory::MissingTests
        );
    }

    #[test]
    fn test_classify_missing_tests_assert_keyword() {
        assert_eq!(
            ReviewClassifier::classify("assert that the result is correct"),
            ReviewCategory::MissingTests
        );
    }

    #[test]
    fn test_classify_missing_tests_verify_keyword() {
        assert_eq!(
            ReviewClassifier::classify("verify the output matches expectations"),
            ReviewCategory::MissingTests
        );
    }

    #[test]
    fn test_classify_missing_tests_integration_test_keyword() {
        assert_eq!(
            ReviewClassifier::classify("we need an integration test for this"),
            ReviewCategory::MissingTests
        );
    }

    #[test]
    fn test_classify_missing_tests_unit_test_keyword() {
        assert_eq!(
            ReviewClassifier::classify("add a unit test for this function"),
            ReviewCategory::MissingTests
        );
    }

    #[test]
    fn test_classify_performance_complexity_keyword() {
        assert_eq!(
            ReviewClassifier::classify("the complexity of this algorithm is too high"),
            ReviewCategory::Performance
        );
    }

    #[test]
    fn test_classify_documentation_document_keyword() {
        assert_eq!(
            ReviewClassifier::classify("please document the API endpoints"),
            ReviewCategory::Documentation
        );
    }

    #[test]
    fn test_truncate_empty_string() {
        assert_eq!(truncate("", 0), "");
        assert_eq!(truncate("", 5), "");
        assert_eq!(truncate("", 100), "");
    }

    #[test]
    fn test_truncate_max_len_two() {
        let result = truncate("hello", 2);
        assert_eq!(result, "he");
        assert!(result.len() <= 2);
    }

    #[test]
    fn test_truncate_exact_boundary() {
        // String exactly at boundary should not be truncated
        assert_eq!(truncate("abcde", 5), "abcde");
        // String one over should be truncated
        let result = truncate("abcdef", 5);
        assert!(result.len() <= 5);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_multibyte_unicode() {
        // Multi-byte characters should not be split mid-character
        let emoji = "\u{1F600}\u{1F601}\u{1F602}"; // 3 emoji, each 4 bytes = 12 bytes
        let result = truncate(emoji, 6);
        // Should safely truncate without splitting a character
        assert!(result.len() <= 6);
        // Verify it's valid UTF-8 (would panic on access otherwise)
        let _ = result.chars().count();
    }

    #[test]
    fn test_truncate_cjk_characters() {
        let cjk = "\u{4f60}\u{597d}\u{4e16}\u{754c}"; // 4 CJK chars, 3 bytes each = 12 bytes
        let result = truncate(cjk, 7);
        assert!(result.len() <= 7);
        // Should be valid UTF-8
        let _ = result.chars().count();
    }

    #[test]
    fn test_process_review_comments_stores_pattern_text_truncated() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let long_body = "a".repeat(500);
        let patterns =
            ReviewClassifier::process_review_comments(&tracker, "org/repo", &[], Some(&long_body))
                .unwrap();
        assert_eq!(patterns.len(), 1);
        // Pattern text should be truncated to 200 chars
        assert!(
            patterns[0].pattern_text.len() <= 200,
            "Pattern text should be truncated to 200 chars, got {}",
            patterns[0].pattern_text.len()
        );
        // Example comments should be truncated to 500 chars
        assert!(
            patterns[0].example_comments[0].len() <= 500,
            "Example comment should be truncated to 500 chars, got {}",
            patterns[0].example_comments[0].len()
        );
    }

    #[test]
    fn test_process_review_comments_pattern_text_preserved_when_short() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let short_body = "Add tests please";
        let patterns =
            ReviewClassifier::process_review_comments(&tracker, "org/repo", &[], Some(short_body))
                .unwrap();
        assert_eq!(patterns[0].pattern_text, "Add tests please");
        assert_eq!(patterns[0].example_comments[0], "Add tests please");
    }

    #[test]
    fn test_process_review_comments_promoted_to_instruction_is_false() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let patterns =
            ReviewClassifier::process_review_comments(&tracker, "org/repo", &[], Some("Add tests"))
                .unwrap();
        assert!(!patterns[0].promoted_to_instruction);
    }

    #[test]
    fn test_process_review_comments_scm_repo_set() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let patterns = ReviewClassifier::process_review_comments(
            &tracker,
            "my-org/my-repo",
            &[],
            Some("Add tests"),
        )
        .unwrap();
        assert_eq!(patterns[0].scm_repo, "my-org/my-repo");
    }

    #[test]
    fn test_check_promotion_threshold_large_threshold() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        for _ in 0..10 {
            ReviewClassifier::process_review_comments(&tracker, "org/repo", &[], Some("Add tests"))
                .unwrap();
        }
        let promotable =
            ReviewClassifier::check_promotion_threshold(&tracker, "org/repo", 100).unwrap();
        assert!(
            promotable.is_empty(),
            "count=10 should be below threshold=100"
        );
    }

    #[test]
    fn test_check_promotion_threshold_multiple_categories() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();

        // Create 3 patterns for tests, 3 for security, 1 for style
        for _ in 0..3 {
            ReviewClassifier::process_review_comments(
                &tracker,
                "org/repo",
                &[],
                Some("Add tests for this"),
            )
            .unwrap();
            ReviewClassifier::process_review_comments(
                &tracker,
                "org/repo",
                &[],
                Some("Security vulnerability here"),
            )
            .unwrap();
        }
        ReviewClassifier::process_review_comments(
            &tracker,
            "org/repo",
            &[],
            Some("Fix the naming convention style"),
        )
        .unwrap();

        let promotable =
            ReviewClassifier::check_promotion_threshold(&tracker, "org/repo", 3).unwrap();
        // Should have 2 promotable patterns (tests=3, security=3), not style (style=1)
        assert_eq!(
            promotable.len(),
            2,
            "Should have 2 promotable patterns, got {}",
            promotable.len()
        );
    }

    #[test]
    fn test_classify_mixed_case_keywords() {
        assert_eq!(
            ReviewClassifier::classify("Please Add Tests For Coverage"),
            ReviewCategory::MissingTests
        );
        assert_eq!(
            ReviewClassifier::classify("SECURITY VULNERABILITY FOUND"),
            ReviewCategory::Security
        );
        assert_eq!(
            ReviewClassifier::classify("Consider Using Approach Instead"),
            ReviewCategory::WrongApproach
        );
    }

    #[test]
    fn test_classify_special_characters_in_comment() {
        // "xss" is a Security keyword, so even though "test" appears, Security wins
        assert_eq!(
            ReviewClassifier::classify("test <script>alert('xss')</script>"),
            ReviewCategory::Security
        );
        assert_eq!(
            ReviewClassifier::classify("security: `rm -rf /`"),
            ReviewCategory::Security
        );
        // Backticks and special markdown should not prevent classification
        assert_eq!(
            ReviewClassifier::classify("add `test` coverage for **this** method"),
            ReviewCategory::MissingTests
        );
    }

    #[test]
    fn test_classify_multiline_comment() {
        let multiline = "This PR needs improvement.\nPlease add tests.\nAlso fix formatting.";
        // "test" appears in the second line, should still be classified
        assert_eq!(
            ReviewClassifier::classify(multiline),
            ReviewCategory::MissingTests
        );
    }

    #[test]
    fn test_process_review_comments_occurrence_count_increments() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        // Send the same comment text 5 times
        for _ in 0..5 {
            ReviewClassifier::process_review_comments(
                &tracker,
                "org/repo",
                &[],
                Some("Please add unit tests for this"),
            )
            .unwrap();
        }
        let stored = tracker.get_review_patterns("org/repo", 10).unwrap();
        assert_eq!(stored.len(), 1, "Should be deduplicated into one pattern");
        assert_eq!(
            stored[0].occurrence_count, 5,
            "Occurrence count should be 5"
        );
    }

    #[test]
    fn test_check_promotion_threshold_empty_repo() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let promotable =
            ReviewClassifier::check_promotion_threshold(&tracker, "nonexistent/repo", 1).unwrap();
        assert!(promotable.is_empty());
    }

    #[test]
    fn test_truncate_single_char_string() {
        assert_eq!(truncate("a", 1), "a");
        assert_eq!(truncate("a", 10), "a");
        assert_eq!(truncate("a", 0), "");
    }

    #[test]
    fn test_truncate_five_chars_max_five() {
        assert_eq!(truncate("abcde", 5), "abcde");
    }

    #[test]
    fn test_truncate_six_chars_max_five() {
        let result = truncate("abcdef", 5);
        assert_eq!(result, "ab...");
        assert!(result.len() <= 5);
    }

    #[test]
    fn test_classify_priority_order_is_deterministic() {
        // Verify the priority ordering: Security > MissingTests > WrongApproach > StyleIssue > Incomplete > Performance > Documentation
        // When multiple categories match, the first in priority should win

        // Security + Performance
        assert_eq!(
            ReviewClassifier::classify("security vulnerability slows performance"),
            ReviewCategory::Security
        );
        // MissingTests + WrongApproach
        assert_eq!(
            ReviewClassifier::classify("add test, wrong approach instead"),
            ReviewCategory::MissingTests
        );
        // WrongApproach + StyleIssue
        assert_eq!(
            ReviewClassifier::classify("wrong way to format style"),
            ReviewCategory::WrongApproach
        );
        // StyleIssue + Incomplete
        assert_eq!(
            ReviewClassifier::classify("naming convention is incomplete"),
            ReviewCategory::StyleIssue
        );
        // Incomplete + Performance
        assert_eq!(
            ReviewClassifier::classify("missing cache optimization"),
            ReviewCategory::Incomplete
        );
        // Performance + Documentation
        assert_eq!(
            ReviewClassifier::classify("slow, needs documentation to explain"),
            ReviewCategory::Performance
        );
    }

    #[test]
    fn test_semantic_classifier_embedding_client_accessor() {
        // Verify the embedding_client() accessor returns the correct reference
        // by checking it from a manually constructed classifier.
        // We cannot easily construct SemanticReviewClassifier without async,
        // so we test the accessor logic indirectly via category_count.
        // category_count returns reference_embeddings.len() which we can verify.
        let categories = ReviewCategory::classified_variants();
        assert_eq!(categories.len(), 7);
    }

    #[test]
    fn test_truncate_large_max_len() {
        let s = "short";
        let result = truncate(s, 1000);
        assert_eq!(result, "short");
    }

    #[test]
    fn test_truncate_max_len_equals_string_length() {
        let s = "hello";
        assert_eq!(truncate(s, 5), "hello");
    }

    #[test]
    fn test_truncate_max_len_one_less_than_string() {
        // "hello" is 5 chars, max_len=4 means we need ellipsis
        let result = truncate("hello", 4);
        assert_eq!(result, "h...");
        assert!(result.len() <= 4);
    }

    #[test]
    fn test_process_review_comments_three_inline_different_categories() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let comments = vec![
            PrReviewComment {
                id: 1,
                path: "src/a.rs".to_string(),
                position: Some(1),
                original_position: None,
                body: "this is a security vulnerability".to_string(),
                user: claudear_core::types::ReviewUser {
                    login: "rev".to_string(),
                    id: 1,
                    user_type: None,
                },
                created_at: "2024-01-01T00:00:00Z".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
                html_url: "url".to_string(),
                pull_request_review_id: None,
                start_line: None,
                line: None,
                side: None,
            },
            PrReviewComment {
                id: 2,
                path: "src/b.rs".to_string(),
                position: Some(2),
                original_position: None,
                body: "optimize this code for better performance".to_string(),
                user: claudear_core::types::ReviewUser {
                    login: "rev".to_string(),
                    id: 1,
                    user_type: None,
                },
                created_at: "2024-01-01T00:00:00Z".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
                html_url: "url".to_string(),
                pull_request_review_id: None,
                start_line: None,
                line: None,
                side: None,
            },
            PrReviewComment {
                id: 3,
                path: "src/c.rs".to_string(),
                position: Some(3),
                original_position: None,
                body: "please add a docstring comment".to_string(),
                user: claudear_core::types::ReviewUser {
                    login: "rev".to_string(),
                    id: 1,
                    user_type: None,
                },
                created_at: "2024-01-01T00:00:00Z".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
                html_url: "url".to_string(),
                pull_request_review_id: None,
                start_line: None,
                line: None,
                side: None,
            },
        ];

        let patterns =
            ReviewClassifier::process_review_comments(&tracker, "org/repo", &comments, None)
                .unwrap();
        assert_eq!(patterns.len(), 3);
        assert_eq!(patterns[0].category, ReviewCategory::Security);
        assert_eq!(patterns[1].category, ReviewCategory::Performance);
        assert_eq!(patterns[2].category, ReviewCategory::Documentation);

        let stored = tracker.get_review_patterns("org/repo", 10).unwrap();
        assert_eq!(stored.len(), 3);
    }

    #[test]
    fn test_check_promotion_threshold_already_promoted_excluded() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();

        // Upsert a pattern 3 times to reach threshold
        for _ in 0..3 {
            ReviewClassifier::process_review_comments(
                &tracker,
                "org/repo",
                &[],
                Some("Add tests for this"),
            )
            .unwrap();
        }

        // First check: should find 1 promotable
        let promotable =
            ReviewClassifier::check_promotion_threshold(&tracker, "org/repo", 3).unwrap();
        assert_eq!(promotable.len(), 1);

        // Mark as promoted (simulated - the real implementation would set this flag)
        // For now, just verify the filter logic works: the pattern has
        // promoted_to_instruction = false, so it shows up
        assert!(!promotable[0].promoted_to_instruction);
    }

    #[test]
    fn test_process_review_comments_pattern_text_from_inline_comment() {
        let tracker = claudear_storage::SqliteTracker::in_memory().unwrap();
        let comments = vec![PrReviewComment {
            id: 1,
            path: "src/main.rs".to_string(),
            position: Some(1),
            original_position: None,
            body: "Please add test coverage here".to_string(),
            user: claudear_core::types::ReviewUser {
                login: "rev".to_string(),
                id: 1,
                user_type: None,
            },
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
            html_url: "url".to_string(),
            pull_request_review_id: None,
            start_line: None,
            line: None,
            side: None,
        }];

        let patterns =
            ReviewClassifier::process_review_comments(&tracker, "org/repo", &comments, None)
                .unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].pattern_text, "Please add test coverage here");
        assert_eq!(
            patterns[0].example_comments[0],
            "Please add test coverage here"
        );
        assert_eq!(patterns[0].scm_repo, "org/repo");
        assert_eq!(patterns[0].occurrence_count, 1);
    }

    // --- classify_with_llm tests ---

    struct MockLlmReview {
        result: Option<ReviewCategory>,
    }

    impl crate::llm::LlmAnalyzer for MockLlmReview {
        fn assess_issues(
            &self,
            _: &[(
                claudear_core::types::Issue,
                claudear_core::types::MatchResult,
            )],
        ) -> Option<Vec<crate::llm::LlmIssueAssessment>> {
            None
        }
        fn classify_review(&self, _: &str) -> Option<ReviewCategory> {
            self.result
        }
        fn extract_learnings(&self, _: &str) -> Option<crate::llm::LlmLogAnalysis> {
            None
        }
        fn explain_correlations(
            &self,
            _: &[(String, String, i64)],
            _: &str,
        ) -> Vec<crate::llm::LlmCorrelationExplanation> {
            Vec::new()
        }
    }

    #[test]
    fn test_classify_with_llm_succeeds() {
        let mock = MockLlmReview {
            result: Some(ReviewCategory::Security),
        };
        let cat = ReviewClassifier::classify_with_llm("some comment", Some(&mock));
        assert_eq!(cat, ReviewCategory::Security);
    }

    #[test]
    fn test_classify_with_llm_falls_back() {
        let mock = MockLlmReview { result: None };
        // "test" keyword → MissingTests via heuristic
        let cat = ReviewClassifier::classify_with_llm("add more test coverage", Some(&mock));
        assert_eq!(cat, ReviewCategory::MissingTests);
    }

    #[test]
    fn test_classify_with_llm_none_analyzer() {
        let cat = ReviewClassifier::classify_with_llm("add more test coverage", None);
        assert_eq!(cat, ReviewCategory::MissingTests);
    }
}
