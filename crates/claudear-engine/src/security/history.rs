//! Fixed, read-only git history queries over a synced repository. Every
//! agent-supplied argument is validated, and git runs without a shell and
//! with a cleared environment.

use chrono::{DateTime, Utc};
use claudear_core::error::{Error, Result};
use regex_lite::Regex;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::process::Command;

const MAX_PATHS: usize = 10;
const MAX_PATH_CHARS: usize = 200;
const MAX_TERMS: usize = 5;
const MIN_TERM_CHARS: usize = 3;
const MAX_TERM_CHARS: usize = 100;
const MAX_OUTPUT_LINES: usize = 200;
const PATH_LOG_LIMIT: &str = "40";
const TERM_LOG_LIMIT: &str = "20";
const LOG_FORMAT: &str = "--format=%H%x09%cI%x09%s";
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const ENVIRONMENT_ALLOWLIST: &[&str] = &["PATH", "HOME"];

static SHA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{7,40}$").expect("valid sha pattern"));
static BRANCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._/-]{1,100}$").expect("valid branch pattern"));

pub(crate) fn validate_sha(sha: &str) -> Result<()> {
    if SHA.is_match(sha) {
        Ok(())
    } else {
        Err(Error::Other(format!("invalid commit sha: {sha:?}")))
    }
}

pub(crate) fn validate_branch(branch: &str) -> Result<()> {
    if BRANCH.is_match(branch) && !branch.starts_with('-') && !branch.contains("..") {
        Ok(())
    } else {
        Err(Error::Other(format!("invalid default branch: {branch:?}")))
    }
}

/// Keep at most ten agent-supplied paths that are relative, plain and
/// resolve inside `project_dir`.
pub(crate) fn validate_paths(project_dir: &Path, paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|path| is_safe_path(project_dir, path))
        .take(MAX_PATHS)
        .cloned()
        .collect()
}

fn is_safe_path(project_dir: &Path, path: &str) -> bool {
    if path.is_empty()
        || path.chars().count() > MAX_PATH_CHARS
        || path.starts_with('-')
        || path.starts_with('/')
        || path.contains("..")
        || path.chars().any(char::is_control)
    {
        return false;
    }
    let relative = Path::new(path);
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return false;
    }
    resolves_inside(project_dir, &project_dir.join(relative))
}

fn resolves_inside(project_dir: &Path, candidate: &Path) -> bool {
    let Ok(root) = project_dir.canonicalize() else {
        return false;
    };
    let mut existing = candidate;
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            return resolved.starts_with(&root);
        }
        match existing.parent() {
            Some(parent) => existing = parent,
            None => return false,
        }
    }
}

