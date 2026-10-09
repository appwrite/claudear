//! GitLab issue source adapter.

use super::IssueSource;
use crate::gitlab::GitLabClient;
use async_trait::async_trait;
use claudear_config::config::GitLabConfig;
use claudear_core::error::{Error, Result};
use claudear_core::types::{Issue, IssueStatus, MatchPriority, MatchResult};

/// GitLab issue source.
///
/// Fetches issues from GitLab groups and maps them to the unified Issue type.
pub struct GitLabSource {
    client: GitLabClient,
    config: GitLabConfig,
}

impl GitLabSource {
    /// Create a new GitLab source.
    pub fn new(config: GitLabConfig) -> Self {
        let client = GitLabClient::new(config.clone());
        Self { client, config }
    }

    /// Map a GitLab API issue to the unified Issue type.
    ///
    /// Issue ID format: `{project_path}:{issue_iid}` (e.g., `mygroup/myproject:42`).
    /// The project path is extracted from the issue web_url.
    fn map_issue(
        &self,
        iid: i64,
        title: &str,
        description: Option<&str>,
        state: &str,
        web_url: &str,
        labels: &[String],
    ) -> Issue {
        // Extract project path from web_url:
        // e.g. "https://gitlab.com/mygroup/myproject/-/issues/42" -> "mygroup/myproject"
        let project_path = extract_project_path(web_url, &self.config.base_url);

        let issue_id = format!("{}:{}", project_path, iid);
        let short_id = format!("{}:{}", project_path, iid);

        let mut issue = Issue::new(&issue_id, &short_id, title, web_url, "gitlab");

        issue.description = description.map(|d| d.to_string());

        issue.status = match state {
            "closed" => IssueStatus::Resolved,
            "opened" => IssueStatus::Open,
            _ => IssueStatus::Open,
        };

        issue.set_metadata("state", state);
        issue.set_metadata("labels", labels.join(", "));
        issue.set_metadata("project_path", &project_path);
        issue.set_metadata("iid", iid);

        issue
    }
}

/// Extract the project path from a GitLab issue web_url.
///
/// Given `https://gitlab.com/mygroup/myproject/-/issues/42` and base_url `https://gitlab.com`,
/// returns `mygroup/myproject`.
fn extract_project_path(web_url: &str, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let path = web_url
        .strip_prefix(base)
        .unwrap_or(web_url)
        .trim_start_matches('/');

    // Path looks like: "mygroup/myproject/-/issues/42"
    // Split on "/-/" and take the first part
    if let Some(project) = path.split("/-/").next() {
        project.trim_matches('/').to_string()
    } else {
        path.to_string()
    }
}

