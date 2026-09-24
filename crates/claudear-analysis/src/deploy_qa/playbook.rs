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
        Some(path) => std::fs::read_to_string(path).map_err(|error| {
            Error::config(format!(
                "Failed to read deploy_qa playbook '{}': {error}",
                path.display()
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_playbook_defaults_to_bundled() {
        assert_eq!(load_playbook(None).unwrap(), bundled_playbook());
    }

    #[test]
    fn load_playbook_reads_custom_path() {
        let contents = "custom live-QA procedure";
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();

        assert_eq!(load_playbook(Some(file.path())).unwrap(), contents);
    }

    #[test]
    fn load_playbook_missing_path_errors() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.md");

        assert!(load_playbook(Some(&missing)).is_err());
    }
}