/// Keep at most five agent-supplied search terms of 3 to 100 plain characters.
pub(crate) fn validate_terms(terms: &[String]) -> Vec<String> {
    terms
        .iter()
        .filter(|term| {
            let length = term.chars().count();
            (MIN_TERM_CHARS..=MAX_TERM_CHARS).contains(&length)
                && !term.starts_with('-')
                && !term.chars().any(char::is_control)
        })
        .take(MAX_TERMS)
        .cloned()
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Commit {
    pub sha: String,
    pub committed_at: String,
    pub subject: String,
}

pub(crate) struct History {
    project_dir: PathBuf,
    remote_branch: String,
}

impl History {
    pub(crate) fn new(project_dir: &Path, default_branch: &str) -> Result<Self> {
        validate_branch(default_branch)?;
        let project_dir = project_dir
            .canonicalize()
            .map_err(|error| Error::io(format!("repository directory unavailable: {error}")))?;
        Ok(Self {
            project_dir,
            remote_branch: format!("origin/{default_branch}"),
        })
    }

    pub(crate) fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    /// Recent commits on the default branch that touched any of `paths`
    /// (already validated with [`validate_paths`]).
    pub(crate) async fn commits_touching(&self, paths: &[String]) -> Result<Vec<Commit>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut arguments = vec![
            "log".to_string(),
            "--no-color".to_string(),
            LOG_FORMAT.to_string(),
            "-n".to_string(),
            PATH_LOG_LIMIT.to_string(),
            self.remote_branch.clone(),
            "--".to_string(),
        ];
        arguments.extend(paths.iter().cloned());
        let output = self.git(&arguments).await?;
        Ok(parse_log(&output.stdout))
    }

    /// Recent commits on the default branch whose diff adds or removes `term`
    /// (already validated with [`validate_terms`]).
    pub(crate) async fn commits_mentioning(&self, term: &str) -> Result<Vec<Commit>> {
        let arguments = [
            "log".to_string(),
            "--no-color".to_string(),
            LOG_FORMAT.to_string(),
            "-n".to_string(),
            TERM_LOG_LIMIT.to_string(),
            format!("-S{term}"),
            self.remote_branch.clone(),
        ];
        let output = self.git(&arguments).await?;
        Ok(parse_log(&output.stdout))
    }

    /// The full sha and committer date of `sha`.
    pub(crate) async fn commit_date(&self, sha: &str) -> Result<(String, DateTime<Utc>)> {
        validate_sha(sha)?;
        let arguments = [
            "show".to_string(),
            "-s".to_string(),
            "--format=%H%x00%cI".to_string(),
            sha.to_string(),
        ];
        let output = self.git(&arguments).await?;
        let line = output.stdout.lines().next().unwrap_or_default();
        let (full, date) = line
            .split_once('\0')
            .ok_or_else(|| Error::git(format!("unexpected git show output for {sha}")))?;
        validate_sha(full)?;
        let committed_at = DateTime::parse_from_rfc3339(date.trim())
            .map_err(|error| Error::git(format!("unparseable commit date for {sha}: {error}")))?
            .with_timezone(&Utc);
        Ok((full.to_string(), committed_at))
    }

    /// Whether `sha` is reachable from the default branch.
    pub(crate) async fn is_ancestor(&self, sha: &str) -> Result<bool> {
        validate_sha(sha)?;
        let arguments = [
            "merge-base".to_string(),
            "--is-ancestor".to_string(),
            sha.to_string(),
            self.remote_branch.clone(),
        ];
        let output = self.run(&arguments).await?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(Error::git(format!(
                "git merge-base failed for {sha}: {}",
                capped(&String::from_utf8_lossy(&output.stderr))
            ))),
        }
    }

    async fn git(&self, arguments: &[String]) -> Result<GitOutput> {
        let output = self.run(arguments).await?;
        if !output.status.success() {
            return Err(Error::git(format!(
                "git {} failed: {}",
                arguments.first().map(String::as_str).unwrap_or_default(),
                capped(&String::from_utf8_lossy(&output.stderr))
            )));
        }
        Ok(GitOutput {
            stdout: capped(&String::from_utf8_lossy(&output.stdout)),
        })
    }

    async fn run(&self, arguments: &[String]) -> Result<std::process::Output> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.project_dir)
            .args(arguments)
            .env_clear()
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for key in ENVIRONMENT_ALLOWLIST {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        tokio::time::timeout(GIT_TIMEOUT, command.output())
            .await
            .map_err(|_| Error::git("git history query timed out"))?
            .map_err(|error| Error::io(format!("failed to run git: {error}")))
    }
}

struct GitOutput {
    stdout: String,
}

