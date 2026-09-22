//! Repository index for file-based searching.
//!
//! Provides a searchable index of repositories discovered from known organizations.
//! This enables issue-to-repository inference by matching file paths and names.
//!
//! Note: GitHub/GitLab-specific index building functions live in the root crate's
//! `repo::index` module since they depend on SCM provider types.

pub use claudear_core::types::{IndexedRepo, RepoIndex};

use super::GitOps;
use claudear_core::error::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Build a repo index by scanning filesystem paths for repos from known orgs.
pub fn build_repo_index(known_orgs: &[String], paths: &[String]) -> Result<RepoIndex> {
    let mut index = RepoIndex::new();
    let orgs_set: HashSet<_> = known_orgs.iter().map(|s| s.to_lowercase()).collect();

    for path_str in paths {
        let path = expand_path(path_str);
        if !path.exists() {
            tracing::warn!(path = %path.display(), "Auto-discover path does not exist");
            continue;
        }

        tracing::info!(path = %path.display(), "Scanning for repositories");

        for entry in WalkDir::new(&path)
            .max_depth(3)
            .into_iter()
            .filter_entry(|e| !is_hidden(e))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let entry_path = entry.path();

            if entry_path.join(".git").is_dir() {
                if let Some(repo_name) = get_repo_name_from_git(entry_path) {
                    let org = repo_name.split('/').next().unwrap_or("");
                    if orgs_set.contains(&org.to_lowercase()) {
                        tracing::debug!(
                            repo = %repo_name,
                            path = %entry_path.display(),
                            "Found repository from known org"
                        );

                        let default_branch = GitOps::detect_default_branch_sync(entry_path);
                        let mut repo = IndexedRepo::new(&repo_name, entry_path)
                            .with_default_branch(&default_branch);
                        repo = index_files(repo);
                        index.add_repo(repo);
                    }
                }
            }
        }
    }

    tracing::info!(count = index.len(), "Repository index built");

    Ok(index)
}

/// Index files for a repository that was just cloned.
///
/// Removes the repo from the index, scans its files, and re-adds it.
/// Returns the number of files indexed, or None if repo not found.
pub fn index_repo_files(index: &mut RepoIndex, repo_name: &str) -> Option<usize> {
    let repo = index.remove(repo_name)?;
    let indexed_repo = index_files(repo);
    let file_count = indexed_repo.files.len();
    index.add_repo(indexed_repo);
    Some(file_count)
}

/// Expand ~ to home directory.
fn expand_path(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix('~') {
        if let Some(home) = dirs::home_dir() {
            // Strip the leading / if present (e.g., ~/foo -> foo, ~ -> empty)
            let suffix = stripped.strip_prefix('/').unwrap_or(stripped);
            return home.join(suffix);
        }
    }
    PathBuf::from(path)
}

/// Check if a directory entry is hidden.
fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

/// Get repository name from git remote origin.
fn get_repo_name_from_git(path: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_repo_name_from_url(&url)
}

/// Parse repository name (org/repo) from a git URL.
pub fn parse_repo_name_from_url(url: &str) -> Option<String> {
    // Handle SSH URLs: git@github.com:org/repo.git or git@gitlab.com:group/repo.git
    if url.starts_with("git@") {
        let parts: Vec<_> = url.split(':').collect();
        if parts.len() == 2 {
            let repo_part = parts[1].trim_end_matches(".git");
            return Some(repo_part.to_string());
        }
    }

    // Handle HTTPS URLs: https://github.com/org/repo.git or https://gitlab.com/group/repo.git
    let url_trimmed = url.trim_end_matches(".git");
    let parts: Vec<_> = url_trimmed.split('/').collect();
    if parts.len() >= 2 {
        let org = parts[parts.len() - 2];
        let repo = parts[parts.len() - 1];
        if !org.is_empty() && !repo.is_empty() {
            return Some(format!("{}/{}", org, repo));
        }
    }

    None
}

