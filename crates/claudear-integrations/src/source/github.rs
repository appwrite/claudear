//! GitHub Issues source adapter.

use super::IssueSource;
use crate::github::GitHubClient;
use crate::github::GitHubIssue;
use abnegate_http::HttpClient;
use async_trait::async_trait;
use claudear_config::config::GitHubConfig;
use claudear_core::error::Error;
use claudear_core::error::Result;
use claudear_core::types::Issue;
use claudear_core::types::IssueStatus;
use claudear_core::types::MatchPriority;
use claudear_core::types::MatchResult;

/// GitHub Issues source.
///
/// Fetches issues from GitHub repositories and maps them to the unified Issue type.
/// Source name is `"github_issues"` to avoid collision with the `"github"` ScmProvider.
pub struct GitHubSource<H: HttpClient = abnegate_http::ReqwestHttpClient> {
    client: GitHubClient<H>,
    config: GitHubConfig,
}

impl GitHubSource {
    /// Create a new GitHub Issues source with the default HTTP client.
    pub fn new(config: GitHubConfig) -> Self {
        let client = GitHubClient::new(config.clone());
        Self { client, config }
    }
}

impl<H: HttpClient> GitHubSource<H> {
    /// Create a new GitHub Issues source with a custom HTTP client.
    pub fn with_http_client(config: GitHubConfig, http: H) -> Self {
        let client = GitHubClient::with_http_client(config.clone(), http);
        Self { client, config }
    }

    /// Map a GitHub API issue to the unified Issue type.
    ///
    /// Issue ID format: `owner:repo#number` (e.g., `myorg:myrepo#42`).
    /// Uses `:` instead of `/` because `validate_issue_id()` rejects forward slashes.
    fn map_issue(&self, repo: &str, gh_issue: &GitHubIssue) -> Issue {
        let issue_id = format_issue_id(repo, gh_issue.number);
        let short_id = issue_id.clone();

        let mut issue = Issue::new(
            &issue_id,
            &short_id,
            &gh_issue.title,
            &gh_issue.html_url,
            "github_issues",
        );

        issue.description = gh_issue.body.clone();

        issue.status = match gh_issue.state.as_str() {
            "closed" => IssueStatus::Resolved,
            "open" => IssueStatus::Open,
            _ => IssueStatus::Open,
        };

        let label_names: Vec<&str> = gh_issue.labels.iter().map(|l| l.name.as_str()).collect();
        issue.set_metadata("state", &gh_issue.state);
        issue.set_metadata("labels", label_names.join(", "));
        issue.set_metadata("repo", repo);
        issue.set_metadata("number", gh_issue.number);

        if let Some(ref user) = gh_issue.user {
            issue.set_metadata("author", &user.login);
        }
        if let Some(ref assignee) = gh_issue.assignee {
            issue.set_metadata("assignee", &assignee.login);
        }

        issue
    }
}

/// Format a GitHub issue ID from repo and number.
///
/// Converts `owner/repo` + `42` to `owner:repo#42`.
fn format_issue_id(repo: &str, number: i64) -> String {
    let safe_repo = repo.replace('/', ":");
    format!("{}#{}", safe_repo, number)
}

/// Parse a GitHub issue ID back into (repo, number).
///
/// Converts `owner:repo#42` to `("owner/repo", 42)`.
fn parse_issue_id(issue_id: &str) -> Result<(String, i64)> {
    let (repo_part, number_str) = issue_id.rsplit_once('#').ok_or_else(|| {
        Error::source(
            "github_issues",
            format!(
                "Invalid issue ID format '{}', expected 'owner:repo#number'",
                issue_id
            ),
        )
    })?;

    let number: i64 = number_str.parse().map_err(|_| {
        Error::source(
            "github_issues",
            format!("Invalid issue number '{}', expected integer", number_str),
        )
    })?;

    // Convert owner:repo back to owner/repo
    let repo = repo_part.replace(':', "/");
    Ok((repo, number))
}

