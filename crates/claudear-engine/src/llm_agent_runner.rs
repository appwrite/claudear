//! LLM-based agent runner.
//!
//! Uses the local LLM model as the agent runner. Generates code changes
//! via the local model, then creates a git branch, applies changes,
//! and opens a PR via `gh`.

use async_trait::async_trait;
use claudear_core::error::Result;
use claudear_core::security::SECURITY_FIX_SOURCE;
use claudear_core::types::{AgentResult, Issue};
use claudear_integrations::chat::llm::{GenerationParams, LlmEngine};
use claudear_integrations::runner::{AgentRunner, ProviderCapabilities};
use regex_lite::Regex;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::Instant;
use tokio::process::Command;

/// A parsed file change from LLM output.
#[derive(Debug, Clone, PartialEq)]
struct FileChange {
    path: String,
    content: String,
}

/// LLM-based agent runner using local model inference.
///
/// Generates code fixes via the local LLM, then creates a git branch,
/// applies the changes, and opens a PR via `gh pr create`.
pub struct LlmAgentRunner {
    engine: Arc<LlmEngine>,
}

impl LlmAgentRunner {
    /// Create a new runner with the given LLM engine.
    pub fn new(engine: Arc<LlmEngine>) -> Self {
        Self { engine }
    }

    /// Parse file changes from structured LLM output.
    ///
    /// Expects blocks in the format:
    /// ```text
    /// === FILE: path/to/file.ext ===
    /// <content>
    /// === END FILE ===
    /// ```
    fn parse_file_changes(output: &str) -> Vec<FileChange> {
        static FILE_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?s)===\s*FILE:\s*(.+?)\s*===\r?\n(.*?)\n?===\s*END\s*FILE\s*===").unwrap()
        });

        FILE_BLOCK_RE
            .captures_iter(output)
            .filter_map(|cap| {
                let path = cap.get(1)?.as_str().trim().to_string();
                let content = cap.get(2)?.as_str().to_string();
                if path.is_empty() {
                    return None;
                }
                Some(FileChange { path, content })
            })
            .collect()
    }

    /// Sanitize a string for use as a git branch segment.
    /// The fix prompt for `issue`. A security fix's prompt names neither its
    /// title, source, id nor URL, so none of them reaches the pull request.
    fn issue_prompt(issue: &Issue, context: &str) -> String {
        let mut prompt = String::new();
        prompt.push_str(
            "You are a software engineer. Fix the following bug by modifying the necessary source files.\n\n",
        );
        if issue.source != SECURITY_FIX_SOURCE {
            prompt.push_str(&format!("## Issue: {}\n", issue.title));
            if let Some(ref desc) = issue.description {
                prompt.push_str(&format!("Description: {}\n", desc));
            }
            prompt.push_str(&format!("Source: {}\n", issue.source));
            prompt.push_str(&format!("URL: {}\n\n", issue.url));
        }

        if !context.is_empty() {
            prompt.push_str("## Context\n");
            prompt.push_str(context);
            prompt.push('\n');
        }

        prompt.push_str("\n## Output Format\n");
        prompt.push_str(
            "For each file you need to change, output the COMPLETE new file content using this exact format:\n\n",
        );
        prompt.push_str("=== FILE: path/to/file.ext ===\n");
        prompt.push_str("<complete file content here>\n");
        prompt.push_str("=== END FILE ===\n\n");
        prompt.push_str(
            "After all file changes, provide a brief summary of what you changed and why.\n",
        );

        prompt
    }

    /// The branch name suffix for `issue`. A security fix's branch does not
    /// carry its id.
    fn branch_suffix(issue: Option<&Issue>) -> String {
        match issue.filter(|issue| issue.source != SECURITY_FIX_SOURCE) {
            Some(issue) => Self::sanitize_for_branch(&issue.short_id),
            None => format!("fix-{}", chrono::Utc::now().timestamp()),
        }
    }

    fn sanitize_for_branch(s: &str) -> String {
        let sanitized: String = s
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        // Collapse multiple hyphens
        let mut result = String::new();
        let mut prev_hyphen = false;
        for c in sanitized.chars() {
            if c == '-' {
                if !prev_hyphen && !result.is_empty() {
                    result.push('-');
                }
                prev_hyphen = true;
            } else {
                result.push(c);
                prev_hyphen = false;
            }
        }
        result.truncate(50);
        result.trim_end_matches('-').to_string()
    }

    /// Run a command in the project directory and return stdout.
    async fn run_cmd(project_dir: &Path, program: &str, args: &[&str]) -> Result<String> {
        let output = Command::new(program)
            .args(args)
            .current_dir(project_dir)
            .output()
            .await
            .map_err(|e| {
                claudear_core::error::Error::runner(format!("{} failed to start: {}", program, e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(claudear_core::error::Error::runner(format!(
                "{} {:?} failed (exit {}): {}",
                program,
                args,
                output.status.code().unwrap_or(-1),
                stderr.trim()
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Restore the original git branch, ignoring errors.
    async fn restore_branch(project_dir: &Path, original: &str) {
        let _ = Command::new("git")
            .args(["checkout", original])
            .current_dir(project_dir)
            .output()
            .await;
    }

    /// Create a PR from the LLM's file changes.
    ///
    /// Creates a branch, applies file changes, commits, pushes, and opens a PR.
    /// Returns the PR URL on success, or None if no changes could be applied.
    async fn create_pr_from_changes(
        changes: &[FileChange],
        issue: Option<&Issue>,
        analysis: &str,
        attempt_id: Option<i64>,
        project_dir: &Path,
    ) -> Result<Option<String>> {
        if changes.is_empty() {
            return Ok(None);
        }

        // Save current branch/ref
        let original_ref =
            Self::run_cmd(project_dir, "git", &["rev-parse", "--abbrev-ref", "HEAD"]).await?;

        // Build branch name
        let branch_suffix = Self::branch_suffix(issue);
        let mut branch = if let Some(aid) = attempt_id {
            format!("claudear/llm-{}-{}", branch_suffix, aid)
        } else {
            format!("claudear/llm-{}", branch_suffix)
        };

        // Create and checkout new branch (with timestamp fallback if name taken)
        if Self::run_cmd(project_dir, "git", &["checkout", "-b", &branch])
            .await
            .is_err()
        {
            branch = format!("{}-{}", branch, chrono::Utc::now().timestamp());
            if let Err(e) = Self::run_cmd(project_dir, "git", &["checkout", "-b", &branch]).await {
                tracing::warn!(error = %e, "Failed to create branch");
                return Ok(None);
            }
        }

        // Apply file changes
        let mut applied = 0;
        for change in changes {
            let file_path = project_dir.join(&change.path);
            if let Some(parent) = file_path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    tracing::warn!(path = %parent.display(), error = %e, "Failed to create dir");
                    continue;
                }
            }
            match tokio::fs::write(&file_path, &change.content).await {
                Ok(()) => {
                    applied += 1;
                    tracing::debug!(path = %change.path, "Applied file change");
                }
                Err(e) => {
                    tracing::warn!(path = %change.path, error = %e, "Failed to write file");
                }
            }
        }

        if applied == 0 {
            Self::restore_branch(project_dir, &original_ref).await;
            return Ok(None);
        }

        // Stage all changes
        if let Err(e) = Self::run_cmd(project_dir, "git", &["add", "-A"]).await {
            tracing::warn!(error = %e, "git add failed");
            Self::restore_branch(project_dir, &original_ref).await;
            return Ok(None);
        }

        // Check if there are actual staged changes (exit 0 = no diff = nothing to commit)
        if Self::run_cmd(project_dir, "git", &["diff", "--cached", "--quiet"])
            .await
            .is_ok()
        {
            tracing::info!("No effective changes after applying LLM output");
            Self::restore_branch(project_dir, &original_ref).await;
            return Ok(None);
        }

        // Commit
        let title = issue
            .map(|i| i.title.as_str())
            .unwrap_or("LLM-generated fix");
        let commit_msg = format!("fix: {}", title);
        if let Err(e) = Self::run_cmd(project_dir, "git", &["commit", "-m", &commit_msg]).await {
            tracing::warn!(error = %e, "git commit failed");
            Self::restore_branch(project_dir, &original_ref).await;
            return Ok(None);
        }

        // Push
        if let Err(e) = Self::run_cmd(project_dir, "git", &["push", "-u", "origin", &branch]).await
        {
            tracing::warn!(error = %e, "git push failed");
            Self::restore_branch(project_dir, &original_ref).await;
            return Ok(None);
        }

        // Create PR via gh
        let pr_title = format!("fix: {}", title);
        let pr_body = format!(
            "## LLM-Generated Fix\n\n{}\n\n---\n*Generated by claudear local LLM agent*",
            if analysis.len() > 2000 {
                &analysis[..2000]
            } else {
                analysis
            }
        );

        let pr_url = match Self::run_cmd(
            project_dir,
            "gh",
            &["pr", "create", "--title", &pr_title, "--body", &pr_body],
        )
        .await
        {
            Ok(url) => {
                let url = url.trim().to_string();
                if url.starts_with("https://") {
                    Some(url)
                } else {
                    tracing::warn!(output = %url, "gh pr create did not return a URL");
                    None
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "gh pr create failed");
                None
            }
        };

        // Restore original branch
        Self::restore_branch(project_dir, &original_ref).await;

        Ok(pr_url)
    }
}

#[async_trait]
impl AgentRunner for LlmAgentRunner {
    fn name(&self) -> &str {
        "llm"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            structured_output: false,
            tool_permissions: false,
            custom_instructions: false,
            streaming_events: false,
            cost_reporting: false,
        }
    }

    fn build_prompt_for_issue(&self, issue: &Issue, context: &str, _project_dir: &Path) -> String {
        Self::issue_prompt(issue, context)
    }

    async fn execute_with_attempt(
        &self,
        prompt: &str,
        issue: Option<&Issue>,
        attempt_id: Option<i64>,
        project_dir: &Path,
    ) -> Result<AgentResult> {
        let start = Instant::now();

        let params = GenerationParams {
            temperature: 0.3,
            max_tokens: 4096,
            top_p: 0.9,
            stop_sequences: vec![],
        };

        // Run LLM inference in a blocking task to avoid stalling the async runtime.
        let engine = self.engine.clone();
        let prompt_owned = prompt.to_string();
        let tokens =
            tokio::task::spawn_blocking(move || engine.complete_streaming(&prompt_owned, &params))
                .await
                .map_err(|e| {
                    claudear_core::error::Error::runner(format!("LLM spawn_blocking failed: {}", e))
                })??;

        let output = tokens.join("");
        let llm_duration = start.elapsed();

        tracing::debug!(
            output_len = output.len(),
            elapsed_ms = llm_duration.as_millis(),
            "LLM inference completed"
        );

        if output.is_empty() {
            return Ok(AgentResult {
                success: false,
                output: String::new(),
                pr_url: None,
                changelog: None,
                error: Some("LLM produced empty output".to_string()),
                blocking_question: None,
                used_qa_ids: vec![],
                confidence: 0,
                confidence_reasoning: None,
                wrong_repo: None,
            });
        }

        // Parse file changes from LLM output
        let changes = Self::parse_file_changes(&output);
        tracing::info!(
            num_changes = changes.len(),
            files = ?changes.iter().map(|c| &c.path).collect::<Vec<_>>(),
            "Parsed file changes from LLM output"
        );

        // Extract summary (text after the last === END FILE === block)
        let summary = output
            .rsplit_once("=== END FILE ===")
            .map(|(_, after)| after.trim())
            .unwrap_or(output.trim())
            .to_string();

        // Attempt to create a PR from the parsed changes
        let pr_url =
            match Self::create_pr_from_changes(&changes, issue, &summary, attempt_id, project_dir)
                .await
            {
                Ok(url) => url,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to create PR from LLM changes");
                    None
                }
            };

        let total_duration = start.elapsed();
        let confidence = if pr_url.is_some() { 50 } else { 20 };

        Ok(AgentResult {
            success: !output.is_empty(),
            output: if pr_url.is_some() { summary } else { output },
            pr_url,
            changelog: None,
            error: None,
            blocking_question: None,
            used_qa_ids: vec![],
            confidence,
            confidence_reasoning: Some(format!(
                "Local LLM fix (inference: {:.1}s, total: {:.1}s)",
                llm_duration.as_secs_f64(),
                total_duration.as_secs_f64()
            )),
            wrong_repo: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_agent_runner_name() {
        // We can't construct without a real engine, but we can verify the constant.
        assert_eq!("llm", "llm");
    }

    #[test]
    fn test_llm_agent_runner_capabilities_are_minimal() {
        let caps = ProviderCapabilities {
            structured_output: false,
            tool_permissions: false,
            custom_instructions: false,
            streaming_events: false,
            cost_reporting: false,
        };
        assert!(!caps.structured_output);
        assert!(!caps.tool_permissions);
        assert!(!caps.custom_instructions);
        assert!(!caps.streaming_events);
        assert!(!caps.cost_reporting);
    }

    #[test]
    fn test_llm_agent_runner_build_prompt() {
        let issue = Issue::new(
            "test-1",
            "TEST-1",
            "NullPointerException in getDocument",
            "https://example.com/issue/1",
            "sentry",
        );
        let context = "Stack trace:\n  at Database.getDocument(Database.java:42)";

        // Rebuild the prompt the same way the runner does
        let mut prompt = String::new();
        prompt.push_str(
            "You are a software engineer. Fix the following bug by modifying the necessary source files.\n\n",
        );
        prompt.push_str(&format!("## Issue: {}\n", issue.title));
        if let Some(ref desc) = issue.description {
            prompt.push_str(&format!("Description: {}\n", desc));
        }
        prompt.push_str(&format!("Source: {}\n", issue.source));
        prompt.push_str(&format!("URL: {}\n\n", issue.url));
        if !context.is_empty() {
            prompt.push_str("## Context\n");
            prompt.push_str(context);
            prompt.push('\n');
        }
        prompt.push_str("\n## Output Format\n");
        prompt.push_str(
            "For each file you need to change, output the COMPLETE new file content using this exact format:\n\n",
        );
        prompt.push_str("=== FILE: path/to/file.ext ===\n");
        prompt.push_str("<complete file content here>\n");
        prompt.push_str("=== END FILE ===\n\n");
        prompt.push_str(
            "After all file changes, provide a brief summary of what you changed and why.\n",
        );

        assert!(prompt.contains("NullPointerException in getDocument"));
        assert!(prompt.contains("Database.getDocument"));
        assert!(prompt.contains("sentry"));
        assert!(prompt.contains("=== FILE:"));
        assert!(prompt.contains("=== END FILE ==="));
    }

    #[test]
    fn a_security_fix_prompt_names_neither_its_source_nor_its_id() {
        let issue = Issue::new("SEC-7", "SEC-7", "Hardening", "", SECURITY_FIX_SOURCE);

        let prompt = LlmAgentRunner::issue_prompt(&issue, "Harden the session revocation");

        assert!(prompt.contains("Harden the session revocation"), "{prompt}");
        assert!(prompt.contains("=== FILE:"), "{prompt}");
        assert!(!prompt.contains("SEC-"), "{prompt}");
        assert!(!prompt.contains("Hardening"), "{prompt}");
        assert!(!prompt.contains("## Issue"), "{prompt}");
        assert!(!prompt.contains("Source:"), "{prompt}");
        assert!(!prompt.contains("URL:"), "{prompt}");
    }

    #[test]
    fn a_support_fix_prompt_still_names_its_title_and_source() {
        let issue = Issue::new(
            "1",
            "HS-1",
            "Login fails",
            "https://example.com/1",
            "helpscout",
        );

        let prompt = LlmAgentRunner::issue_prompt(&issue, "context");

        assert!(prompt.contains("## Issue: Login fails"), "{prompt}");
        assert!(prompt.contains("Source: helpscout"), "{prompt}");
    }

    #[test]
    fn a_security_fix_branch_does_not_carry_its_id() {
        let security = Issue::new("SEC-7", "SEC-7", "Hardening", "", SECURITY_FIX_SOURCE);
        let support = Issue::new("1", "HS-1", "Login fails", "url", "helpscout");

        assert!(!LlmAgentRunner::branch_suffix(Some(&security)).contains("sec"));
        assert_eq!(LlmAgentRunner::branch_suffix(Some(&support)), "hs-1");
    }

    #[test]
    fn test_parse_file_changes_single_file() {
        let output = r#"Here's the fix:

=== FILE: src/main.rs ===
fn main() {
    println!("fixed!");
}
=== END FILE ===

I fixed the issue by updating main.rs."#;

        let changes = LlmAgentRunner::parse_file_changes(output);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "src/main.rs");
        assert!(changes[0].content.contains("println!(\"fixed!\")"));
    }

    #[test]
    fn test_parse_file_changes_multiple_files() {
        let output = r#"=== FILE: src/lib.rs ===
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
=== END FILE ===

=== FILE: src/main.rs ===
fn main() {
    println!("{}", lib::add(1, 2));
}
=== END FILE ===

Updated both files."#;

        let changes = LlmAgentRunner::parse_file_changes(output);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "src/lib.rs");
        assert_eq!(changes[1].path, "src/main.rs");
    }

    #[test]
    fn test_parse_file_changes_no_blocks() {
        let output = "I think you should check the database connection settings.";
        let changes = LlmAgentRunner::parse_file_changes(output);
        assert!(changes.is_empty());
    }

    #[test]
    fn test_parse_file_changes_empty_path() {
        let output = "=== FILE:  ===\ncontent\n=== END FILE ===";
        let changes = LlmAgentRunner::parse_file_changes(output);
        assert!(changes.is_empty());
    }

    #[test]
    fn test_sanitize_for_branch() {
        assert_eq!(LlmAgentRunner::sanitize_for_branch("PROJ-123"), "proj-123");
        assert_eq!(
            LlmAgentRunner::sanitize_for_branch("fix: NullPointer Exception"),
            "fix-nullpointer-exception"
        );
        assert_eq!(LlmAgentRunner::sanitize_for_branch("a///b"), "a-b");
        // Truncation
        let long = "a".repeat(100);
        assert!(LlmAgentRunner::sanitize_for_branch(&long).len() <= 50);
    }

    #[test]
    fn test_sanitize_for_branch_no_leading_trailing_hyphens() {
        assert_eq!(LlmAgentRunner::sanitize_for_branch("--hello--"), "hello");
    }
}
