use crate::distribution::Markup;
use crate::distribution::PullRequestSummary;

/// A PR its assignee saw through since the previous digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedUp {
    /// The PR.
    pub pull_request: PullRequestSummary,
    /// Whether it merged rather than closed unmerged.
    pub merged: bool,
}

impl WrappedUp {
    /// One roundup line: the linked PR, its title, and how it ended.
    pub fn line(&self, markup: &dyn Markup) -> String {
        format!(
            "• {} {} ({})",
            markup.link(&self.pull_request.label(), &self.pull_request.url),
            markup.text(&self.pull_request.title),
            if self.merged { "merged" } else { "closed" }
        )
    }
}