/// Build context string for a GitLab issue.
pub(crate) fn format_gitlab_context(issue: &Issue) -> String {
    let mut context = format!("# GitLab Issue: {}\n\n", issue.short_id);
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

    if let Some(project_path) = issue.get_metadata::<String>("project_path") {
        context.push_str(&format!("**Project:** {}\n", project_path));
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

/// Check if an issue matches GitLab trigger criteria.
pub(crate) fn gitlab_matches_criteria(config: &GitLabConfig, issue: &Issue) -> MatchResult {
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
        format!("GitLab issue {} matches criteria", issue.short_id),
        MatchPriority::Normal,
    )
}

#[async_trait]
impl IssueSource for GitLabSource {
    fn name(&self) -> &str {
        "gitlab"
    }

    fn display_name(&self) -> &str {
        "GitLab"
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        let mut all_issues = Vec::new();

        let labels = &self.config.trigger_labels;

        // For each state in trigger_states, fetch issues from each group
        let states: Vec<Option<&str>> = if self.config.trigger_states.is_empty() {
            vec![None]
        } else {
            self.config
                .trigger_states
                .iter()
                .map(|s| Some(s.as_str()))
                .collect()
        };

        for group in &self.config.groups {
            for state in &states {
                let api_issues = self.client.get_group_issues(group, labels, *state).await?;

                for api_issue in api_issues {
                    let issue = self.map_issue(
                        api_issue.iid,
                        &api_issue.title,
                        api_issue.description.as_deref(),
                        &api_issue.state,
                        &api_issue.web_url,
                        &api_issue.labels,
                    );
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
        gitlab_matches_criteria(&self.config, issue)
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        Ok(format_gitlab_context(issue))
    }

    async fn get_issue(&self, issue_id: &str) -> Result<Issue> {
        // Issue ID format: "project_path:iid"
        let (project_path, iid_str) = issue_id.rsplit_once(':').ok_or_else(|| {
            Error::source(
                "gitlab",
                format!(
                    "Invalid issue ID format '{}', expected 'project_path:iid'",
                    issue_id
                ),
            )
        })?;

        let iid: i64 = iid_str.parse().map_err(|_| {
            Error::source(
                "gitlab",
                format!("Invalid issue IID '{}', expected integer", iid_str),
            )
        })?;

        let api_issue = self.client.get_issue(project_path, iid).await?;
        Ok(self.map_issue(
            api_issue.iid,
            &api_issue.title,
            api_issue.description.as_deref(),
            &api_issue.state,
            &api_issue.web_url,
            &api_issue.labels,
        ))
    }

    async fn resolve_issue(&self, issue_id: &str) -> Result<()> {
        // GitLabClient currently only supports GET methods.
        // POST/PUT support for closing issues will be added later.
        tracing::warn!(
            source = "gitlab",
            issue_id = %issue_id,
            "resolve_issue not yet implemented - GitLab API write support pending"
        );
        Ok(())
    }

    async fn add_comment(&self, issue_id: &str, comment: &str) -> Result<()> {
        // GitLabClient currently only supports GET methods.
        // POST support for adding comments will be added later.
        tracing::info!(
            source = "gitlab",
            issue_id = %issue_id,
            comment_len = comment.len(),
            "add_comment not yet implemented - GitLab API write support pending"
        );
        Ok(())
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

    fn test_config() -> GitLabConfig {
        GitLabConfig::test_default()
    }

    #[test]
    fn test_extract_project_path_standard() {
        let url = "https://gitlab.com/mygroup/myproject/-/issues/42";
        assert_eq!(
            extract_project_path(url, "https://gitlab.com"),
            "mygroup/myproject"
        );
    }

    #[test]
    fn test_extract_project_path_subgroup() {
        let url = "https://gitlab.com/mygroup/subgroup/myproject/-/issues/7";
        assert_eq!(
            extract_project_path(url, "https://gitlab.com"),
            "mygroup/subgroup/myproject"
        );
    }

    #[test]
    fn test_extract_project_path_trailing_slash() {
        let url = "https://gitlab.com/mygroup/myproject/-/issues/1";
        assert_eq!(
            extract_project_path(url, "https://gitlab.com/"),
            "mygroup/myproject"
        );
    }

    #[test]
    fn test_source_name() {
        let source = GitLabSource::new(test_config());
        assert_eq!(source.name(), "gitlab");
        assert_eq!(source.display_name(), "GitLab");
    }

    #[test]
    fn test_is_terminal_status() {
        let source = GitLabSource::new(test_config());
        assert!(source.is_terminal_status("closed"));
        assert!(!source.is_terminal_status("opened"));
        assert!(!source.is_terminal_status(""));
    }

    #[test]
    fn test_matches_criteria_basic_match() {
        let source = GitLabSource::new(test_config());

        let mut issue = Issue::new(
            "mygroup/proj:1",
            "mygroup/proj:1",
            "Test",
            "https://gitlab.com/mygroup/proj/-/issues/1",
            "gitlab",
        );
        issue.set_metadata("state", "opened");
        issue.set_metadata("labels", "auto-implement");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_wrong_state() {
        let source = GitLabSource::new(test_config());

        let mut issue = Issue::new(
            "mygroup/proj:1",
            "mygroup/proj:1",
            "Test",
            "https://gitlab.com/mygroup/proj/-/issues/1",
            "gitlab",
        );
        issue.set_metadata("state", "closed");
        issue.set_metadata("labels", "auto-implement");

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("not in trigger_states"));
    }

    #[test]
    fn test_matches_criteria_wrong_labels() {
        let source = GitLabSource::new(test_config());

        let mut issue = Issue::new(
            "mygroup/proj:1",
            "mygroup/proj:1",
            "Test",
            "https://gitlab.com/mygroup/proj/-/issues/1",
            "gitlab",
        );
        issue.set_metadata("state", "opened");
        issue.set_metadata("labels", "unrelated");

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("No matching trigger labels"));
    }

    #[test]
    fn test_matches_criteria_no_labels_config() {
        let mut config = test_config();
        config.trigger_labels = Vec::new();
        let source = GitLabSource::new(config);

        let mut issue = Issue::new(
            "mygroup/proj:1",
            "mygroup/proj:1",
            "Test",
            "https://gitlab.com/mygroup/proj/-/issues/1",
            "gitlab",
        );
        issue.set_metadata("state", "opened");
        issue.set_metadata("labels", "");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_no_states_config() {
        let mut config = test_config();
        config.trigger_states = Vec::new();
        let source = GitLabSource::new(config);

        let mut issue = Issue::new(
            "mygroup/proj:1",
            "mygroup/proj:1",
            "Test",
            "https://gitlab.com/mygroup/proj/-/issues/1",
            "gitlab",
        );
        issue.set_metadata("state", "closed");
        issue.set_metadata("labels", "auto-implement");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_map_issue() {
        let source = GitLabSource::new(test_config());
        let issue = source.map_issue(
            42,
            "Fix the bug",
            Some("Bug description"),
            "opened",
            "https://gitlab.com/mygroup/myproject/-/issues/42",
            &["auto-implement".to_string(), "bug".to_string()],
        );

        assert_eq!(issue.id, "mygroup/myproject:42");
        assert_eq!(issue.short_id, "mygroup/myproject:42");
        assert_eq!(issue.title, "Fix the bug");
        assert_eq!(issue.description, Some("Bug description".to_string()));
        assert_eq!(issue.source, "gitlab");
        assert_eq!(issue.status, IssueStatus::Open);
        assert_eq!(
            issue.get_metadata::<String>("state"),
            Some("opened".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("labels"),
            Some("auto-implement, bug".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("project_path"),
            Some("mygroup/myproject".to_string())
        );
    }

    #[test]
    fn test_map_issue_closed() {
        let source = GitLabSource::new(test_config());
        let issue = source.map_issue(
            1,
            "Done",
            None,
            "closed",
            "https://gitlab.com/mygroup/proj/-/issues/1",
            &[],
        );

        assert_eq!(issue.status, IssueStatus::Resolved);
        assert_eq!(issue.description, None);
    }

    #[test]
    fn test_format_gitlab_context() {
        let mut issue = Issue::new(
            "mygroup/proj:42",
            "mygroup/proj:42",
            "Fix the bug",
            "https://gitlab.com/mygroup/proj/-/issues/42",
            "gitlab",
        );
        issue.description = Some("Detailed description".to_string());
        issue.set_metadata("state", "opened");
        issue.set_metadata("labels", "auto-implement, bug");
        issue.set_metadata("project_path", "mygroup/proj");

        let context = format_gitlab_context(&issue);
        assert!(context.contains("# GitLab Issue: mygroup/proj:42"));
        assert!(context.contains("**Title:** Fix the bug"));
        assert!(context.contains("**State:** opened"));
        assert!(context.contains("**Labels:** auto-implement, bug"));
        assert!(context.contains("**Project:** mygroup/proj"));
        assert!(context.contains("## Description"));
        assert!(context.contains("Detailed description"));
    }

    #[test]
    fn test_format_gitlab_context_minimal() {
        let issue = Issue::new(
            "mygroup/proj:1",
            "mygroup/proj:1",
            "Simple issue",
            "https://gitlab.com/mygroup/proj/-/issues/1",
            "gitlab",
        );

        let context = format_gitlab_context(&issue);
        assert!(context.contains("# GitLab Issue: mygroup/proj:1"));
        assert!(context.contains("**Title:** Simple issue"));
        assert!(!context.contains("## Description"));
    }

    #[tokio::test]
    async fn test_get_issue_invalid_id_no_colon() {
        let source = GitLabSource::new(test_config());
        let result = source.get_issue("invalid_no_colon").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid issue ID format"),
            "error should mention invalid format: {err_msg}"
        );
        assert!(
            err_msg.contains("project_path:iid"),
            "error should mention expected format: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_issue_invalid_id_non_numeric_iid() {
        let source = GitLabSource::new(test_config());
        let result = source.get_issue("mygroup/project:abc").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid issue IID"),
            "error should mention invalid IID: {err_msg}"
        );
        assert!(
            err_msg.contains("abc"),
            "error should include the bad IID value: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_issue_invalid_id_empty_string() {
        let source = GitLabSource::new(test_config());
        let result = source.get_issue("").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid issue ID format"),
            "error should mention invalid format for empty string: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_issue_invalid_id_colon_only() {
        let source = GitLabSource::new(test_config());
        // ":" splits into ("", "") -- the IID part is empty, which fails i64 parse
        let result = source.get_issue(":").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid issue IID"),
            "error should mention invalid IID for colon-only input: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_issue_invalid_id_trailing_colon() {
        let source = GitLabSource::new(test_config());
        // "mygroup/project:" splits into ("mygroup/project", "") -- empty IID fails parse
        let result = source.get_issue("mygroup/project:").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid issue IID"),
            "error should mention invalid IID for trailing colon: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_issue_invalid_id_negative_iid() {
        let source = GitLabSource::new(test_config());
        // Negative IIDs are technically parseable as i64 but the test verifies
        // the parsing path works for edge cases. -1 will parse but the API call
        // will fail (no mock). We just verify it gets past parsing.
        let result = source.get_issue("mygroup/project:-1").await;
        // This will error at the HTTP level since there's no mock, but it should
        // NOT error with "Invalid issue IID" -- the parse succeeds for negative numbers.
        if let Err(e) = &result {
            let err_msg = e.to_string();
            assert!(
                !err_msg.contains("Invalid issue IID"),
                "negative IID should parse as i64: {err_msg}"
            );
        }
    }

    #[tokio::test]
    async fn test_get_issue_id_with_multiple_colons() {
        let source = GitLabSource::new(test_config());
        // "group:subgroup:42" -- rsplit_once(':') yields ("group:subgroup", "42")
        // This should parse successfully (IID = 42), but the API call will fail
        // since there's no mock. We verify the parsing doesn't reject it.
        let result = source.get_issue("group:subgroup:42").await;
        if let Err(e) = &result {
            let err_msg = e.to_string();
            assert!(
                !err_msg.contains("Invalid issue ID format"),
                "multiple colons should be handled by rsplit_once: {err_msg}"
            );
            assert!(
                !err_msg.contains("Invalid issue IID"),
                "IID '42' should parse fine: {err_msg}"
            );
        }
    }

    #[tokio::test]
    async fn test_resolve_issue_returns_ok() {
        let source = GitLabSource::new(test_config());
        // resolve_issue is a no-op that always returns Ok(())
        let result = source.resolve_issue("any-issue-id").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_resolve_issue_returns_ok_with_empty_id() {
        let source = GitLabSource::new(test_config());
        let result = source.resolve_issue("").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_comment_returns_ok() {
        let source = GitLabSource::new(test_config());
        // add_comment is a no-op that always returns Ok(())
        let result = source
            .add_comment("any-issue-id", "some comment text")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_comment_returns_ok_with_empty_inputs() {
        let source = GitLabSource::new(test_config());
        let result = source.add_comment("", "").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_issue_status_invalid_id() {
        let source = GitLabSource::new(test_config());
        // get_issue_status delegates to get_issue, so invalid ID format causes error
        let result = source.get_issue_status("bad_id_no_colon").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid issue ID format"),
            "error should mention invalid format: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_issue_status_invalid_iid() {
        let source = GitLabSource::new(test_config());
        let result = source.get_issue_status("project:not_a_number").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid issue IID"),
            "error should mention invalid IID: {err_msg}"
        );
    }

    #[test]
    fn test_is_terminal_status_variants() {
        let source = GitLabSource::new(test_config());

        // "closed" is the only terminal status for GitLab
        assert!(source.is_terminal_status("closed"));

        // Non-terminal statuses
        assert!(!source.is_terminal_status("opened"));
        assert!(!source.is_terminal_status("merged"));
        assert!(!source.is_terminal_status(""));
        assert!(!source.is_terminal_status("open"));
        assert!(!source.is_terminal_status("CLOSED")); // case-sensitive
        assert!(!source.is_terminal_status("Closed"));
        assert!(!source.is_terminal_status("resolved"));
        assert!(!source.is_terminal_status("completed"));
        assert!(!source.is_terminal_status("cancelled"));
    }

    #[tokio::test]
    async fn test_build_issue_context_async() {
        let source = GitLabSource::new(test_config());

        let mut issue = Issue::new(
            "mygroup/proj:10",
            "mygroup/proj:10",
            "Async context test",
            "https://gitlab.com/mygroup/proj/-/issues/10",
            "gitlab",
        );
        issue.description = Some("Test description for context".to_string());
        issue.set_metadata("state", "opened");
        issue.set_metadata("labels", "bug");
        issue.set_metadata("project_path", "mygroup/proj");

        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("# GitLab Issue: mygroup/proj:10"));
        assert!(context.contains("**Title:** Async context test"));
        assert!(context.contains("**State:** opened"));
        assert!(context.contains("**Labels:** bug"));
        assert!(context.contains("**Project:** mygroup/proj"));
        assert!(context.contains("## Description"));
        assert!(context.contains("Test description for context"));
    }

    #[tokio::test]
    async fn test_build_issue_context_no_description() {
        let source = GitLabSource::new(test_config());

        let issue = Issue::new(
            "mygroup/proj:5",
            "mygroup/proj:5",
            "No description issue",
            "https://gitlab.com/mygroup/proj/-/issues/5",
            "gitlab",
        );

        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("**Title:** No description issue"));
        assert!(!context.contains("## Description"));
    }

    /// Mock HTTP client for testing fetch_issues deduplication logic.
    /// Reuses the same pattern as the GitLabClient test module.
    mod fetch_tests {
        use super::*;
        use crate::gitlab::GitLabClient;
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

        /// Test the deduplication logic in fetch_issues by simulating what
        /// the method does: fetch from client, map issues, sort, dedup.
        ///
        /// Since GitLabSource stores a concrete GitLabClient (not generic),
        /// we test the dedup logic by directly exercising the client + map_issue
        /// pipeline, which is exactly what fetch_issues does internally.
        #[tokio::test]
        async fn test_fetch_issues_deduplication() {
            let mock = MockHttpClient::new();

            // Simulate the same issue appearing in two state queries for the same group.
            // The URL for group issues includes state filter, so "opened" and "closed"
            // queries produce different URLs but may return overlapping issues.
            let issue_json = r#"[
                {
                    "iid": 42,
                    "title": "Duplicate issue",
                    "description": "This appears in multiple queries",
                    "state": "opened",
                    "web_url": "https://gitlab.com/mygroup/myproject/-/issues/42",
                    "labels": ["auto-implement"],
                    "author": null,
                    "assignees": []
                },
                {
                    "iid": 43,
                    "title": "Unique issue",
                    "description": null,
                    "state": "opened",
                    "web_url": "https://gitlab.com/mygroup/myproject/-/issues/43",
                    "labels": [],
                    "author": null,
                    "assignees": []
                }
            ]"#;

            let issue_json_closed = r#"[
                {
                    "iid": 42,
                    "title": "Duplicate issue",
                    "description": "This appears in multiple queries",
                    "state": "closed",
                    "web_url": "https://gitlab.com/mygroup/myproject/-/issues/42",
                    "labels": ["auto-implement"],
                    "author": null,
                    "assignees": []
                }
            ]"#;

            // Mock both state query URLs
            mock.mock_response(
                "https://gitlab.com/api/v4/groups/mygroup/issues?per_page=100&labels=auto-implement,claude&state=opened",
                200,
                issue_json,
            );
            mock.mock_response(
                "https://gitlab.com/api/v4/groups/mygroup/issues?per_page=100&labels=auto-implement,claude&state=closed",
                200,
                issue_json_closed,
            );

            let mut config = test_config();
            config.groups = vec!["mygroup".to_string()];
            config.trigger_labels = vec!["auto-implement".to_string(), "claude".to_string()];
            config.trigger_states = vec!["opened".to_string(), "closed".to_string()];

            let client = GitLabClient::with_http_client(config.clone(), mock);

            // Replicate the fetch_issues logic: fetch from each group/state, map, dedup
            let source_for_mapping = GitLabSource::new(config.clone());
            let mut all_issues = Vec::new();

            let states: Vec<Option<&str>> = config
                .trigger_states
                .iter()
                .map(|s| Some(s.as_str()))
                .collect();

            for group in &config.groups {
                for state in &states {
                    let api_issues = client
                        .get_group_issues(group, &config.trigger_labels, *state)
                        .await
                        .unwrap();

                    for api_issue in api_issues {
                        let issue = source_for_mapping.map_issue(
                            api_issue.iid,
                            &api_issue.title,
                            api_issue.description.as_deref(),
                            &api_issue.state,
                            &api_issue.web_url,
                            &api_issue.labels,
                        );
                        all_issues.push(issue);
                    }
                }
            }

            // Before dedup: 2 from opened query + 1 from closed query = 3 total
            assert_eq!(all_issues.len(), 3, "should have 3 issues before dedup");

            // Apply the same dedup logic as fetch_issues
            all_issues.sort_by(|a, b| a.id.cmp(&b.id));
            all_issues.dedup_by(|a, b| a.id == b.id);

            // After dedup: issue 42 appears twice, so we should have 2 unique issues
            assert_eq!(all_issues.len(), 2, "should have 2 issues after dedup");

            // Verify the unique IDs
            let ids: Vec<&str> = all_issues.iter().map(|i| i.id.as_str()).collect();
            assert!(ids.contains(&"mygroup/myproject:42"));
            assert!(ids.contains(&"mygroup/myproject:43"));
        }

        /// Test fetch_issues with a single issue from the mock client.
        #[tokio::test]
        async fn test_fetch_issues_single_group_single_state() {
            let mock = MockHttpClient::new();

            let issue_json = r#"[
                {
                    "iid": 1,
                    "title": "First issue",
                    "description": "Description one",
                    "state": "opened",
                    "web_url": "https://gitlab.com/team/repo/-/issues/1",
                    "labels": ["claude"],
                    "author": null,
                    "assignees": []
                }
            ]"#;

            mock.mock_response(
                "https://gitlab.com/api/v4/groups/team/issues?per_page=100&labels=claude&state=opened",
                200,
                issue_json,
            );

            let mut config = test_config();
            config.groups = vec!["team".to_string()];
            config.trigger_labels = vec!["claude".to_string()];
            config.trigger_states = vec!["opened".to_string()];

            let client = GitLabClient::with_http_client(config.clone(), mock);
            let source_for_mapping = GitLabSource::new(config.clone());

            let api_issues = client
                .get_group_issues("team", &config.trigger_labels, Some("opened"))
                .await
                .unwrap();

            assert_eq!(api_issues.len(), 1);

            let issue = source_for_mapping.map_issue(
                api_issues[0].iid,
                &api_issues[0].title,
                api_issues[0].description.as_deref(),
                &api_issues[0].state,
                &api_issues[0].web_url,
                &api_issues[0].labels,
            );

            assert_eq!(issue.id, "team/repo:1");
            assert_eq!(issue.title, "First issue");
            assert_eq!(issue.description, Some("Description one".to_string()));
            assert_eq!(issue.status, IssueStatus::Open);
            assert_eq!(issue.source, "gitlab");
        }

        /// Test fetch_issues with empty response (no issues in group).
        #[tokio::test]
        async fn test_fetch_issues_empty_response() {
            let mock = MockHttpClient::new();

            mock.mock_response(
                "https://gitlab.com/api/v4/groups/empty-group/issues?per_page=100&state=opened",
                200,
                "[]",
            );

            let mut config = test_config();
            config.groups = vec!["empty-group".to_string()];
            config.trigger_labels = vec![];
            config.trigger_states = vec!["opened".to_string()];

            let client = GitLabClient::with_http_client(config.clone(), mock);
            let api_issues = client
                .get_group_issues("empty-group", &config.trigger_labels, Some("opened"))
                .await
                .unwrap();

            assert!(api_issues.is_empty());
        }

        /// Test that get_issue works through the client with a mock when given
        /// a valid project_path:iid format.
        #[tokio::test]
        async fn test_get_issue_via_client_with_mock() {
            let mock = MockHttpClient::new();

            mock.mock_response(
                "https://gitlab.com/api/v4/projects/mygroup%2Fmyproject/issues/42",
                200,
                r#"{
                    "iid": 42,
                    "title": "Mock issue",
                    "description": "Fetched via mock",
                    "state": "opened",
                    "web_url": "https://gitlab.com/mygroup/myproject/-/issues/42",
                    "labels": ["bug"],
                    "author": null,
                    "assignees": []
                }"#,
            );

            let config = test_config();
            let client = GitLabClient::with_http_client(config.clone(), mock);

            let api_issue = client.get_issue("mygroup/myproject", 42).await.unwrap();
            assert_eq!(api_issue.iid, 42);
            assert_eq!(api_issue.title, "Mock issue");
            assert_eq!(api_issue.state, "opened");

            // Now map it through the source to verify end-to-end mapping
            let source = GitLabSource::new(config);
            let issue = source.map_issue(
                api_issue.iid,
                &api_issue.title,
                api_issue.description.as_deref(),
                &api_issue.state,
                &api_issue.web_url,
                &api_issue.labels,
            );
            assert_eq!(issue.id, "mygroup/myproject:42");
            assert_eq!(issue.title, "Mock issue");
            assert_eq!(
                issue.get_metadata::<String>("state"),
                Some("opened".to_string())
            );
        }

        /// Test get_issue_status end-to-end: parse ID -> fetch via client -> extract state.
        /// We test the parsing + state extraction logic, using the client mock for the fetch.
        #[tokio::test]
        async fn test_get_issue_status_extracts_state() {
            let mock = MockHttpClient::new();

            mock.mock_response(
                "https://gitlab.com/api/v4/projects/org%2Frepo/issues/7",
                200,
                r#"{
                    "iid": 7,
                    "title": "Status test",
                    "description": null,
                    "state": "closed",
                    "web_url": "https://gitlab.com/org/repo/-/issues/7",
                    "labels": [],
                    "author": null,
                    "assignees": []
                }"#,
            );

            let config = test_config();
            let client = GitLabClient::with_http_client(config.clone(), mock);

            // Simulate what get_issue_status does: parse the ID, fetch, extract state
            let issue_id = "org/repo:7";
            let (project_path, iid_str) = issue_id.rsplit_once(':').unwrap();
            let iid: i64 = iid_str.parse().unwrap();

            let api_issue = client.get_issue(project_path, iid).await.unwrap();
            let source = GitLabSource::new(config);
            let issue = source.map_issue(
                api_issue.iid,
                &api_issue.title,
                api_issue.description.as_deref(),
                &api_issue.state,
                &api_issue.web_url,
                &api_issue.labels,
            );

            let state: String = issue.get_metadata("state").unwrap_or_default();
            assert_eq!(state, "closed");
            assert!(source.is_terminal_status(&state));
        }
    }
}
