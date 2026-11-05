//! Shared storage types used across trait signatures and API responses.

use claudear_core::identity::{Identities, Role, UserStatus};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;

/// A user row from the database.
#[derive(Debug, Clone, Serialize)]
pub struct UserRow {
    pub id: i64,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub name: String,
    pub role: Role,
    pub avatar_url: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub status: UserStatus,
}

/// A single-use invite link for an invited user. Only the SHA-256 hash of the
/// secret is stored, and it is never serialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InviteRow {
    pub id: String,
    pub user_id: i64,
    #[serde(skip_serializing)]
    pub token_hash: String,
    pub created_by: Option<i64>,
    pub created_at: String,
    pub expires_at: String,
    pub accepted_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// The identities to store for one user. Writing replaces the user's whole row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityWrite {
    pub slug: Option<String>,
    pub identities: Identities,
}

/// An invite to insert. `expires_at` is UTC `%Y-%m-%d %H:%M:%S`.
#[derive(Clone, PartialEq, Eq)]
pub struct InviteWrite {
    pub token_hash: String,
    pub expires_at: String,
    pub created_by: i64,
}

impl fmt::Debug for InviteWrite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InviteWrite")
            .field("expires_at", &self.expires_at)
            .field("created_by", &self.created_by)
            .finish_non_exhaustive()
    }
}

/// The user a [`DirectoryWrite`] applies to.
#[derive(Clone, PartialEq, Eq)]
pub enum DirectoryUser {
    /// Sparse update of an existing user; `None` leaves the column unchanged.
    Existing {
        user_id: i64,
        name: Option<String>,
        role: Option<Role>,
    },
    /// A user to create. `Active` carries a real password hash and no invite;
    /// `Invited` carries an unusable hash and normally an invite.
    New {
        email: String,
        name: String,
        role: Role,
        password_hash: String,
        status: UserStatus,
    },
}

impl fmt::Debug for DirectoryUser {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Existing {
                user_id,
                name,
                role,
            } => formatter
                .debug_struct("Existing")
                .field("user_id", user_id)
                .field("name", name)
                .field("role", role)
                .finish(),
            Self::New { role, status, .. } => formatter
                .debug_struct("New")
                .field("role", role)
                .field("status", status)
                .finish_non_exhaustive(),
        }
    }
}

/// One entry of an all-or-nothing directory write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryWrite {
    pub user: DirectoryUser,
    pub identities: Option<IdentityWrite>,
    pub invite: Option<InviteWrite>,
}

/// A personal access token row. The secret is never stored or serialized; only
/// its hash (kept server-side) and a short display prefix.
#[derive(Debug, Clone, Serialize)]
pub struct ApiTokenRow {
    pub id: String,
    pub user_id: i64,
    pub name: String,
    #[serde(skip_serializing)]
    pub token_hash: String,
    pub token_prefix: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
}

/// An API token joined with its owner's email/name, for the admin usage view.
#[derive(Debug, Clone, Serialize)]
pub struct ApiTokenWithOwner {
    #[serde(flatten)]
    pub token: ApiTokenRow,
    pub user_email: String,
    pub user_name: String,
}

/// An indexed repository stored in the database.
#[derive(Debug, Clone, Serialize)]
pub struct StoredIndexedRepo {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub scm_url: Option<String>,
    pub default_branch: String,
    pub file_count: i64,
    pub last_indexed_at: String,
    pub created_at: String,
}

/// Index statistics.
#[derive(Debug, Clone, Serialize)]
pub struct IndexStats {
    pub repo_count: usize,
    pub file_count: usize,
    pub last_indexed_at: Option<String>,
}

/// A Discord channel/thread tracked in the knowledgebase index, enriched with
/// derived indexing stats (chunk count + indexed timeline) for the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct StoredDiscordChannel {
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub parent_id: Option<String>,
    /// Name of the category this channel/thread belongs to, resolved from the
    /// persisted category rows. For a channel this is its parent category; for a
    /// thread it is the category of its parent channel. `None` if uncategorised
    /// or the category has not been indexed yet.
    pub category_name: Option<String>,
    pub name: Option<String>,
    /// "channel" | "thread" | "category".
    pub kind: String,
    /// Whether this is an archived thread.
    pub archived: bool,
    /// Whether the full history (within the backfill window) has been scraped.
    pub backfill_complete: bool,
    /// Forward cursor: newest indexed message id.
    pub last_indexed_message_id: Option<String>,
    pub last_indexed_at: Option<String>,
    /// Number of indexed (embedded or pending) message chunks for this channel.
    pub chunk_count: i64,
    /// Timestamp of the oldest indexed message (start of the indexed window).
    pub indexed_from: Option<String>,
    /// Timestamp of the newest indexed message (end of the indexed window).
    pub indexed_to: Option<String>,
}