/// Build context string for a GitHub issue.
pub(crate) fn format_github_issues_context(issue: &Issue) -> String {
    let mut context = format!("# GitHub Issue: {}\n\n", issue.short_id);
    context.push_str(&format!("**Title:** {}\n", issue.title));
    context.push_str(&format!("**URL:** {}\n", issue.url));

    if let Some(state) = issue.get_metadata::<String>("state") {
        context.push_str(&format!("**State:** {}\n", state));
    }

    if let Some(labels) = issue.get_metadata::<String>("labels") {
        if !labels.is_empty() {
            context.push_str(&format!("**Labels:** {}\n", labels));
        }
    }

    if let Some(repo) = issue.get_metadata::<String>("repo") {
        context.push_str(&format!("**Repository:** {}\n", repo));
    }

    if let Some(author) = issue.get_metadata::<String>("author") {
        context.push_str(&format!("**Author:** {}\n", author));
    }

    if let Some(assignee) = issue.get_metadata::<String>("assignee") {
        context.push_str(&format!("**Assignee:** {}\n", assignee));
    }

    context.push('\n');

    if let Some(ref description) = issue.description {
        if !description.is_empty() {
            context.push_str("## Description\n");
            context.push_str(description);
            context.push_str("\n\n");
        }
    }

    context
}

/// Check if an issue matches GitHub Issues trigger criteria.
pub(crate) fn github_issues_matches_criteria(config: &GitHubConfig, issue: &Issue) -> MatchResult {
    // Check state against trigger_states
    if !config.trigger_states.is_empty() {
        let state: String = issue.get_metadata("state").unwrap_or_default();
        let state_lower = state.to_lowercase();
        let matches_state = config
            .trigger_states
            .iter()
            .any(|s| s.to_lowercase() == state_lower);
        if !matches_state {
            return MatchResult::not_matched(format!("State '{}' not in trigger_states", state));
        }
    }

    // Check labels against trigger_labels
    if !config.trigger_labels.is_empty() {
        let labels: String = issue.get_metadata("labels").unwrap_or_default();
        let issue_labels: Vec<&str> = if labels.is_empty() {
            vec![]
        } else {
            labels.split(", ").collect()
        };
        let has_label = config
            .trigger_labels
            .iter()
            .any(|tl| issue_labels.iter().any(|il| il == tl));
        if !has_label {
            return MatchResult::not_matched("No matching trigger labels");
        }
    }

    MatchResult::matched(
        format!("GitHub issue {} matches criteria", issue.short_id),
        MatchPriority::Normal,
    )
}

#[async_trait]
impl<H: HttpClient> IssueSource for GitHubSource<H> {
    fn name(&self) -> &str {
        "github_issues"
    }

    fn display_name(&self) -> &str {
        "GitHub Issues"
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        let mut all_issues = Vec::new();

        let states: Vec<Option<&str>> = if self.config.trigger_states.is_empty() {
            vec![None]
        } else {
            self.config
                .trigger_states
                .iter()
                .map(|s| Some(s.as_str()))
                .collect()
        };

        for repo in &self.config.repos {
            for state in &states {
                let gh_issues = self
                    .client
                    .list_repo_issues(repo, *state, &self.config.trigger_labels)
                    .await?;

                for gh_issue in &gh_issues {
                    let issue = self.map_issue(repo, gh_issue);
                    all_issues.push(issue);
                }
            }
        }

        // Deduplicate by issue ID (same issue could appear in multiple state queries)
        all_issues.sort_by(|a, b| a.id.cmp(&b.id));
        all_issues.dedup_by(|a, b| a.id == b.id);

        Ok(all_issues)
    }

    fn matches_criteria(&self, issue: &Issue) -> MatchResult {
        github_issues_matches_criteria(&self.config, issue)
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        Ok(format_github_issues_context(issue))
    }

    async fn get_issue(&self, issue_id: &str) -> Result<Issue> {
        let (repo, number) = parse_issue_id(issue_id)?;
        let gh_issue = self.client.get_issue(&repo, number).await?;
        Ok(self.map_issue(&repo, &gh_issue))
    }

    async fn resolve_issue(&self, issue_id: &str) -> Result<()> {
        let (repo, number) = parse_issue_id(issue_id)?;
        self.client.close_issue(&repo, number).await
    }

    async fn add_comment(&self, issue_id: &str, comment: &str) -> Result<()> {
        let (repo, number) = parse_issue_id(issue_id)?;
        self.client.add_issue_comment(&repo, number, comment).await
    }

    async fn get_issue_status(&self, issue_id: &str) -> Result<String> {
        let issue = self.get_issue(issue_id).await?;
        let state: String = issue.get_metadata("state").unwrap_or_default();
        Ok(state)
    }

