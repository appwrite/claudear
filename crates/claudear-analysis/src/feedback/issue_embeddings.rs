//! Issue embedding service for semantic similarity search.
//!
//! Provides functionality to embed issues and find similar past issues
//! to improve Claude's context when processing new issues.

use crate::feedback::EmbeddingClient;
use chrono::Utc;
use claudear_core::error::Result;
use claudear_core::types::{Issue, IssueEmbedding, SimilarIssue};
use claudear_storage::FixAttemptTracker;
use std::sync::Arc;

/// Configuration for issue embedding.
#[derive(Debug, Clone)]
pub struct IssueEmbeddingConfig {
    /// Whether embeddings are enabled.
    pub enabled: bool,
    /// Minimum similarity score to consider a match (0.0-1.0).
    pub min_similarity: f64,
    /// Maximum number of similar issues to return.
    pub max_similar_issues: usize,
    /// Similarity threshold above which a new issue is considered a semantic duplicate (0.0-1.0).
    pub skip_similarity_threshold: f64,
}

impl Default for IssueEmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_similarity: 0.7,
            max_similar_issues: 5,
            skip_similarity_threshold: 0.90,
        }
    }
}

/// A similar issue with its details and similarity score.
#[derive(Debug, Clone)]
pub struct SimilarIssueWithDetails {
    /// The original issue embedding.
    pub embedding: IssueEmbedding,
    /// Similarity score (0.0-1.0).
    pub similarity: f64,
    /// Outcome of the fix attempt (if known).
    pub outcome: Option<String>,
    /// PR URL (if created).
    pub pr_url: Option<String>,
}

/// Service for managing issue embeddings and finding similar issues.
pub struct IssueEmbeddingService {
    embedding_client: Arc<EmbeddingClient>,
    tracker: Arc<dyn FixAttemptTracker>,
    config: IssueEmbeddingConfig,
}