/// Aggregate statistics for the Discord knowledgebase index.
#[derive(Debug, Clone, Serialize)]
pub struct DiscordKnowledgebaseStats {
    /// Number of tracked regular channels (excludes threads/categories).
    pub channel_count: usize,
    /// Number of tracked threads.
    pub thread_count: usize,
    /// Total indexed message chunks across all channels.
    pub chunk_count: usize,
    /// Most recent index run across all channels.
    pub last_indexed_at: Option<String>,
}

/// Current indexing progress.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IndexingProgress {
    pub status: String,
    pub total_repos: usize,
    pub indexed_repos: usize,
    pub current_repo: Option<String>,
    pub current_repo_files: usize,
    pub total_files_indexed: usize,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
}

impl Default for IndexingProgress {
    fn default() -> Self {
        Self {
            status: "idle".to_string(),
            total_repos: 0,
            indexed_repos: 0,
            current_repo: None,
            current_repo_files: 0,
            total_files_indexed: 0,
            started_at: None,
            updated_at: None,
        }
    }
}

/// Inference statistics.
#[derive(Debug, Clone, Serialize)]
pub struct InferenceStats {
    pub total_attempts: usize,
    pub with_feedback: usize,
    pub correct: usize,
    pub accuracy: f64,
    pub by_confidence: ConfidenceBreakdown,
}

/// Breakdown by confidence level.
#[derive(Debug, Clone, Serialize)]
pub struct ConfidenceBreakdown {
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub none: usize,
}

/// A single inference attempt from the history.
#[derive(Debug, Clone, Serialize)]
pub struct InferenceHistoryEntry {
    pub id: i64,
    pub issue_id: String,
    pub issue_source: String,
    pub extracted_keywords: Option<String>,
    pub inferred_repo_name: Option<String>,
    pub confidence: Option<String>,
    pub inference_reason: Option<String>,
    pub was_correct: Option<bool>,
    pub duration_ms: Option<i64>,
    pub created_at: String,
}

/// A repository stored in the database.
#[derive(Debug, Clone, Serialize)]
pub struct StoredRepository {
    pub id: i64,
    pub name: String,
    pub path: Option<String>,
    pub scm_url: String,
    pub created_at: String,
}

/// A dependency relationship stored in the database.
#[derive(Debug, Clone, Serialize)]
pub struct StoredDependency {
    pub id: i64,
    pub upstream: String,
    pub downstream: String,
    pub dep_type: String,
    pub created_at: String,
}

/// Diagnostic counts for all major tables.
#[derive(Debug, Clone, Serialize)]
pub struct PurgeResult {
    pub fix_attempts: usize,
    pub prs: usize,
    pub pr_reviews: usize,
    pub pr_review_comments: usize,
    pub pr_review_states: usize,
    pub claude_executions: usize,
    pub strategy_fingerprints: usize,
    pub diff_analyses: usize,
    pub regression_watches: usize,
    pub release_tracking: usize,
    pub regression_checks: usize,
    pub qa_usage: usize,
    pub activity_log: usize,
    pub processing_metrics: usize,
    pub webhook_deliveries: usize,
    pub issue_clusters: usize,
    pub issue_cluster_members: usize,
    pub content_clusters: usize,
    pub severity_scores: usize,
    pub suppression_log: usize,
    pub eval_snapshots: usize,
    pub eval_deltas: usize,
    pub feedback_outcomes_detached: usize,
}

impl PurgeResult {
    pub fn total_deleted(&self) -> usize {
        self.fix_attempts
            + self.prs
            + self.pr_reviews
            + self.pr_review_comments
            + self.pr_review_states
            + self.claude_executions
            + self.strategy_fingerprints
            + self.diff_analyses
            + self.regression_watches
            + self.release_tracking
            + self.regression_checks
            + self.qa_usage
            + self.activity_log
            + self.processing_metrics
            + self.webhook_deliveries
            + self.issue_clusters
            + self.issue_cluster_members
            + self.content_clusters
            + self.severity_scores
            + self.suppression_log
            + self.eval_snapshots
            + self.eval_deltas
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticCounts {
    pub fix_attempts: i64,
    pub fix_attempts_by_status: HashMap<String, i64>,
    pub activity_log: i64,
    pub claude_executions: i64,
    pub pr_reviews: i64,
    pub pr_review_states: i64,
    pub issues: i64,
    pub similar_issues: i64,
    pub repositories: i64,
    pub repo_files: i64,
    pub inference_attempts: i64,
    pub error_patterns: i64,
    pub processing_metrics: i64,
    pub feedback_outcomes: i64,
    pub prs: i64,
    pub recent_fix_attempts: Vec<(String, String, String, String)>,
}

/// A stored PR review comment from the database.
#[derive(Debug, Clone, Serialize)]
pub struct StoredPrReviewComment {
    pub id: i64,
    pub scm_comment_id: i64,
    pub pr_url: String,
    pub review_id: Option<i64>,
    pub path: String,
    pub position: Option<i64>,
    pub line: Option<i64>,
    pub body: String,
    pub author: String,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
}
