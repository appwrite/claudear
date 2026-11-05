//! Where open PRs and their ownership evidence come from.

use async_trait::async_trait;
use claudear_analysis::distribution::PullRequest;
use claudear_core::error::Result;

/// A code host that can describe a PR and who works on its code.
#[async_trait]
pub trait PullRequestHost: Send + Sync {
    /// The PR's state, outstanding review requests and submitted reviews.
    async fn pull_request(&self, repo: &str, number: i64) -> Result<PullRequest>;

    /// Paths the PR changes.
    async fn changed_files(&self, repo: &str, number: i64) -> Result<Vec<String>>;

    /// The repository's CODEOWNERS file, if it has one.
    async fn codeowners(&self, repo: &str) -> Result<Option<String>>;

    /// Logins authoring recent commits to `path`, or to the whole repository
    /// when `path` is `None`.
    async fn commit_authors(&self, repo: &str, path: Option<&str>) -> Result<Vec<String>>;
}