impl IssueEmbeddingService {
    /// Create a new issue embedding service.
    pub fn new(
        embedding_client: Arc<EmbeddingClient>,
        tracker: Arc<dyn FixAttemptTracker>,
        config: IssueEmbeddingConfig,
    ) -> Self {
        Self {
            embedding_client,
            tracker,
            config,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults(
        embedding_client: Arc<EmbeddingClient>,
        tracker: Arc<dyn FixAttemptTracker>,
    ) -> Self {
        Self::new(embedding_client, tracker, IssueEmbeddingConfig::default())
    }

    /// Check if embedding service is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Embed a single issue and store it in the database.
    pub async fn embed_issue(&self, issue: &Issue, source: &str) -> Result<IssueEmbedding> {
        // Build text to embed from issue content
        let text = build_embedding_text(issue);

        // Generate embedding
        let embedding_vec = self.embedding_client.embed(&text).await?;

        // Create embedding record with full issue content
        let mut embedding = IssueEmbedding::new(source, &issue.id, embedding_vec);
        embedding.short_id = Some(issue.short_id.clone());
        embedding.title = Some(issue.title.clone());
        embedding.description = issue.description.clone();
        embedding.url = Some(issue.url.clone());
        embedding.priority = Some(issue.priority.to_string());
        embedding.status = Some(issue.status.to_string());
        embedding.updated_at = issue.updated_at;
        let labels: Vec<String> = issue.get_metadata("labels").unwrap_or_default();
        if !labels.is_empty() {
            embedding.labels = serde_json::to_string(&labels).ok();
        }
        embedding.embedding_model = Some(self.embedding_client.model().to_string());
        embedding.created_at = Utc::now();

        // Store in database
        self.tracker.store_embedding(&embedding)?;

        tracing::debug!(
            source = source,
            issue_id = %issue.id,
            short_id = %issue.short_id,
            "Stored issue embedding"
        );

        Ok(embedding)
    }

    /// Embed multiple issues efficiently.
    ///
    /// Uses a single database transaction for all inserts instead of
    /// acquiring the mutex lock once per embedding.
    pub async fn embed_batch(&self, issues: &[Issue], source: &str) -> Result<Vec<IssueEmbedding>> {
        if issues.is_empty() {
            return Ok(Vec::new());
        }

        // Build texts for all issues
        let texts: Vec<String> = issues.iter().map(build_embedding_text).collect();
        let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();

        // Generate embeddings in batch
        let embeddings_vecs = self.embedding_client.embed_batch(&text_refs).await?;

        // Create embedding records
        let model_name = self.embedding_client.model().to_string();
        let now = Utc::now();
        let mut results = Vec::with_capacity(issues.len());
        for (issue, embedding_vec) in issues.iter().zip(embeddings_vecs) {
            let mut embedding = IssueEmbedding::new(source, &issue.id, embedding_vec);
            embedding.short_id = Some(issue.short_id.clone());
            embedding.title = Some(issue.title.clone());
            embedding.description = issue.description.clone();
            embedding.url = Some(issue.url.clone());
            embedding.priority = Some(issue.priority.to_string());
            embedding.status = Some(issue.status.to_string());
            embedding.updated_at = issue.updated_at;
            let labels: Vec<String> = issue.get_metadata("labels").unwrap_or_default();
            if !labels.is_empty() {
                embedding.labels = serde_json::to_string(&labels).ok();
            }
            embedding.embedding_model = Some(model_name.clone());
            embedding.created_at = now;
            results.push(embedding);
        }

        // Store all embeddings in a single transaction
        self.tracker.store_embeddings_batch(&results)?;

        tracing::info!(
            source = source,
            count = results.len(),
            "Stored batch of issue embeddings"
        );

        Ok(results)
    }

    /// Find similar issues for a given issue using HNSW vector search.
    ///
    /// Returns empty if vectorlite is unavailable or no results found.
    pub async fn find_similar(
        &self,
        issue: &Issue,
        source: &str,
    ) -> Result<Vec<SimilarIssueWithDetails>> {
        if !self.config.enabled {
            return Ok(Vec::new());
        }

        // Generate embedding for the query issue
        let text = build_embedding_text(issue);
        let query_embedding = self.embedding_client.embed(&text).await?;

        // HNSW vector search via vectorlite
        let top_similar = match self.tracker.find_similar_issues_vector(
            &query_embedding,
            source,
            Some(&issue.id),
            self.config.min_similarity,
            self.config.max_similar_issues,
        )? {
            Some(results) => {
                tracing::debug!(
                    count = results.len(),
                    "Issue similarity search used HNSW index"
                );
                results
            }
            None => {
                tracing::warn!("vectorlite unavailable for issue similarity search");
                return Ok(Vec::new());
            }
        };

        // Enrich with fix attempt details (batch query to avoid N+1)
        let keys: Vec<(&str, &str)> = top_similar
            .iter()
            .map(|(emb, _)| (emb.source.as_str(), emb.issue_id.as_str()))
            .collect();
        let attempts = self.tracker.get_attempts_batch(&keys).unwrap_or_default();

        let mut results = Vec::with_capacity(top_similar.len());
        for ((embedding, similarity), attempt) in top_similar.into_iter().zip(attempts) {
            let (outcome, pr_url) = match attempt {
                Some(a) => (Some(a.status.to_string()), a.pr_url),
                None => (None, None),
            };

            results.push(SimilarIssueWithDetails {
                embedding,
                similarity,
                outcome,
                pr_url,
            });
        }

        // Store similar issue relationships in a single transaction (batch insert)
        let now = Utc::now();
        let similar_batch: Vec<SimilarIssue> = results
            .iter()
            .map(|result| SimilarIssue {
                id: 0,
                source_issue_id: issue.id.clone(),
                similar_issue_id: result.embedding.issue_id.clone(),
                similarity_score: result.similarity,
                computed_at: now,
            })
            .collect();
        if let Err(e) = self.tracker.store_similar_issues_batch(&similar_batch) {
            tracing::warn!(error = %e, "Failed to store similar issue relationships");
        }

        tracing::debug!(
            source = source,
            issue_id = %issue.id,
            similar_count = results.len(),
            "Found similar issues"
        );

        Ok(results)
    }

    /// Check if an issue is a semantic duplicate of one already being processed or resolved.
    ///
    /// Returns `Some(duplicate)` if the top similar issue has similarity ≥ `skip_similarity_threshold`
    /// AND the similar issue's outcome is pending, success, or merged.
    /// Returns `None` if no duplicate is found or the similar issue failed.
    pub async fn check_duplicate(
        &self,
        issue: &Issue,
        source: &str,
    ) -> Result<Option<SimilarIssueWithDetails>> {
        if !self.config.enabled {
            return Ok(None);
        }

        let similar = self.find_similar(issue, source).await?;
        if let Some(top) = similar.into_iter().next() {
            if top.similarity >= self.config.skip_similarity_threshold {
                // Only skip if the similar issue is actively being handled
                let dominated_by_active = matches!(
                    top.outcome.as_deref(),
                    Some("pending") | Some("success") | Some("merged")
                );
                if dominated_by_active {
                    return Ok(Some(top));
                }
            }
        }
        Ok(None)
    }

    /// Get an existing embedding for an issue.
    pub fn get_embedding(&self, source: &str, issue_id: &str) -> Result<Option<IssueEmbedding>> {
        self.tracker.get_embedding(source, issue_id)
    }

    /// Check if an issue already has an embedding.
    pub fn has_embedding(&self, source: &str, issue_id: &str) -> bool {
        self.get_embedding(source, issue_id)
            .map(|e| e.is_some())
            .unwrap_or(false)
    }
}

/// Build text content for embedding from an issue.
///
/// Uses direct string building with push_str to avoid intermediate allocations.
fn build_embedding_text(issue: &Issue) -> String {
    // Estimate capacity: title + description + some overhead for labels/stack
    let est_cap = issue.title.len() + issue.description.as_ref().map_or(0, |d| d.len() + 2) + 256;
    let mut text = String::with_capacity(est_cap);

    // Title is most important
    text.push_str(&issue.title);

    // Description if available
    if let Some(ref desc) = issue.description {
        text.push_str("\n\n");
        text.push_str(desc);
    }

    // Add labels from metadata if available
    if let Some(labels) = issue.metadata.get("labels") {
        if let Some(labels_arr) = labels.as_array() {
            let label_strs: Vec<&str> = labels_arr.iter().filter_map(|l| l.as_str()).collect();
            if !label_strs.is_empty() {
                text.push_str("\n\nLabels: ");
                text.push_str(&label_strs.join(", "));
            }
        }
    }

    // Stack trace from metadata if available (very important for bug similarity)
    if let Some(stack) = issue.metadata.get("stack_trace").and_then(|v| v.as_str()) {
        text.push_str("\n\n");
        if stack.len() > 2000 {
            let truncate_pos = stack
                .char_indices()
                .take_while(|(i, _)| *i < 2000)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            text.push_str(&stack[..truncate_pos]);
            text.push_str("...");
        } else {
            text.push_str(stack);
        }
    }

    // Also check for error message in metadata
    if let Some(error) = issue.metadata.get("error_message").and_then(|v| v.as_str()) {
        text.push_str("\n\n");
        text.push_str(error);
    }

    text
}

/// Format similar issues as context for Claude.
///
/// Uses `std::fmt::Write` to write directly into the String, avoiding
/// intermediate format! allocations for each field.
pub fn format_similar_issues_context(similar: &[SimilarIssueWithDetails]) -> String {
    use std::fmt::Write;

    if similar.is_empty() {
        return String::new();
    }

    let mut context = String::from("\n\n## Similar Past Issues\n\n");
    context.push_str("The following similar issues have been processed before. ");
    context.push_str("Use this context to inform your approach:\n\n");

    for (i, sim) in similar.iter().enumerate() {
        let _ = writeln!(
            context,
            "### {}. {} (Similarity: {:.0}%)",
            i + 1,
            sim.embedding
                .short_id
                .as_deref()
                .unwrap_or(&sim.embedding.issue_id),
            sim.similarity * 100.0
        );

        if let Some(ref title) = sim.embedding.title {
            let _ = writeln!(context, "**Title:** {}", title);
        }

        if let Some(ref outcome) = sim.outcome {
            let _ = writeln!(context, "**Outcome:** {}", outcome);
        }

        if let Some(ref pr_url) = sim.pr_url {
            let _ = writeln!(context, "**PR:** {}", pr_url);
        }

        context.push('\n');
    }

    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_core::types::{IssuePriority, IssueStatus};
    use std::collections::HashMap;

    #[test]
    fn test_build_embedding_text() {
        let mut metadata = HashMap::new();
        metadata.insert("labels".to_string(), serde_json::json!(["bug", "auth"]));
        metadata.insert(
            "stack_trace".to_string(),
            serde_json::json!("Error at auth.rs:42"),
        );

        let issue = Issue {
            id: "123".to_string(),
            short_id: "PROJ-123".to_string(),
            title: "Fix the login bug".to_string(),
            description: Some("Users can't log in".to_string()),
            url: "https://example.com/issue/123".to_string(),
            source: "linear".to_string(),
            priority: IssuePriority::High,
            status: IssueStatus::Open,
            metadata,
            created_at: None,
            updated_at: None,
        };

        let text = build_embedding_text(&issue);
        assert!(text.contains("Fix the login bug"));
        assert!(text.contains("Users can't log in"));
        assert!(text.contains("Labels: bug, auth"));
        assert!(text.contains("Error at auth.rs:42"));
    }

    #[test]
    fn test_format_similar_issues_context() {
        let similar = vec![SimilarIssueWithDetails {
            embedding: IssueEmbedding::new("linear", "456", vec![0.1, 0.2]),
            similarity: 0.85,
            outcome: Some("merged".to_string()),
            pr_url: Some("https://github.com/org/repo/pull/42".to_string()),
        }];

        let context = format_similar_issues_context(&similar);
        assert!(context.contains("Similar Past Issues"));
        assert!(context.contains("85%"));
        assert!(context.contains("merged"));
        assert!(context.contains("pull/42"));
    }

    fn make_issue(title: &str) -> Issue {
        Issue {
            id: "123".to_string(),
            short_id: "PROJ-123".to_string(),
            title: title.to_string(),
            description: None,
            url: "https://example.com".to_string(),
            source: "linear".to_string(),
            priority: IssuePriority::High,
            status: IssueStatus::Open,
            metadata: HashMap::new(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn test_build_embedding_text_title_only() {
        let issue = make_issue("Title only issue");
        let text = build_embedding_text(&issue);
        assert_eq!(text, "Title only issue");
    }

    #[test]
    fn test_build_embedding_text_with_description_no_metadata() {
        let mut issue = make_issue("Bug report");
        issue.description = Some("Detailed description here".to_string());
        let text = build_embedding_text(&issue);
        assert_eq!(text, "Bug report\n\nDetailed description here");
    }

    #[test]
    fn test_build_embedding_text_empty_description() {
        let mut issue = make_issue("Empty desc");
        issue.description = Some("".to_string());
        let text = build_embedding_text(&issue);
        // Should include the "\n\n" separator even though description is empty
        assert_eq!(text, "Empty desc\n\n");
    }

    #[test]
    fn test_build_embedding_text_with_labels() {
        let mut issue = make_issue("Labeled issue");
        issue.metadata.insert(
            "labels".to_string(),
            serde_json::json!(["bug", "critical", "auth"]),
        );
        let text = build_embedding_text(&issue);
        assert!(text.contains("Labels: bug, critical, auth"));
    }

    #[test]
    fn test_build_embedding_text_empty_labels_array() {
        let mut issue = make_issue("No labels");
        issue
            .metadata
            .insert("labels".to_string(), serde_json::json!([]));
        let text = build_embedding_text(&issue);
        // Empty array should NOT produce a "Labels:" line
        assert!(!text.contains("Labels:"));
        assert_eq!(text, "No labels");
    }

    #[test]
    fn test_build_embedding_text_long_stack_trace_truncated() {
        let mut issue = make_issue("Stack overflow");
        let long_stack = "x".repeat(3000);
        issue
            .metadata
            .insert("stack_trace".to_string(), serde_json::json!(long_stack));
        let text = build_embedding_text(&issue);
        // The stack portion should be truncated to 2000 chars + "..."
        assert!(text.ends_with("..."));
        // Original 3000-char stack should not appear in full
        assert!(!text.contains(&"x".repeat(3000)));
        // But 2000 chars of it should be present
        assert!(text.contains(&"x".repeat(2000)));
    }

    #[test]
    fn test_build_embedding_text_stack_trace_exactly_2000_not_truncated() {
        let mut issue = make_issue("Exact stack");
        let exact_stack = "y".repeat(2000);
        issue
            .metadata
            .insert("stack_trace".to_string(), serde_json::json!(exact_stack));
        let text = build_embedding_text(&issue);
        // Exactly 2000 chars should NOT be truncated
        assert!(!text.ends_with("..."));
        assert!(text.contains(&"y".repeat(2000)));
    }

    #[test]
    fn test_build_embedding_text_error_message() {
        let mut issue = make_issue("Error issue");
        issue.metadata.insert(
            "error_message".to_string(),
            serde_json::json!("NullPointerException at line 99"),
        );
        let text = build_embedding_text(&issue);
        assert!(text.contains("NullPointerException at line 99"));
        assert_eq!(text, "Error issue\n\nNullPointerException at line 99");
    }

    #[test]
    fn test_build_embedding_text_all_metadata_fields() {
        let mut issue = make_issue("Full metadata");
        issue.description = Some("A comprehensive bug".to_string());
        issue
            .metadata
            .insert("labels".to_string(), serde_json::json!(["bug", "p0"]));
        issue.metadata.insert(
            "stack_trace".to_string(),
            serde_json::json!("panic at core.rs:10"),
        );
        issue.metadata.insert(
            "error_message".to_string(),
            serde_json::json!("thread 'main' panicked"),
        );

        let text = build_embedding_text(&issue);
        assert!(text.contains("Full metadata"));
        assert!(text.contains("A comprehensive bug"));
        assert!(text.contains("Labels: bug, p0"));
        assert!(text.contains("panic at core.rs:10"));
        assert!(text.contains("thread 'main' panicked"));
    }

    #[test]
    fn test_build_embedding_text_non_array_labels_skipped() {
        let mut issue = make_issue("String labels");
        // labels is a plain string, not an array
        issue
            .metadata
            .insert("labels".to_string(), serde_json::json!("bug"));
        let text = build_embedding_text(&issue);
        // Should not contain Labels: because as_array() returns None for a string
        assert!(!text.contains("Labels:"));
        assert_eq!(text, "String labels");
    }

    #[test]
    fn test_build_embedding_text_non_string_stack_trace_skipped() {
        let mut issue = make_issue("Numeric stack");
        // stack_trace is numeric, not a string
        issue
            .metadata
            .insert("stack_trace".to_string(), serde_json::json!(42));
        let text = build_embedding_text(&issue);
        // Should not include the numeric value because as_str() returns None
        assert_eq!(text, "Numeric stack");
    }

    #[test]
    fn test_format_similar_issues_context_empty() {
        let context = format_similar_issues_context(&[]);
        assert_eq!(context, "");
    }

    #[test]
    fn test_format_similar_issues_context_single_all_fields() {
        let mut emb = IssueEmbedding::new("linear", "456", vec![0.1, 0.2]);
        emb.short_id = Some("PROJ-456".to_string());
        emb.title = Some("Previous auth bug".to_string());

        let similar = vec![SimilarIssueWithDetails {
            embedding: emb,
            similarity: 0.92,
            outcome: Some("success".to_string()),
            pr_url: Some("https://github.com/org/repo/pull/99".to_string()),
        }];

        let context = format_similar_issues_context(&similar);
        assert!(context.contains("PROJ-456"));
        assert!(context.contains("92%"));
        assert!(context.contains("**Title:** Previous auth bug"));
        assert!(context.contains("**Outcome:** success"));
        assert!(context.contains("**PR:** https://github.com/org/repo/pull/99"));
    }

    #[test]
    fn test_format_similar_issues_context_no_optional_fields() {
        // No title, no outcome, no pr_url on the embedding
        let emb = IssueEmbedding::new("sentry", "789", vec![0.3]);

        let similar = vec![SimilarIssueWithDetails {
            embedding: emb,
            similarity: 0.75,
            outcome: None,
            pr_url: None,
        }];

        let context = format_similar_issues_context(&similar);
        // Should still render without panicking
        assert!(context.contains("789")); // falls back to issue_id
        assert!(context.contains("75%"));
        assert!(!context.contains("**Title:**"));
        assert!(!context.contains("**Outcome:**"));
        assert!(!context.contains("**PR:**"));
    }

    #[test]
    fn test_format_similar_issues_context_multiple_numbered() {
        let similar = vec![
            SimilarIssueWithDetails {
                embedding: IssueEmbedding::new("linear", "a1", vec![0.1]),
                similarity: 0.90,
                outcome: None,
                pr_url: None,
            },
            SimilarIssueWithDetails {
                embedding: IssueEmbedding::new("linear", "b2", vec![0.2]),
                similarity: 0.80,
                outcome: None,
                pr_url: None,
            },
            SimilarIssueWithDetails {
                embedding: IssueEmbedding::new("linear", "c3", vec![0.3]),
                similarity: 0.70,
                outcome: None,
                pr_url: None,
            },
        ];

        let context = format_similar_issues_context(&similar);
        assert!(context.contains("### 1. a1"));
        assert!(context.contains("### 2. b2"));
        assert!(context.contains("### 3. c3"));
    }

    #[test]
    fn test_format_similar_issues_context_uses_short_id() {
        let mut emb = IssueEmbedding::new("linear", "long-uuid", vec![0.1]);
        emb.short_id = Some("PROJ-42".to_string());

        let similar = vec![SimilarIssueWithDetails {
            embedding: emb,
            similarity: 0.88,
            outcome: None,
            pr_url: None,
        }];

        let context = format_similar_issues_context(&similar);
        assert!(context.contains("PROJ-42"));
        // The long uuid should NOT appear as the header identifier
        assert!(!context.contains("long-uuid"));
    }

    #[test]
    fn test_format_similar_issues_context_falls_back_to_issue_id() {
        // short_id is None, so it should fall back to issue_id
        let emb = IssueEmbedding::new("sentry", "fallback-id", vec![0.1]);

        let similar = vec![SimilarIssueWithDetails {
            embedding: emb,
            similarity: 0.77,
            outcome: None,
            pr_url: None,
        }];

        let context = format_similar_issues_context(&similar);
        assert!(context.contains("fallback-id"));
    }

    #[test]
    fn test_format_similar_issues_context_similarity_100_percent() {
        let similar = vec![SimilarIssueWithDetails {
            embedding: IssueEmbedding::new("linear", "dup", vec![0.1]),
            similarity: 1.0,
            outcome: None,
            pr_url: None,
        }];

        let context = format_similar_issues_context(&similar);
        assert!(context.contains("100%"));
    }

    #[test]
    fn test_format_similar_issues_context_similarity_0_percent() {
        let similar = vec![SimilarIssueWithDetails {
            embedding: IssueEmbedding::new("linear", "unrelated", vec![0.1]),
            similarity: 0.0,
            outcome: None,
            pr_url: None,
        }];

        let context = format_similar_issues_context(&similar);
        assert!(context.contains("0%"));
    }

    #[test]
    fn test_config_default_min_similarity() {
        let config = IssueEmbeddingConfig::default();
        assert!(
            (config.min_similarity - 0.7).abs() < f64::EPSILON,
            "default min_similarity should be 0.7"
        );
    }

    #[test]
    fn test_config_default_max_similar_issues() {
        let config = IssueEmbeddingConfig::default();
        assert_eq!(config.max_similar_issues, 5);
    }

    #[test]
    fn test_config_default_skip_similarity_threshold() {
        let config = IssueEmbeddingConfig::default();
        assert!(
            (config.skip_similarity_threshold - 0.90).abs() < f64::EPSILON,
            "default skip_similarity_threshold should be 0.90"
        );
    }

    #[test]
    fn test_config_default_enabled() {
        let config = IssueEmbeddingConfig::default();
        assert!(config.enabled, "default enabled should be true");
    }

    #[test]
    fn test_config_custom_values() {
        let config = IssueEmbeddingConfig {
            enabled: false,
            min_similarity: 0.5,
            max_similar_issues: 10,
            skip_similarity_threshold: 0.95,
        };
        assert!(!config.enabled);
        assert!((config.min_similarity - 0.5).abs() < f64::EPSILON);
        assert_eq!(config.max_similar_issues, 10);
        assert!((config.skip_similarity_threshold - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_clone() {
        let config = IssueEmbeddingConfig::default();
        let cloned = config.clone();
        assert_eq!(config.enabled, cloned.enabled);
        assert!((config.min_similarity - cloned.min_similarity).abs() < f64::EPSILON);
        assert_eq!(config.max_similar_issues, cloned.max_similar_issues);
    }

    #[test]
    fn test_config_debug() {
        let config = IssueEmbeddingConfig::default();
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("IssueEmbeddingConfig"));
        assert!(dbg.contains("enabled"));
        assert!(dbg.contains("min_similarity"));
    }

    #[test]
    fn test_build_embedding_text_labels_with_non_string_elements() {
        let mut issue = make_issue("Mixed labels");
        issue.metadata.insert(
            "labels".to_string(),
            serde_json::json!(["bug", 42, null, "critical"]),
        );
        let text = build_embedding_text(&issue);
        // Only string elements should appear
        assert!(text.contains("Labels: bug, critical"));
        assert!(!text.contains("42"));
    }

    #[test]
    fn test_build_embedding_text_stack_trace_under_2000_not_truncated() {
        let mut issue = make_issue("Short stack");
        let short_stack = "a".repeat(500);
        issue
            .metadata
            .insert("stack_trace".to_string(), serde_json::json!(short_stack));
        let text = build_embedding_text(&issue);
        assert!(!text.ends_with("..."));
        assert!(text.contains(&"a".repeat(500)));
    }

    #[test]
    fn test_build_embedding_text_stack_trace_2001_truncated() {
        let mut issue = make_issue("Just over stack");
        let stack = "z".repeat(2001);
        issue
            .metadata
            .insert("stack_trace".to_string(), serde_json::json!(stack));
        let text = build_embedding_text(&issue);
        assert!(text.ends_with("..."));
        assert!(text.contains(&"z".repeat(2000)));
    }

    #[test]
    fn test_build_embedding_text_non_string_error_message_skipped() {
        let mut issue = make_issue("Numeric error");
        issue
            .metadata
            .insert("error_message".to_string(), serde_json::json!(123));
        let text = build_embedding_text(&issue);
        assert_eq!(text, "Numeric error");
    }

    #[test]
    fn test_build_embedding_text_both_stack_and_error() {
        let mut issue = make_issue("Full error");
        issue
            .metadata
            .insert("stack_trace".to_string(), serde_json::json!("at line 42"));
        issue
            .metadata
            .insert("error_message".to_string(), serde_json::json!("NullRef"));
        let text = build_embedding_text(&issue);
        assert!(text.contains("at line 42"));
        assert!(text.contains("NullRef"));
        // Error message should come after stack trace
        let stack_pos = text.find("at line 42").unwrap();
        let error_pos = text.find("NullRef").unwrap();
        assert!(error_pos > stack_pos);
    }

    #[test]
    fn test_build_embedding_text_unicode_title() {
        let issue = make_issue("Fix bug in \u{65E5}\u{672C}\u{8A9E} module");
        let text = build_embedding_text(&issue);
        assert!(text.contains("\u{65E5}\u{672C}\u{8A9E}"));
    }

    #[test]
    fn test_build_embedding_text_unicode_stack_trace_truncation() {
        let mut issue = make_issue("Unicode stack");
        // Multi-byte characters that cross the 2000-byte boundary
        let stack: String = "\u{1F600}".repeat(600); // Each is 4 bytes, 600 * 4 = 2400 bytes
        issue
            .metadata
            .insert("stack_trace".to_string(), serde_json::json!(stack));
        let text = build_embedding_text(&issue);
        assert!(text.ends_with("..."));
        // Should not panic from splitting multi-byte characters
    }

    #[test]
    fn test_format_similar_issues_context_header() {
        let similar = vec![SimilarIssueWithDetails {
            embedding: IssueEmbedding::new("linear", "x", vec![0.1]),
            similarity: 0.5,
            outcome: None,
            pr_url: None,
        }];
        let context = format_similar_issues_context(&similar);
        assert!(context.contains("## Similar Past Issues"));
        assert!(context.contains("Use this context to inform your approach"));
    }

    #[test]
    fn test_format_similar_issues_context_five_items() {
        let similar: Vec<SimilarIssueWithDetails> = (1..=5)
            .map(|i| SimilarIssueWithDetails {
                embedding: IssueEmbedding::new("linear", format!("id-{}", i), vec![0.1]),
                similarity: 0.9 - (i as f64 * 0.05),
                outcome: Some("success".to_string()),
                pr_url: Some(format!("https://github.com/pull/{}", i)),
            })
            .collect();
        let context = format_similar_issues_context(&similar);
        for i in 1..=5 {
            assert!(context.contains(&format!("### {}.", i)));
            assert!(context.contains(&format!("id-{}", i)));
        }
    }

    #[test]
    fn test_similar_issue_with_details_clone() {
        let detail = SimilarIssueWithDetails {
            embedding: IssueEmbedding::new("linear", "456", vec![0.1, 0.2]),
            similarity: 0.85,
            outcome: Some("merged".to_string()),
            pr_url: Some("https://github.com/pull/42".to_string()),
        };
        let cloned = detail.clone();
        assert!((cloned.similarity - 0.85).abs() < f64::EPSILON);
        assert_eq!(cloned.outcome, Some("merged".to_string()));
        assert_eq!(
            cloned.pr_url,
            Some("https://github.com/pull/42".to_string())
        );
    }

    #[test]
    fn test_similar_issue_with_details_debug() {
        let detail = SimilarIssueWithDetails {
            embedding: IssueEmbedding::new("linear", "789", vec![]),
            similarity: 0.0,
            outcome: None,
            pr_url: None,
        };
        let dbg = format!("{:?}", detail);
        assert!(dbg.contains("SimilarIssueWithDetails"));
    }

    #[test]
    fn test_build_embedding_text_empty_metadata() {
        let issue = make_issue("No metadata issue");
        let text = build_embedding_text(&issue);
        assert_eq!(text, "No metadata issue");
    }

    #[test]
    fn test_build_embedding_text_capacity_estimation() {
        // Test that the capacity estimation does not cause issues
        let mut issue = make_issue("A");
        issue.description = Some("B".repeat(10000));
        let text = build_embedding_text(&issue);
        assert!(text.contains("A"));
        assert!(text.contains(&"B".repeat(10000)));
    }

    #[test]
    fn test_format_context_fractional_similarity() {
        let similar = vec![SimilarIssueWithDetails {
            embedding: IssueEmbedding::new("linear", "frac", vec![0.1]),
            similarity: 0.777,
            outcome: None,
            pr_url: None,
        }];
        let context = format_similar_issues_context(&similar);
        // 0.777 * 100 = 77.7, formatted as 78% (rounded)
        assert!(context.contains("78%"));
    }

    #[test]
    fn test_build_embedding_text_labels_single_element() {
        let mut issue = make_issue("Single label");
        issue
            .metadata
            .insert("labels".to_string(), serde_json::json!(["enhancement"]));
        let text = build_embedding_text(&issue);
        assert!(text.contains("Labels: enhancement"));
    }

    #[test]
    fn test_build_embedding_text_null_labels_skipped() {
        let mut issue = make_issue("Null labels");
        issue
            .metadata
            .insert("labels".to_string(), serde_json::Value::Null);
        let text = build_embedding_text(&issue);
        assert!(!text.contains("Labels:"));
    }

    #[test]
    fn test_build_embedding_text_null_stack_trace_skipped() {
        let mut issue = make_issue("Null stack");
        issue
            .metadata
            .insert("stack_trace".to_string(), serde_json::Value::Null);
        let text = build_embedding_text(&issue);
        assert_eq!(text, "Null stack");
    }

    #[test]
    fn test_build_embedding_text_null_error_message_skipped() {
        let mut issue = make_issue("Null error");
        issue
            .metadata
            .insert("error_message".to_string(), serde_json::Value::Null);
        let text = build_embedding_text(&issue);
        assert_eq!(text, "Null error");
    }

    // ===================================================================
    // Mock tracker and IssueEmbeddingService tests
    // ===================================================================

    use claudear_core::types::{FixAttempt, FixAttemptStats, FixAttemptStatus};
    use claudear_storage::{
        ActivityStore, AttemptTracker, ChatStore, DiscordStore, EmbeddingStore, EvaluationStore,
        ExperimentStore, KnowledgeStore, RegressionStore, RepoStore, SimilarityStore, UserStore,
        WebhookStore,
    };
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// A mock tracker that implements all sub-traits required by FixAttemptTracker.
    /// Stores embeddings in memory and returns configurable results for get_embedding
    /// and find_similar_issues_vector.
    struct MockTracker {
        stored_embeddings: Mutex<Vec<IssueEmbedding>>,
        /// If set, `get_embedding` returns this for any query.
        get_embedding_result: Mutex<Option<IssueEmbedding>>,
        /// If set, `find_similar_issues_vector` returns Some(this).
        /// If None, returns None (simulating vectorlite unavailable).
        similar_results: Mutex<Option<Vec<(IssueEmbedding, f64)>>>,
        /// Attempts returned by `get_attempts_batch`.
        batch_attempts: Mutex<Vec<Option<FixAttempt>>>,
    }

    impl MockTracker {
        fn new() -> Self {
            Self {
                stored_embeddings: Mutex::new(Vec::new()),
                get_embedding_result: Mutex::new(None),
                similar_results: Mutex::new(None),
                batch_attempts: Mutex::new(Vec::new()),
            }
        }

        fn with_get_embedding(self, emb: IssueEmbedding) -> Self {
            *self.get_embedding_result.lock().unwrap() = Some(emb);
            self
        }

        fn with_similar_results(self, results: Vec<(IssueEmbedding, f64)>) -> Self {
            *self.similar_results.lock().unwrap() = Some(results);
            self
        }

        fn with_batch_attempts(self, attempts: Vec<Option<FixAttempt>>) -> Self {
            *self.batch_attempts.lock().unwrap() = attempts;
            self
        }
    }

    impl AttemptTracker for MockTracker {
        fn has_attempted(&self, _: &str, _: &str) -> Result<bool> {
            Ok(false)
        }
        fn get_attempted_issue_ids(&self, _: &str) -> Result<HashSet<String>> {
            Ok(HashSet::new())
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
        fn get_attempt(&self, _: &str, _: &str) -> Result<Option<FixAttempt>> {
            Ok(None)
        }
        fn get_attempts_by_status(&self, _: FixAttemptStatus) -> Result<Vec<FixAttempt>> {
            Ok(vec![])
        }
        fn get_pending_prs(&self) -> Result<Vec<FixAttempt>> {
            Ok(vec![])
        }
        fn get_attempt_by_pr_url(&self, _: &str) -> Result<Option<FixAttempt>> {
            Ok(None)
        }
        fn reset_attempt(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_stats(&self) -> Result<FixAttemptStats> {
            Ok(FixAttemptStats {
                total: 0,
                pending: 0,
                success: 0,
                failed: 0,
                merged: 0,
                closed: 0,
                cannot_fix: 0,
                by_source: Default::default(),
            })
        }
        fn increment_retry(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn mark_cannot_fix(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_retryable_issues(&self, _: u32) -> Result<Vec<FixAttempt>> {
            Ok(vec![])
        }
        fn prepare_for_retry(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
    }

    impl EmbeddingStore for MockTracker {
        fn store_embedding(&self, embedding: &IssueEmbedding) -> Result<i64> {
            self.stored_embeddings
                .lock()
                .unwrap()
                .push(embedding.clone());
            Ok(self.stored_embeddings.lock().unwrap().len() as i64)
        }

        fn store_embeddings_batch(&self, embeddings: &[IssueEmbedding]) -> Result<()> {
            self.stored_embeddings
                .lock()
                .unwrap()
                .extend(embeddings.iter().cloned());
            Ok(())
        }

        fn get_embedding(&self, _source: &str, _issue_id: &str) -> Result<Option<IssueEmbedding>> {
            Ok(self.get_embedding_result.lock().unwrap().clone())
        }

        fn find_similar_issues_vector(
            &self,
            _query_embedding: &[f32],
            _source: &str,
            _exclude_issue_id: Option<&str>,
            _min_similarity: f64,
            _limit: usize,
        ) -> Result<Option<Vec<(IssueEmbedding, f64)>>> {
            Ok(self.similar_results.lock().unwrap().clone())
        }
    }

    impl ActivityStore for MockTracker {
        fn get_attempts_batch(&self, _keys: &[(&str, &str)]) -> Result<Vec<Option<FixAttempt>>> {
            Ok(self.batch_attempts.lock().unwrap().clone())
        }
    }

    impl KnowledgeStore for MockTracker {}
    impl RepoStore for MockTracker {}
    impl UserStore for MockTracker {}
    impl RegressionStore for MockTracker {}
    impl ChatStore for MockTracker {}
    impl ExperimentStore for MockTracker {}
    impl EvaluationStore for MockTracker {}
    impl WebhookStore for MockTracker {}
    impl SimilarityStore for MockTracker {}
    impl DiscordStore for MockTracker {}

    // --- Helper to create a mock EmbeddingClient for tests ---
    // We use the real EmbeddingClient with the fast (AllMiniLML6V2) model.
    // This is cached after first download and runs locally.
    fn make_embedding_client() -> Arc<EmbeddingClient> {
        use crate::feedback::EmbeddingConfig;
        use fastembed::EmbeddingModel;
        Arc::new(
            EmbeddingClient::new(EmbeddingConfig {
                model: EmbeddingModel::AllMiniLML6V2,
                show_download_progress: false,
                cache_dir: None,
                pool_size: 1,
                ..EmbeddingConfig::default()
            })
            .expect("Failed to create test embedding client"),
        )
    }

    fn make_service(
        tracker: Arc<dyn claudear_storage::FixAttemptTracker>,
    ) -> IssueEmbeddingService {
        let client = make_embedding_client();
        IssueEmbeddingService::new(client, tracker, IssueEmbeddingConfig::default())
    }

    fn make_service_disabled(
        tracker: Arc<dyn claudear_storage::FixAttemptTracker>,
    ) -> IssueEmbeddingService {
        let client = make_embedding_client();
        IssueEmbeddingService::new(
            client,
            tracker,
            IssueEmbeddingConfig {
                enabled: false,
                ..IssueEmbeddingConfig::default()
            },
        )
    }

    fn make_service_with_config(
        tracker: Arc<dyn claudear_storage::FixAttemptTracker>,
        config: IssueEmbeddingConfig,
    ) -> IssueEmbeddingService {
        let client = make_embedding_client();
        IssueEmbeddingService::new(client, tracker, config)
    }

    #[test]
    fn test_service_is_enabled_default() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker);
        assert!(service.is_enabled());
    }

    #[test]
    fn test_service_is_enabled_false() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service_disabled(tracker);
        assert!(!service.is_enabled());
    }

    #[test]
    fn test_service_with_defaults_constructor() {
        let tracker = Arc::new(MockTracker::new());
        let client = make_embedding_client();
        let service = IssueEmbeddingService::with_defaults(client, tracker);
        // with_defaults uses IssueEmbeddingConfig::default(), which has enabled=true
        assert!(service.is_enabled());
    }

    #[test]
    fn test_service_get_embedding_returns_none() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker);
        let result = service.get_embedding("linear", "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_service_get_embedding_returns_some() {
        let emb = IssueEmbedding::new("linear", "123", vec![0.1, 0.2]);
        let tracker = Arc::new(MockTracker::new().with_get_embedding(emb.clone()));
        let service = make_service(tracker);
        let result = service.get_embedding("linear", "123").unwrap();
        assert!(result.is_some());
        let returned = result.unwrap();
        assert_eq!(returned.issue_id, "123");
    }

    #[test]
    fn test_service_has_embedding_true() {
        let emb = IssueEmbedding::new("linear", "123", vec![0.1]);
        let tracker = Arc::new(MockTracker::new().with_get_embedding(emb));
        let service = make_service(tracker);
        assert!(service.has_embedding("linear", "123"));
    }

    #[test]
    fn test_service_has_embedding_false() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker);
        assert!(!service.has_embedding("linear", "nonexistent"));
    }

    #[tokio::test]
    async fn test_service_embed_batch_empty() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker);
        let result = service.embed_batch(&[], "linear").await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_service_find_similar_disabled() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service_disabled(tracker);
        let issue = make_issue("Test issue");
        let result = service.find_similar(&issue, "linear").await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_service_check_duplicate_disabled() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service_disabled(tracker);
        let issue = make_issue("Test issue");
        let result = service.check_duplicate(&issue, "linear").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_service_find_similar_vectorlite_unavailable() {
        // MockTracker default returns None for find_similar_issues_vector
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker);
        let issue = make_issue("Test issue for similarity");
        let result = service.find_similar(&issue, "linear").await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_service_find_similar_with_results_no_attempts() {
        let emb1 = IssueEmbedding::new("linear", "old-1", vec![0.1, 0.2]);
        let emb2 = IssueEmbedding::new("linear", "old-2", vec![0.3, 0.4]);
        let similar = vec![(emb1, 0.85), (emb2, 0.75)];
        // batch_attempts returns empty (no matching attempts)
        let tracker = Arc::new(
            MockTracker::new()
                .with_similar_results(similar)
                .with_batch_attempts(vec![None, None]),
        );
        let service = make_service(tracker);
        let issue = make_issue("New similar issue");
        let result = service.find_similar(&issue, "linear").await.unwrap();
        assert_eq!(result.len(), 2);
        assert!((result[0].similarity - 0.85).abs() < f64::EPSILON);
        assert!(result[0].outcome.is_none());
        assert!(result[0].pr_url.is_none());
        assert!((result[1].similarity - 0.75).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_service_find_similar_with_attempts() {
        let emb = IssueEmbedding::new("linear", "old-1", vec![0.1]);
        let similar = vec![(emb, 0.90)];
        let attempt = FixAttempt {
            id: 1,
            issue_id: "old-1".to_string(),
            short_id: "PROJ-1".to_string(),
            source: "linear".to_string(),
            attempted_at: Utc::now(),
            pr_url: Some("https://github.com/org/repo/pull/10".to_string()),
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };
        let tracker = Arc::new(
            MockTracker::new()
                .with_similar_results(similar)
                .with_batch_attempts(vec![Some(attempt)]),
        );
        let service = make_service(tracker);
        let issue = make_issue("Issue with fix history");
        let result = service.find_similar(&issue, "linear").await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].outcome.as_deref(), Some("merged"));
        assert_eq!(
            result[0].pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/10")
        );
    }

    #[tokio::test]
    async fn test_service_embed_issue_stores_embedding() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker.clone());
        let mut issue = make_issue("Embed this issue");
        issue.description = Some("Description for embedding".to_string());
        issue
            .metadata
            .insert("labels".to_string(), serde_json::json!(["bug", "critical"]));

        let result = service.embed_issue(&issue, "linear").await.unwrap();
        assert_eq!(result.issue_id, "123");
        assert_eq!(result.short_id, Some("PROJ-123".to_string()));
        assert_eq!(result.title, Some("Embed this issue".to_string()));
        assert_eq!(
            result.description,
            Some("Description for embedding".to_string())
        );
        assert!(result.embedding.is_some());
        assert!(result.embedding_model.is_some());
        // The embedding should have been stored in the tracker
        let stored = tracker.stored_embeddings.lock().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].issue_id, "123");
    }

    #[tokio::test]
    async fn test_service_embed_issue_without_labels() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker.clone());
        let issue = make_issue("No labels issue");