/// Index all files in a repository.
pub fn index_files(mut repo: IndexedRepo) -> IndexedRepo {
    let mut files = Vec::new();

    for entry in WalkDir::new(&repo.path)
        .into_iter()
        .filter_entry(|e| {
            // Skip hidden directories and common non-source directories
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.')
                && name != "node_modules"
                && name != "vendor"
                && name != "target"
                && name != "build"
                && name != "dist"
                && name != "__pycache__"
        })
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            // Store relative path from repo root
            if let Ok(rel_path) = entry.path().strip_prefix(&repo.path) {
                files.push(rel_path.to_string_lossy().to_string());
            }
        }
    }

    repo.files = files;
    repo
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_indexed_repo_new() {
        let repo = IndexedRepo::new("appwrite/cloud", "/path/to/cloud");
        assert_eq!(repo.name, "appwrite/cloud");
        assert_eq!(repo.scm_url, "https://github.com/appwrite/cloud");
        assert_eq!(repo.default_branch, "main");
    }

    #[test]
    fn test_indexed_repo_with_scm_url() {
        let repo =
            IndexedRepo::new("test/repo", "/path").with_scm_url("https://gitlab.com/test/repo");
        assert_eq!(repo.scm_url, "https://gitlab.com/test/repo");
    }

    #[test]
    fn test_indexed_repo_has_file() {
        let mut repo = IndexedRepo::new("test/repo", "/path");
        repo.files = vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "README.md".to_string(),
        ];

        assert!(repo.has_file("main.rs"));
        assert!(repo.has_file("src/main.rs"));
        assert!(repo.has_file("README.md"));
        assert!(!repo.has_file("nonexistent.txt"));
    }

    #[test]
    fn test_expand_path_home() {
        let expanded = expand_path("~/test");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expanded, home.join("test"));
        }
    }

    #[test]
    fn test_expand_path_absolute() {
        let expanded = expand_path("/absolute/path");
        assert_eq!(expanded, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_parse_repo_name_ssh() {
        let name = parse_repo_name_from_url("git@github.com:appwrite/cloud.git");
        assert_eq!(name, Some("appwrite/cloud".to_string()));
    }

    #[test]
    fn test_parse_repo_name_https() {
        let name = parse_repo_name_from_url("https://github.com/appwrite/cloud.git");
        assert_eq!(name, Some("appwrite/cloud".to_string()));
    }

    #[test]
    fn test_is_hidden() {
        use walkdir::WalkDir;
        let temp = TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join(".hidden")).unwrap();
        std::fs::create_dir(temp.path().join("visible")).unwrap();

        for entry in WalkDir::new(temp.path()).max_depth(1) {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy();
            if name == ".hidden" {
                assert!(is_hidden(&entry));
            } else if name == "visible" {
                assert!(!is_hidden(&entry));
            }
        }
    }

    #[test]
    fn test_index_files_skips_hidden_and_build_dirs() {
        let temp = TempDir::new().unwrap();
        let repo_dir = temp.path().join("myrepo");
        std::fs::create_dir(&repo_dir).unwrap();
        std::fs::write(repo_dir.join("visible.rs"), "fn main() {}").unwrap();
        std::fs::create_dir(repo_dir.join(".hidden")).unwrap();
        std::fs::write(repo_dir.join(".hidden/secret.rs"), "// hidden").unwrap();
        std::fs::create_dir(repo_dir.join("node_modules")).unwrap();
        std::fs::write(repo_dir.join("node_modules/pkg.js"), "// npm").unwrap();

        let repo = IndexedRepo::new("test/repo", &repo_dir);
        let indexed = index_files(repo);

        assert_eq!(indexed.files.len(), 1);
        assert!(indexed.files.iter().any(|f| f.contains("visible.rs")));
    }

    #[test]
    fn test_repo_index_index_repo_files_not_found() {
        let mut index = RepoIndex::new();
        let result = index_repo_files(&mut index, "nonexistent/repo");
        assert!(result.is_none());
    }

    #[test]
    fn test_repo_index_index_repo_files_with_real_dir() {
        let temp = TempDir::new().unwrap();
        let repo_dir = temp.path().join("myrepo");
        std::fs::create_dir(&repo_dir).unwrap();
        std::fs::write(repo_dir.join("file1.rs"), "fn main() {}").unwrap();
        std::fs::write(repo_dir.join("file2.rs"), "fn test() {}").unwrap();

        let mut index = RepoIndex::new();
        let repo = IndexedRepo::new("test/repo", &repo_dir);
        index.add_repo(repo);

        let count = index_repo_files(&mut index, "test/repo");
        assert!(count.is_some());
        assert!(count.unwrap() >= 2);

        let repo = index.get("test/repo").unwrap();
        assert!(repo.files.len() >= 2);
    }
}