    fn is_terminal_status(&self, status: &str) -> bool {
        status == "closed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::GitHubLabel;

    fn test_config() -> GitHubConfig {
        GitHubConfig::test_default()
    }

    #[test]
    fn test_format_issue_id() {
        assert_eq!(format_issue_id("owner/repo", 42), "owner:repo#42");
        assert_eq!(format_issue_id("myorg/myrepo", 1), "myorg:myrepo#1");
    }

    #[test]
    fn test_parse_issue_id_valid() {
        let (repo, number) = parse_issue_id("owner:repo#42").unwrap();
        assert_eq!(repo, "owner/repo");
        assert_eq!(number, 42);
    }

    #[test]
    fn test_parse_issue_id_no_hash() {
        let result = parse_issue_id("invalid_no_hash");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Invalid issue ID format"));
    }

    #[test]
    fn test_parse_issue_id_non_numeric() {
        let result = parse_issue_id("owner:repo#abc");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Invalid issue number"));
    }

    #[test]
    fn test_parse_issue_id_empty() {
        let result = parse_issue_id("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_issue_id_trailing_hash() {
        let result = parse_issue_id("owner:repo#");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Invalid issue number"));
    }

    #[test]
    fn test_parse_issue_id_roundtrip() {
        let id = format_issue_id("myorg/myrepo", 99);
        let (repo, number) = parse_issue_id(&id).unwrap();
        assert_eq!(repo, "myorg/myrepo");
        assert_eq!(number, 99);
    }

    #[test]
    fn test_map_issue_open() {
        let config = test_config();
        let source = GitHubSource::new(config);

        let gh_issue = GitHubIssue {
            number: 42,
            title: "Fix the bug".to_string(),
            body: Some("Bug description".to_string()),
            state: "open".to_string(),
            html_url: "https://github.com/owner/repo/issues/42".to_string(),
            labels: vec![
                GitHubLabel {
                    name: "auto-implement".to_string(),
                },
                GitHubLabel {
                    name: "bug".to_string(),
                },
            ],
            user: Some(crate::scm::ReviewUser {
                id: 1,
                login: "testuser".to_string(),
                user_type: None,
            }),
            assignee: None,
            pull_request: None,
        };

        let issue = source.map_issue("owner/repo", &gh_issue);

        assert_eq!(issue.id, "owner:repo#42");
        assert_eq!(issue.short_id, "owner:repo#42");
        assert_eq!(issue.title, "Fix the bug");
        assert_eq!(issue.description, Some("Bug description".to_string()));
        assert_eq!(issue.source, "github_issues");
        assert_eq!(issue.status, IssueStatus::Open);
        assert_eq!(
            issue.get_metadata::<String>("state"),
            Some("open".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("labels"),
            Some("auto-implement, bug".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("repo"),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("author"),
            Some("testuser".to_string())
        );
    }

    #[test]
    fn test_map_issue_closed() {
        let config = test_config();
        let source = GitHubSource::new(config);

        let gh_issue = GitHubIssue {
            number: 1,
            title: "Done".to_string(),
            body: None,
            state: "closed".to_string(),
            html_url: "https://github.com/owner/repo/issues/1".to_string(),
            labels: vec![],
            user: None,
            assignee: None,
            pull_request: None,
        };

        let issue = source.map_issue("owner/repo", &gh_issue);
        assert_eq!(issue.status, IssueStatus::Resolved);
        assert_eq!(issue.description, None);
    }

    #[test]
    fn test_matches_criteria_basic_match() {
        let source = GitHubSource::new(test_config());

        let mut issue = Issue::new(
            "owner:repo#1",
            "owner:repo#1",
            "Test",
            "https://github.com/owner/repo/issues/1",
            "github_issues",
        );
        issue.set_metadata("state", "open");
        issue.set_metadata("labels", "auto-implement");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_wrong_state() {
        let source = GitHubSource::new(test_config());

        let mut issue = Issue::new(
            "owner:repo#1",
            "owner:repo#1",
            "Test",
            "https://github.com/owner/repo/issues/1",
            "github_issues",
        );
        issue.set_metadata("state", "closed");
        issue.set_metadata("labels", "auto-implement");

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("not in trigger_states"));
    }

    #[test]
    fn test_matches_criteria_wrong_labels() {
        let source = GitHubSource::new(test_config());

        let mut issue = Issue::new(
            "owner:repo#1",
            "owner:repo#1",
            "Test",
            "https://github.com/owner/repo/issues/1",
            "github_issues",
        );
        issue.set_metadata("state", "open");
        issue.set_metadata("labels", "unrelated");

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("No matching trigger labels"));
    }

    #[test]
    fn test_matches_criteria_no_labels_config() {
        let mut config = test_config();
        config.trigger_labels = Vec::new();
        let source = GitHubSource::new(config);

        let mut issue = Issue::new(
            "owner:repo#1",
            "owner:repo#1",
            "Test",
            "https://github.com/owner/repo/issues/1",
            "github_issues",
        );
        issue.set_metadata("state", "open");
        issue.set_metadata("labels", "");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_no_states_config() {
        let mut config = test_config();
        config.trigger_states = Vec::new();
        let source = GitHubSource::new(config);

        let mut issue = Issue::new(
            "owner:repo#1",
            "owner:repo#1",
            "Test",
            "https://github.com/owner/repo/issues/1",
            "github_issues",
        );
        issue.set_metadata("state", "closed");
        issue.set_metadata("labels", "auto-implement");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_format_context() {
        let mut issue = Issue::new(
            "owner:repo#42",
            "owner:repo#42",
            "Fix the bug",
            "https://github.com/owner/repo/issues/42",
            "github_issues",
        );
        issue.description = Some("Detailed description".to_string());
        issue.set_metadata("state", "open");
        issue.set_metadata("labels", "auto-implement, bug");
        issue.set_metadata("repo", "owner/repo");
        issue.set_metadata("author", "testuser");

        let context = format_github_issues_context(&issue);
        assert!(context.contains("# GitHub Issue: owner:repo#42"));
        assert!(context.contains("**Title:** Fix the bug"));
        assert!(context.contains("**State:** open"));
        assert!(context.contains("**Labels:** auto-implement, bug"));
        assert!(context.contains("**Repository:** owner/repo"));
        assert!(context.contains("**Author:** testuser"));
        assert!(context.contains("## Description"));
        assert!(context.contains("Detailed description"));
    }

    #[test]
    fn test_format_context_minimal() {
        let issue = Issue::new(
            "owner:repo#1",
            "owner:repo#1",
            "Simple issue",
            "https://github.com/owner/repo/issues/1",
            "github_issues",
        );

        let context = format_github_issues_context(&issue);
        assert!(context.contains("**Title:** Simple issue"));
        assert!(!context.contains("## Description"));
    }

    #[test]
    fn test_source_name() {
        let source = GitHubSource::new(test_config());
        assert_eq!(source.name(), "github_issues");
        assert_eq!(source.display_name(), "GitHub Issues");
    }

    #[test]
    fn test_is_terminal_status() {
        let source = GitHubSource::new(test_config());
        assert!(source.is_terminal_status("closed"));
        assert!(!source.is_terminal_status("open"));
        assert!(!source.is_terminal_status(""));
        assert!(!source.is_terminal_status("CLOSED")); // case-sensitive
    }

    #[tokio::test]
    async fn test_get_issue_invalid_id_no_hash() {
        let source = GitHubSource::new(test_config());
        let result = source.get_issue("invalid_no_hash").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Invalid issue ID format"));
    }

    #[tokio::test]
    async fn test_get_issue_invalid_id_non_numeric() {
        let source = GitHubSource::new(test_config());
        let result = source.get_issue("owner:repo#abc").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Invalid issue number"));
    }

    #[tokio::test]
    async fn test_build_issue_context_async() {
        let source = GitHubSource::new(test_config());

        let mut issue = Issue::new(
            "owner:repo#10",
            "owner:repo#10",
            "Async context test",
            "https://github.com/owner/repo/issues/10",
            "github_issues",
        );
        issue.description = Some("Test description".to_string());
        issue.set_metadata("state", "open");
        issue.set_metadata("labels", "bug");
        issue.set_metadata("repo", "owner/repo");

        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("# GitHub Issue: owner:repo#10"));
        assert!(context.contains("**Title:** Async context test"));
    }

    mod fetch_tests {
        use super::*;
        use abnegate_http::HttpClient;
        use abnegate_http::HttpResponse;
        use async_trait::async_trait;
        use std::collections::HashMap;
        use std::sync::Mutex;

        struct MockHttpClient {
            responses: Mutex<HashMap<String, HttpResponse>>,
        }

        impl MockHttpClient {
            fn new() -> Self {
                Self {
                    responses: Mutex::new(HashMap::new()),
                }
            }

            fn mock_response(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
                self.responses
                    .lock()
                    .unwrap()
                    .insert(url.into(), HttpResponse::new(status, body));
            }
        }

        #[async_trait]
        impl HttpClient for MockHttpClient {
            async fn get(
                &self,
                url: &str,
                _headers: Vec<(&str, String)>,
            ) -> abnegate_http::Result<HttpResponse> {
                let responses = self.responses.lock().unwrap();
                if let Some(response) = responses.get(url) {
                    Ok(response.clone())
                } else {
                    Ok(HttpResponse::new(404, "Not found"))
                }
            }
        }

        #[tokio::test]
        async fn test_fetch_issues_single_repo() {
            let mock = MockHttpClient::new();
            mock.mock_response(
                "https://api.github.com/repos/owner/repo/issues?per_page=100&page=1&state=open&labels=auto-implement,claude",
                200,
                r#"[
                    {
                        "number": 1,
                        "title": "First issue",
                        "body": "Description one",
                        "state": "open",
                        "html_url": "https://github.com/owner/repo/issues/1",
                        "labels": [{"name": "auto-implement"}],
                        "user": {"id": 1, "login": "author1"},
                        "assignee": null,
                        "pull_request": null
                    },
                    {
                        "number": 2,
                        "title": "A pull request",
                        "body": "This is a PR",
                        "state": "open",
                        "html_url": "https://github.com/owner/repo/pull/2",
                        "labels": [],
                        "user": null,
                        "assignee": null,
                        "pull_request": {"url": "https://api.github.com/repos/owner/repo/pulls/2"}
                    }
                ]"#,
            );

            let mut config = test_config();
            config.repos = vec!["owner/repo".to_string()];
            config.trigger_labels = vec!["auto-implement".to_string(), "claude".to_string()];
            config.trigger_states = vec!["open".to_string()];

            let source = GitHubSource::with_http_client(config, mock);
            let issues = source.fetch_issues().await.unwrap();

            // PR should be filtered out
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].id, "owner:repo#1");
            assert_eq!(issues[0].title, "First issue");
        }

        #[tokio::test]
        async fn test_fetch_issues_deduplication() {
            let mock = MockHttpClient::new();

            mock.mock_response(
                "https://api.github.com/repos/owner/repo/issues?per_page=100&page=1&state=open&labels=auto-implement",
                200,
                r#"[
                    {
                        "number": 1,
                        "title": "Dupe issue",
                        "body": null,
                        "state": "open",
                        "html_url": "https://github.com/owner/repo/issues/1",
                        "labels": [{"name": "auto-implement"}],
                        "user": null,
                        "assignee": null,
                        "pull_request": null
                    }
                ]"#,
            );

            mock.mock_response(
                "https://api.github.com/repos/owner/repo/issues?per_page=100&page=1&state=closed&labels=auto-implement",
                200,
                r#"[
                    {
                        "number": 1,
                        "title": "Dupe issue",
                        "body": null,
                        "state": "closed",
                        "html_url": "https://github.com/owner/repo/issues/1",
                        "labels": [{"name": "auto-implement"}],
                        "user": null,
                        "assignee": null,
                        "pull_request": null
                    }
                ]"#,
            );

            let mut config = test_config();
            config.repos = vec!["owner/repo".to_string()];
            config.trigger_labels = vec!["auto-implement".to_string()];
            config.trigger_states = vec!["open".to_string(), "closed".to_string()];

            let source = GitHubSource::with_http_client(config, mock);
            let issues = source.fetch_issues().await.unwrap();

            // Same issue #1 in two states should be deduped
            assert_eq!(issues.len(), 1);
        }