fn capped(text: &str) -> String {
    text.lines()
        .take(MAX_OUTPUT_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_log(output: &str) -> Vec<Commit> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let sha = fields.next()?;
            let committed_at = fields.next()?;
            let subject = fields.next().unwrap_or_default();
            SHA.is_match(sha).then(|| Commit {
                sha: sha.to_string(),
                committed_at: committed_at.to_string(),
                subject: subject.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    /// A clone of a local bare origin, so `origin/main` exists offline.
    pub(crate) struct Repository {
        pub directory: tempfile::TempDir,
        pub clone: PathBuf,
        pub origin: PathBuf,
    }

    fn git(directory: &Path, arguments: &[&str], date: Option<&str>) -> String {
        let mut command = StdCommand::new("git");
        command
            .arg("-C")
            .arg(directory)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        if let Some(date) = date {
            command
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date);
        }
        let output = command.output().expect("git runs");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    impl Repository {
        pub(crate) fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let origin = directory.path().join("origin.git");
            let work = directory.path().join("work");
            std::fs::create_dir_all(&work).unwrap();
            git(
                directory.path(),
                &["init", "--bare", "-b", "main", "origin.git"],
                None,
            );
            git(&work, &["init", "-b", "main"], None);
            git(
                &work,
                &["remote", "add", "origin", origin.to_str().unwrap()],
                None,
            );
            std::fs::create_dir_all(work.join("src")).unwrap();
            std::fs::write(work.join("src/auth.php"), "<?php\n// initial\n").unwrap();
            git(&work, &["add", "."], None);
            git(
                &work,
                &["commit", "-m", "initial import"],
                Some("2026-01-01T00:00:00Z"),
            );
            git(&work, &["push", "origin", "main"], None);
            let clone = directory.path().join("clone");
            git(
                directory.path(),
                &["clone", origin.to_str().unwrap(), "clone"],
                None,
            );
            Self {
                directory,
                clone,
                origin,
            }
        }

        fn work(&self) -> PathBuf {
            self.directory.path().join("work")
        }

        /// Commit a change to `src/auth.php` on origin's main branch at `date`
        /// and return its full sha.
        pub(crate) fn commit_fix(&self, message: &str, marker: &str, date: &str) -> String {
            let work = self.work();
            let path = work.join("src/auth.php");
            let mut contents = std::fs::read_to_string(&path).unwrap();
            contents.push_str(&format!("// {marker}\n"));
            std::fs::write(&path, contents).unwrap();
            git(&work, &["commit", "-am", message], Some(date));
            git(&work, &["push", "origin", "main"], None);
            git(&work, &["rev-parse", "HEAD"], None)
        }

        /// Commit on a side branch that never reaches main.
        pub(crate) fn commit_unmerged(&self, date: &str) -> String {
            let work = self.work();
            git(&work, &["checkout", "-b", "side"], None);
            std::fs::write(work.join("side.txt"), "side\n").unwrap();
            git(&work, &["add", "."], None);
            git(&work, &["commit", "-m", "side change"], Some(date));
            git(&work, &["push", "origin", "side"], None);
            let sha = git(&work, &["rev-parse", "HEAD"], None);
            git(&work, &["checkout", "main"], None);
            sha
        }

        pub(crate) fn fetch(&self) {
            git(&self.clone, &["fetch", "origin"], None);
        }
    }

    #[test]
    fn sha_validation_accepts_only_short_or_full_lowercase_hex() {
        for valid in ["abc1234", "0123456789abcdef0123456789abcdef01234567"] {
            assert!(validate_sha(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "-",
            "--output",
            "abc123",
            "ABC1234",
            "abc1234 ",
            "HEAD",
            "abc1234..def5678",
            "0123456789abcdef0123456789abcdef012345678",
        ] {
            assert!(validate_sha(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn branch_validation_rejects_options_and_traversal() {
        for valid in ["main", "1.9.x", "release/2026"] {
            assert!(validate_branch(valid).is_ok(), "{valid}");
        }
        for invalid in ["-", "--output=x", "main..evil", "main branch", "", "a\nb"] {
            assert!(validate_branch(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn path_validation_keeps_only_plain_relative_paths_inside_the_repository() {
        let repository = Repository::new();
        let outside = repository.directory.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, repository.clone.join("escape")).unwrap();
        let paths: Vec<String> = [
            "src/auth.php",
            "src/deleted/file.php",
            "-",
            "--output=/tmp/x",
            "../outside",
            "src/../../outside",
            "/etc/hosts",
            "src/a\u{0}b",
            "src/\nnew",
            "escape/inside.txt",
            "",
        ]
        .iter()
        .map(|path| path.to_string())
        .collect();
        assert_eq!(
            validate_paths(&repository.clone, &paths),
            vec![
                "src/auth.php".to_string(),
                "src/deleted/file.php".to_string()
            ]
        );
        let long = "a/".repeat(101);
        assert!(validate_paths(&repository.clone, &[long]).is_empty());
        let many: Vec<String> = (0..12).map(|index| format!("src/{index}.php")).collect();
        assert_eq!(validate_paths(&repository.clone, &many).len(), MAX_PATHS);
    }

    #[test]
    fn term_validation_bounds_length_count_and_characters() {
        let terms: Vec<String> = ["ab", "validateSession", "-", "--output", "a\u{7}bc", "okay"]
            .iter()
            .map(|term| term.to_string())
            .collect();
        assert_eq!(
            validate_terms(&terms),
            vec!["validateSession".to_string(), "okay".to_string()]
        );
        assert!(validate_terms(&["x".repeat(101)]).is_empty());
        let many: Vec<String> = (0..8).map(|index| format!("term{index}")).collect();
        assert_eq!(validate_terms(&many).len(), MAX_TERMS);
    }

    #[tokio::test]
    async fn history_queries_read_the_default_branch() {
        let repository = Repository::new();
        let fixed = repository.commit_fix(
            "harden session validation",
            "validateSessionToken",
            "2026-03-01T12:00:00Z",
        );
        let side = repository.commit_unmerged("2026-03-02T12:00:00Z");
        repository.fetch();
        let history = History::new(&repository.clone, "main").unwrap();

        let touching = history
            .commits_touching(&["src/auth.php".to_string()])
            .await
            .unwrap();
        assert_eq!(touching.len(), 2);
        assert_eq!(touching[0].sha, fixed);
        assert_eq!(touching[0].subject, "harden session validation");

        let mentioning = history
            .commits_mentioning("validateSessionToken")
            .await
            .unwrap();
        assert_eq!(mentioning.len(), 1);
        assert_eq!(mentioning[0].sha, fixed);

        let (full, date) = history.commit_date(&fixed[..10]).await.unwrap();
        assert_eq!(full, fixed);
        assert_eq!(date.to_rfc3339(), "2026-03-01T12:00:00+00:00");

        assert!(history.is_ancestor(&fixed).await.unwrap());
        assert!(!history.is_ancestor(&side).await.unwrap());
        assert!(history.is_ancestor("-").await.is_err());
        assert!(history.commit_date("--output").await.is_err());
    }
}
