//! Playbook loading for `[deploy_qa]` agent instructions.

use claudear_core::error::{Error, Result};
use std::path::Path;

/// Issue source name for synthetic deploy-QA work items.
pub const DEPLOY_QA_SOURCE: &str = "deploy_qa";

/// Bundled playbook shipped with Claudear (used when `instructions_path` is unset).
pub const BUNDLED_PLAYBOOK: &str = include_str!("../../../../playbooks/deploy_qa.md");

/// Return the bundled playbook text.
pub fn bundled_playbook() -> &'static str {
    BUNDLED_PLAYBOOK
}

/// Load playbook text from `path`, or the bundled default when `path` is `None`.
pub fn load_playbook(path: Option<&Path>) -> Result<String> {
    match path {
        None => Ok(bundled_playbook().to_string()),
        Some(path) => std::fs::read_to_string(path).map_err(|e| {
            Error::config(format!(
                "Failed to read deploy_qa playbook '{}': {e}",
                path.display()
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_playbook_mentions_non_goals() {
        let text = bundled_playbook();
        assert!(text.contains("[regression]"));
        assert!(text.contains("DEPLOY_QA_VERDICT"));
        assert!(text.contains("Do **not** open fix PRs"));
    }
}