        #[tokio::test]
        async fn test_fetch_issues_empty_response() {
            let mock = MockHttpClient::new();
            mock.mock_response(
                "https://api.github.com/repos/owner/repo/issues?per_page=100&page=1&state=open&labels=auto-implement,claude",
                200,
                "[]",
            );

            let source = GitHubSource::with_http_client(test_config(), mock);
            let issues = source.fetch_issues().await.unwrap();
            assert!(issues.is_empty());
        }

        #[tokio::test]
        async fn test_get_issue_via_mock() {
            let mock = MockHttpClient::new();
            mock.mock_response(
                "https://api.github.com/repos/owner/repo/issues/42",
                200,
                r#"{
                    "number": 42,
                    "title": "Mock issue",
                    "body": "Fetched via mock",
                    "state": "open",
                    "html_url": "https://github.com/owner/repo/issues/42",
                    "labels": [{"name": "bug"}],
                    "user": {"id": 1, "login": "mockuser"},
                    "assignee": null,
                    "pull_request": null
                }"#,
            );

            let source = GitHubSource::with_http_client(test_config(), mock);
            let issue = source.get_issue("owner:repo#42").await.unwrap();

            assert_eq!(issue.id, "owner:repo#42");
            assert_eq!(issue.title, "Mock issue");
            assert_eq!(
                issue.get_metadata::<String>("state"),
                Some("open".to_string())
            );
        }

        #[tokio::test]
        async fn test_get_issue_not_found() {
            let mock = MockHttpClient::new();
            // No mock response = 404

            let source = GitHubSource::with_http_client(test_config(), mock);
            let result = source.get_issue("owner:repo#999").await;
            assert!(result.is_err());
        }
    }
}