        let result = service.embed_issue(&issue, "sentry").await.unwrap();
        assert!(result.labels.is_none());
    }

    #[tokio::test]
    async fn test_service_embed_issue_with_labels() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker.clone());
        let mut issue = make_issue("Labeled issue embed");
        issue
            .metadata
            .insert("labels".to_string(), serde_json::json!(["bug", "p1"]));

        let result = service.embed_issue(&issue, "linear").await.unwrap();
        assert!(result.labels.is_some());
        let labels_str = result.labels.unwrap();
        assert!(labels_str.contains("bug"));
        assert!(labels_str.contains("p1"));
    }

    #[tokio::test]
    async fn test_service_embed_batch_multiple_issues() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker.clone());

        let mut issue1 = make_issue("First batch issue");
        issue1.id = "batch-1".to_string();
        let mut issue2 = make_issue("Second batch issue");
        issue2.id = "batch-2".to_string();

        let issues = vec![issue1, issue2];
        let result = service.embed_batch(&issues, "linear").await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].issue_id, "batch-1");
        assert_eq!(result[1].issue_id, "batch-2");
        // Both should have been stored
        let stored = tracker.stored_embeddings.lock().unwrap();
        assert_eq!(stored.len(), 2);
    }

    #[tokio::test]
    async fn test_service_check_duplicate_no_similar() {
        // find_similar returns empty when vectorlite unavailable (None)
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker);
        let issue = make_issue("Unique issue");
        let result = service.check_duplicate(&issue, "linear").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_service_check_duplicate_below_threshold() {
        // Similar issue found but below skip_similarity_threshold (0.90 default)
        let emb = IssueEmbedding::new("linear", "existing", vec![0.1]);
        let tracker = Arc::new(
            MockTracker::new()
                .with_similar_results(vec![(emb, 0.80)]) // below 0.90
                .with_batch_attempts(vec![Some(FixAttempt {
                    id: 1,
                    issue_id: "existing".to_string(),
                    short_id: "PROJ-1".to_string(),
                    source: "linear".to_string(),
                    attempted_at: Utc::now(),
                    pr_url: None,
                    scm_repo: None,
                    scm_pr_number: None,
                    status: FixAttemptStatus::Pending,
                    error_message: None,
                    merged_at: None,
                    resolved_at: None,
                    retry_count: 0,
                    last_retry_at: None,
                    issue_labels: vec![],
                    parent_attempt_id: None,
                    cascade_repo: None,
                })]),
        );
        let service = make_service(tracker);
        let issue = make_issue("Similar but not duplicate");
        let result = service.check_duplicate(&issue, "linear").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_service_check_duplicate_above_threshold_active() {
        // Similar issue above threshold with active outcome (pending)
        let emb = IssueEmbedding::new("linear", "dup-issue", vec![0.1]);
        let tracker = Arc::new(
            MockTracker::new()
                .with_similar_results(vec![(emb, 0.95)]) // above 0.90
                .with_batch_attempts(vec![Some(FixAttempt {
                    id: 2,
                    issue_id: "dup-issue".to_string(),
                    short_id: "PROJ-2".to_string(),
                    source: "linear".to_string(),
                    attempted_at: Utc::now(),
                    pr_url: None,
                    scm_repo: None,
                    scm_pr_number: None,
                    status: FixAttemptStatus::Pending,
                    error_message: None,
                    merged_at: None,
                    resolved_at: None,
                    retry_count: 0,
                    last_retry_at: None,
                    issue_labels: vec![],
                    parent_attempt_id: None,
                    cascade_repo: None,
                })]),
        );
        let service = make_service(tracker);
        let issue = make_issue("Duplicate of active issue");
        let result = service.check_duplicate(&issue, "linear").await.unwrap();
        assert!(result.is_some());
        let dup = result.unwrap();
        assert_eq!(dup.embedding.issue_id, "dup-issue");
        assert!(dup.similarity >= 0.90);
    }

    #[tokio::test]
    async fn test_service_check_duplicate_above_threshold_success() {
        // Similar issue above threshold with "success" outcome
        let emb = IssueEmbedding::new("linear", "success-issue", vec![0.1]);
        let tracker = Arc::new(
            MockTracker::new()
                .with_similar_results(vec![(emb, 0.95)])
                .with_batch_attempts(vec![Some(FixAttempt {
                    id: 3,
                    issue_id: "success-issue".to_string(),
                    short_id: "PROJ-3".to_string(),
                    source: "linear".to_string(),
                    attempted_at: Utc::now(),
                    pr_url: Some("https://github.com/pull/5".to_string()),
                    scm_repo: None,
                    scm_pr_number: None,
                    status: FixAttemptStatus::Success,
                    error_message: None,
                    merged_at: None,
                    resolved_at: None,
                    retry_count: 0,
                    last_retry_at: None,
                    issue_labels: vec![],
                    parent_attempt_id: None,
                    cascade_repo: None,
                })]),
        );
        let service = make_service(tracker);
        let issue = make_issue("Duplicate of success issue");
        let result = service.check_duplicate(&issue, "linear").await.unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_service_check_duplicate_above_threshold_merged() {
        // Similar issue above threshold with "merged" outcome
        let emb = IssueEmbedding::new("linear", "merged-issue", vec![0.1]);
        let tracker = Arc::new(
            MockTracker::new()
                .with_similar_results(vec![(emb, 0.92)])
                .with_batch_attempts(vec![Some(FixAttempt {
                    id: 4,
                    issue_id: "merged-issue".to_string(),
                    short_id: "PROJ-4".to_string(),
                    source: "linear".to_string(),
                    attempted_at: Utc::now(),
                    pr_url: Some("https://github.com/pull/8".to_string()),
                    scm_repo: None,
                    scm_pr_number: None,
                    status: FixAttemptStatus::Merged,
                    error_message: None,
                    merged_at: Some(Utc::now()),
                    resolved_at: None,
                    retry_count: 0,
                    last_retry_at: None,
                    issue_labels: vec![],
                    parent_attempt_id: None,
                    cascade_repo: None,
                })]),
        );
        let service = make_service(tracker);
        let issue = make_issue("Duplicate of merged issue");
        let result = service.check_duplicate(&issue, "linear").await.unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_service_check_duplicate_above_threshold_failed_not_dup() {
        // Similar issue above threshold but outcome is "failed" -- should NOT be a duplicate
        let emb = IssueEmbedding::new("linear", "failed-issue", vec![0.1]);
        let tracker = Arc::new(
            MockTracker::new()
                .with_similar_results(vec![(emb, 0.95)])
                .with_batch_attempts(vec![Some(FixAttempt {
                    id: 5,
                    issue_id: "failed-issue".to_string(),
                    short_id: "PROJ-5".to_string(),
                    source: "linear".to_string(),
                    attempted_at: Utc::now(),
                    pr_url: None,
                    scm_repo: None,
                    scm_pr_number: None,
                    status: FixAttemptStatus::Failed,
                    error_message: Some("build error".to_string()),
                    merged_at: None,
                    resolved_at: None,
                    retry_count: 0,
                    last_retry_at: None,
                    issue_labels: vec![],
                    parent_attempt_id: None,
                    cascade_repo: None,
                })]),
        );
        let service = make_service(tracker);
        let issue = make_issue("Similar to failed issue");
        let result = service.check_duplicate(&issue, "linear").await.unwrap();
        // Failed outcome should NOT be considered a duplicate
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_service_check_duplicate_above_threshold_no_outcome() {
        // Similar issue above threshold but no attempt record (outcome is None)
        let emb = IssueEmbedding::new("linear", "no-attempt", vec![0.1]);
        let tracker = Arc::new(
            MockTracker::new()
                .with_similar_results(vec![(emb, 0.95)])
                .with_batch_attempts(vec![None]),
        );
        let service = make_service(tracker);
        let issue = make_issue("Similar to untracked issue");
        let result = service.check_duplicate(&issue, "linear").await.unwrap();
        // No outcome means not actively handled, should NOT be a duplicate
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_service_check_duplicate_above_threshold_closed_not_dup() {
        // Similar issue above threshold but outcome is "closed" -- should NOT be a duplicate
        let emb = IssueEmbedding::new("linear", "closed-issue", vec![0.1]);
        let tracker = Arc::new(
            MockTracker::new()
                .with_similar_results(vec![(emb, 0.95)])
                .with_batch_attempts(vec![Some(FixAttempt {
                    id: 6,
                    issue_id: "closed-issue".to_string(),
                    short_id: "PROJ-6".to_string(),
                    source: "linear".to_string(),
                    attempted_at: Utc::now(),
                    pr_url: None,
                    scm_repo: None,
                    scm_pr_number: None,
                    status: FixAttemptStatus::Closed,
                    error_message: None,
                    merged_at: None,
                    resolved_at: None,
                    retry_count: 0,
                    last_retry_at: None,
                    issue_labels: vec![],
                    parent_attempt_id: None,
                    cascade_repo: None,
                })]),
        );
        let service = make_service(tracker);
        let issue = make_issue("Similar to closed issue");
        let result = service.check_duplicate(&issue, "linear").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_service_check_duplicate_custom_threshold() {
        // Use a lower skip_similarity_threshold (0.80) and check that 0.85 triggers duplicate
        let emb = IssueEmbedding::new("linear", "custom-thresh", vec![0.1]);
        let tracker = Arc::new(
            MockTracker::new()
                .with_similar_results(vec![(emb, 0.85)])
                .with_batch_attempts(vec![Some(FixAttempt {
                    id: 7,
                    issue_id: "custom-thresh".to_string(),
                    short_id: "PROJ-7".to_string(),
                    source: "linear".to_string(),
                    attempted_at: Utc::now(),
                    pr_url: None,
                    scm_repo: None,
                    scm_pr_number: None,
                    status: FixAttemptStatus::Pending,
                    error_message: None,
                    merged_at: None,
                    resolved_at: None,
                    retry_count: 0,
                    last_retry_at: None,
                    issue_labels: vec![],
                    parent_attempt_id: None,
                    cascade_repo: None,
                })]),
        );
        let service = make_service_with_config(
            tracker,
            IssueEmbeddingConfig {
                enabled: true,
                min_similarity: 0.5,
                max_similar_issues: 5,
                skip_similarity_threshold: 0.80,
            },
        );
        let issue = make_issue("Custom threshold test");
        let result = service.check_duplicate(&issue, "linear").await.unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_service_embed_batch_with_labels() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker.clone());

        let mut issue = make_issue("Batch with labels");
        issue.id = "batch-label-1".to_string();
        issue.metadata.insert(
            "labels".to_string(),
            serde_json::json!(["enhancement", "backend"]),
        );

        let result = service.embed_batch(&[issue], "linear").await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].labels.is_some());
        let labels_str = result[0].labels.as_ref().unwrap();
        assert!(labels_str.contains("enhancement"));
    }

    #[tokio::test]
    async fn test_service_embed_batch_without_labels() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker.clone());

        let mut issue = make_issue("Batch no labels");
        issue.id = "batch-nolabel-1".to_string();

        let result = service.embed_batch(&[issue], "linear").await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].labels.is_none());
    }

    #[tokio::test]
    async fn test_service_embed_issue_populates_all_fields() {
        let tracker = Arc::new(MockTracker::new());
        let service = make_service(tracker.clone());
        let mut issue = make_issue("Full field test");
        issue.description = Some("Full description".to_string());
        issue.url = "https://linear.app/issue/123".to_string();
        issue.priority = IssuePriority::Critical;
        issue.status = IssueStatus::InProgress;
        issue.updated_at = Some(Utc::now());

        let result = service.embed_issue(&issue, "linear").await.unwrap();
        assert_eq!(result.url, Some("https://linear.app/issue/123".to_string()));
        assert_eq!(result.priority, Some("critical".to_string()));
        assert_eq!(result.status, Some("in_progress".to_string()));
        assert!(result.updated_at.is_some());
        assert!(result.created_at <= Utc::now());
    }
}
