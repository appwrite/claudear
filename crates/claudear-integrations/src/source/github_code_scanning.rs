//! GitHub code-scanning alerts source adapter.

use super::IssueSource;
use crate::github::CodeScanningAlert;
use crate::github::GitHubClient;
use abnegate_http::HttpClient;
use async_trait::async_trait;
use chrono::DateTime;
use chrono::Utc;
use claudear_config::config::CodeScanningSeverity;
use claudear_config::config::GitHubCodeScanningConfig;
use claudear_config::config::GitHubConfig;
use claudear_core::error::Error;
use claudear_core::error::Result;
use claudear_core::types::Issue;
use claudear_core::types::IssuePriority;
use claudear_core::types::IssueStatus;
use claudear_core::types::MatchPriority;
use claudear_core::types::MatchResult;

const DISPLAY_NAME: &str = "GitHub Code Scanning";
const ALERT_ID_MARKER: &str = ":alert#";
const CODE_SCANNING_LABEL: &str = "code-scanning";

mod metadata {
    pub const STATE: &str = "state";
    pub const REPO: &str = "repo";
    pub const NUMBER: &str = "number";
    pub const TOOL: &str = "tool";
    pub const SEVERITY: &str = "severity";
    pub const RULE_ID: &str = "rule_id";
    pub const RULE_NAME: &str = "rule_name";
    pub const LABELS: &str = "labels";
    pub const FILENAME: &str = "filename";
    pub const START_LINE: &str = "start_line";
    pub const END_LINE: &str = "end_line";
    pub const MESSAGE: &str = "message";
    pub const COMMIT: &str = "commit_sha";
}

/// State of a code-scanning alert as reported by GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlertState {
    Open,
    Fixed,
    Dismissed,
    Closed,
}

impl AlertState {
    const ALL: [Self; 4] = [Self::Open, Self::Fixed, Self::Dismissed, Self::Closed];

    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Fixed => "fixed",
            Self::Dismissed => "dismissed",
            Self::Closed => "closed",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == name)
    }

    fn is_terminal(self) -> bool {
        self != Self::Open
    }

    fn issue_status(self) -> IssueStatus {
        match self {
            Self::Open => IssueStatus::Open,
            Self::Fixed | Self::Closed => IssueStatus::Resolved,
            Self::Dismissed => IssueStatus::Ignored,
        }
    }
}

/// GitHub code-scanning alerts source.
///
/// Polls open code-scanning alerts and maps each one to the unified Issue type,
/// authenticating with the `[scm.github]` token.
pub struct GitHubCodeScanningSource<H: HttpClient = abnegate_http::ReqwestHttpClient> {
    client: GitHubClient<H>,
    config: GitHubCodeScanningConfig,
}

impl GitHubCodeScanningSource {
    /// Create a new code-scanning source with the default HTTP client.
    pub fn new(config: GitHubCodeScanningConfig, github: GitHubConfig) -> Self {
        let client = GitHubClient::new(github);
        Self { client, config }
    }
}

impl<H: HttpClient> GitHubCodeScanningSource<H> {
    /// Create a new code-scanning source with a custom HTTP client.
    pub fn with_http_client(
        config: GitHubCodeScanningConfig,
        github: GitHubConfig,
        http: H,
    ) -> Self {
        let client = GitHubClient::with_http_client(github, http);
        Self { client, config }
    }

    fn tool_queries(&self) -> Vec<Option<&str>> {
        if self.config.tool_names.is_empty() {
            vec![None]
        } else {
            self.config
                .tool_names
                .iter()
                .map(|name| Some(name.as_str()))
                .collect()
        }
    }
}

/// Severity of an alert: `rule.security_severity_level`, falling back to `rule.severity`.
fn alert_severity(alert: &CodeScanningAlert) -> Option<CodeScanningSeverity> {
    let parse = |name: &Option<String>| name.as_deref().and_then(|name| name.parse().ok());
    parse(&alert.rule.security_severity_level).or_else(|| parse(&alert.rule.severity))
}

fn issue_priority(severity: Option<CodeScanningSeverity>) -> IssuePriority {
    match severity {
        Some(CodeScanningSeverity::Critical) => IssuePriority::Critical,
        Some(CodeScanningSeverity::High | CodeScanningSeverity::Error) => IssuePriority::High,
        Some(CodeScanningSeverity::Medium | CodeScanningSeverity::Warning) => IssuePriority::Medium,
        Some(CodeScanningSeverity::Low | CodeScanningSeverity::Note) => IssuePriority::Low,
        Some(CodeScanningSeverity::None) | None => IssuePriority::None,
    }
}

fn match_priority(severity: Option<CodeScanningSeverity>) -> MatchPriority {
    match issue_priority(severity) {
        IssuePriority::Critical => MatchPriority::Urgent,
        IssuePriority::High => MatchPriority::High,
        IssuePriority::Medium => MatchPriority::Normal,
        IssuePriority::Low | IssuePriority::None => MatchPriority::Low,
    }
}

/// Why an alert from `tool_name` with `severity` is excluded by `config`, if it is.
fn filter_rejection(
    config: &GitHubCodeScanningConfig,
    tool_name: &str,
    severity: Option<CodeScanningSeverity>,
) -> Option<String> {
    if !config.tool_names.is_empty() && !config.tool_names.iter().any(|name| name == tool_name) {
        return Some(format!("Tool '{}' not in tool_names", tool_name));
    }
    match severity {
        Some(severity) if config.severities.contains(&severity) => None,
        Some(severity) => Some(format!("Severity '{}' not in severities", severity)),
        None => Some("Alert has no severity".to_string()),
    }
}

/// Format an alert ID from repo and alert number.
///
/// Converts `owner/repo` + `17` to `owner:repo:alert#17`. The `:alert` segment
/// keeps alert IDs disjoint from GitHub issue IDs (`owner:repo#17`).
fn format_alert_id(repo: &str, number: i64) -> String {
    format!("{}{}{}", repo.replace('/', ":"), ALERT_ID_MARKER, number)
}

/// Parse an alert ID back into (repo, number).
fn parse_alert_id(alert_id: &str) -> Result<(String, i64)> {
    let invalid_format = || {
        Error::source(
            GitHubCodeScanningConfig::SOURCE_NAME,
            format!(
                "Invalid alert ID format '{}', expected 'owner:repo:alert#number'",
                alert_id
            ),
        )
    };

    let (repo_part, number_text) = alert_id
        .rsplit_once(ALERT_ID_MARKER)
        .ok_or_else(invalid_format)?;

    let (owner, name) = repo_part.split_once(':').ok_or_else(invalid_format)?;
    if owner.is_empty() || name.is_empty() || name.contains(':') {
        return Err(invalid_format());
    }

    let number: i64 = number_text.parse().map_err(|_| {
        Error::source(
            GitHubCodeScanningConfig::SOURCE_NAME,
            format!("Invalid alert number '{}', expected integer", number_text),
        )
    })?;

    Ok((format!("{}/{}", owner, name), number))
}

fn parse_timestamp(timestamp: &Option<String>) -> Option<DateTime<Utc>> {
    timestamp
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn format_location(path: &str, start_line: Option<i64>) -> String {
    match start_line {
        Some(line) => format!("{}:{}", path, line),
        None => path.to_string(),
    }
}

/// Map a code-scanning alert to the unified Issue type.
fn map_alert(repo: &str, alert: &CodeScanningAlert) -> Issue {
    let alert_id = format_alert_id(repo, alert.number);
    let rule_id = alert.rule.id.clone().unwrap_or_default();
    let title = alert
        .rule
        .description
        .clone()
        .or_else(|| alert.rule.name.clone())
        .unwrap_or_else(|| rule_id.clone());

    let mut issue = Issue::new(
        &alert_id,
        &alert_id,
        title,
        &alert.html_url,
        GitHubCodeScanningConfig::SOURCE_NAME,
    );

    let severity = alert_severity(alert);
    issue.priority = issue_priority(severity);
    issue.status = AlertState::from_name(&alert.state)
        .map(AlertState::issue_status)
        .unwrap_or_default();
    issue.created_at = parse_timestamp(&alert.created_at);
    issue.updated_at = parse_timestamp(&alert.updated_at);

    let instance = alert.most_recent_instance.as_ref();
    let message = instance
        .and_then(|instance| instance.message.as_ref())
        .and_then(|message| message.text.clone())
        .unwrap_or_default();
    let location = instance.and_then(|instance| instance.location.as_ref());
    let path = location.and_then(|location| location.path.clone());
    let start_line = location.and_then(|location| location.start_line);
    let end_line = location.and_then(|location| location.end_line);

    let mut description = message.clone();
    description.push_str("\n\n");
    if let Some(ref path) = path {
        description.push_str(&format!(
            "**Location:** `{}`\n",
            format_location(path, start_line)
        ));
    }
    if !rule_id.is_empty() {
        description.push_str(&format!("**Rule:** `{}`\n", rule_id));
    }
    description.push_str(&format!("**Repository:** {}\n", repo));
    description.push_str(&format!("**Alert:** {}\n", alert.html_url));
    issue.description = Some(description);

    let mut labels = vec![CODE_SCANNING_LABEL.to_string()];
    if let Some(severity) = severity {
        labels.push(severity.to_string());
    }
    labels.extend(alert.rule.tags.iter().cloned());

    issue.set_metadata(metadata::STATE, &alert.state);
    issue.set_metadata(metadata::REPO, repo);
    issue.set_metadata(metadata::NUMBER, alert.number);
    issue.set_metadata(metadata::TOOL, &alert.tool.name);
    issue.set_metadata(metadata::RULE_ID, &rule_id);
    issue.set_metadata(metadata::LABELS, labels);
    issue.set_metadata(metadata::MESSAGE, &message);
    if let Some(severity) = severity {
        issue.set_metadata(metadata::SEVERITY, severity.as_str());
    }
    if let Some(ref name) = alert.rule.name {
        issue.set_metadata(metadata::RULE_NAME, name);
    }
    if let Some(ref path) = path {
        issue.set_metadata(metadata::FILENAME, path);
    }
    if let Some(line) = start_line {
        issue.set_metadata(metadata::START_LINE, line);
    }
    if let Some(line) = end_line {
        issue.set_metadata(metadata::END_LINE, line);
    }
    if let Some(commit) = instance.and_then(|instance| instance.commit_sha.as_ref()) {
        issue.set_metadata(metadata::COMMIT, commit);
    }

    issue
}

/// Build the context string for a code-scanning alert.
///
/// `alert` is the freshly fetched alert, which carries the rule's full
/// description and help text; without it the context falls back to the
/// issue's own metadata.
fn format_code_scanning_context(issue: &Issue, alert: Option<&CodeScanningAlert>) -> String {
    let text = |key: &str| {
        issue
            .get_metadata::<String>(key)
            .filter(|value| !value.is_empty())
    };

    let mut context = format!("# GitHub Code-Scanning Alert: {}\n\n", issue.short_id);
    context.push_str(&format!("**Title:** {}\n", issue.title));
    context.push_str(&format!("**URL:** {}\n", issue.url));

    if let Some(repo) = text(metadata::REPO) {
        context.push_str(&format!("**Repository:** {}\n", repo));
    }
    if let Some(tool) = text(metadata::TOOL) {
        context.push_str(&format!("**Tool:** {}\n", tool));
    }
    if let Some(rule_id) = text(metadata::RULE_ID) {
        match text(metadata::RULE_NAME) {
            Some(name) if name != rule_id => {
                context.push_str(&format!("**Rule:** {} ({})\n", rule_id, name))
            }
            _ => context.push_str(&format!("**Rule:** {}\n", rule_id)),
        }
    }
    if let Some(severity) = text(metadata::SEVERITY) {
        context.push_str(&format!("**Severity:** {}\n", severity));
    }
    if let Some(state) = text(metadata::STATE) {
        context.push_str(&format!("**State:** {}\n", state));
    }
    if let Some(path) = text(metadata::FILENAME) {
        let start_line = issue.get_metadata::<i64>(metadata::START_LINE);
        let mut location = format_location(&path, start_line);
        if let Some(end_line) = issue.get_metadata::<i64>(metadata::END_LINE) {
            if start_line.is_some_and(|start_line| end_line > start_line) {
                location.push_str(&format!("-{}", end_line));
            }
        }
        context.push_str(&format!("**Location:** {}\n", location));
    }
    if let Some(commit) = text(metadata::COMMIT) {
        context.push_str(&format!("**Commit:** {}\n", commit));
    }

    context.push('\n');

    let message = alert
        .and_then(|alert| alert.most_recent_instance.as_ref())
        .and_then(|instance| instance.message.as_ref())
        .and_then(|message| message.text.clone())
        .filter(|message| !message.is_empty())
        .or_else(|| text(metadata::MESSAGE));
    if let Some(message) = message {
        context.push_str("## Finding\n");
        context.push_str(&message);
        context.push_str("\n\n");
    }

    let rule = alert.map(|alert| &alert.rule);
    if let Some(full_description) = rule
        .and_then(|rule| rule.full_description.as_deref())
        .filter(|description| !description.is_empty() && *description != issue.title)
    {
        context.push_str("## Rule Description\n");
        context.push_str(full_description);
        context.push_str("\n\n");
    }
    if let Some(help) = rule
        .and_then(|rule| rule.help.as_deref())
        .filter(|help| !help.is_empty())
    {
        context.push_str("## Rule Help\n");
        context.push_str(help);
        context.push_str("\n\n");
    }
    if let Some(help_uri) = rule
        .and_then(|rule| rule.help_uri.as_deref())
        .filter(|uri| !uri.is_empty())
    {
        context.push_str(&format!("**Reference:** {}\n\n", help_uri));
    }

    context
}

#[async_trait]
impl<H: HttpClient> IssueSource for GitHubCodeScanningSource<H> {
    fn name(&self) -> &str {
        GitHubCodeScanningConfig::SOURCE_NAME
    }

    fn display_name(&self) -> &str {
        DISPLAY_NAME
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        let mut issues = Vec::new();

        for repo in &self.config.repos {
            for tool_name in self.tool_queries() {
                let alerts = self
                    .client
                    .list_code_scanning_alerts(repo, tool_name)
                    .await?;

                for alert in &alerts {
                    if let Some(reason) =
                        filter_rejection(&self.config, &alert.tool.name, alert_severity(alert))
                    {
                        tracing::debug!(
                            source = GitHubCodeScanningConfig::SOURCE_NAME,
                            repo = %repo,
                            alert = alert.number,
                            reason = %reason,
                            "Skipping code-scanning alert"
                        );
                        continue;
                    }
                    issues.push(map_alert(repo, alert));
                }
            }
        }

        issues.sort_by(|a, b| a.id.cmp(&b.id));
        issues.dedup_by(|a, b| a.id == b.id);

        Ok(issues)
    }

    fn matches_criteria(&self, issue: &Issue) -> MatchResult {
        let state: String = issue.get_metadata(metadata::STATE).unwrap_or_default();
        if AlertState::from_name(&state) != Some(AlertState::Open) {
            return MatchResult::not_matched(format!("Alert state '{}' is not open", state));
        }

        let tool_name: String = issue.get_metadata(metadata::TOOL).unwrap_or_default();
        let severity = issue
            .get_metadata::<String>(metadata::SEVERITY)
            .and_then(|name| name.parse().ok());
        if let Some(reason) = filter_rejection(&self.config, &tool_name, severity) {
            return MatchResult::not_matched(reason);
        }

        MatchResult::matched(
            format!("Code-scanning alert {} matches criteria", issue.short_id),
            match_priority(severity),
        )
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        let (repo, number) = parse_alert_id(&issue.id)?;
        let alert = match self.client.get_code_scanning_alert(&repo, number).await {
            Ok(alert) => Some(alert),
            Err(error) => {
                tracing::warn!(
                    source = GitHubCodeScanningConfig::SOURCE_NAME,
                    issue_id = %issue.id,
                    error = %error,
                    "Failed to fetch code-scanning alert details; building context from the cached alert"
                );
                None
            }
        };
        Ok(format_code_scanning_context(issue, alert.as_ref()))
    }

    async fn get_issue(&self, issue_id: &str) -> Result<Issue> {
        let (repo, number) = parse_alert_id(issue_id)?;
        let alert = self.client.get_code_scanning_alert(&repo, number).await?;
        Ok(map_alert(&repo, &alert))
    }

    async fn resolve_issue(&self, issue_id: &str) -> Result<()> {
        tracing::info!(
            source = GitHubCodeScanningConfig::SOURCE_NAME,
            issue_id = %issue_id,
            "Leaving code-scanning alert open; it closes when the next scan no longer reports it"
        );
        Ok(())
    }

    async fn add_comment(&self, issue_id: &str, _comment: &str) -> Result<()> {
        tracing::info!(
            source = GitHubCodeScanningConfig::SOURCE_NAME,
            issue_id = %issue_id,
            "Code-scanning alerts do not accept comments; skipping"
        );
        Ok(())
    }

    async fn get_issue_status(&self, issue_id: &str) -> Result<String> {
        let (repo, number) = parse_alert_id(issue_id)?;
        let alert = self.client.get_code_scanning_alert(&repo, number).await?;
        Ok(alert.state)
    }

    fn is_terminal_status(&self, status: &str) -> bool {
        AlertState::from_name(status).is_some_and(AlertState::is_terminal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abnegate_http::HttpResponse;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;

    const FIXTURE: &str = include_str!("fixtures/github_code_scanning_alert.json");
    const REPO: &str = "appwrite/appwrite";
    const TOOL: &str = "ureview security review";
    const LIST_URL: &str =
        "https://api.github.com/repos/appwrite/appwrite/code-scanning/alerts?state=open&per_page=100";

    #[derive(Clone, Default)]
    struct MockHttpClient {
        responses: Arc<Mutex<HashMap<String, HttpResponse>>>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl MockHttpClient {
        fn mock_response(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
            self.responses
                .lock()
                .unwrap()
                .insert(url.into(), HttpResponse::new(status, body));
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HttpClient for MockHttpClient {
        async fn get(
            &self,
            url: &str,
            _headers: Vec<(&str, String)>,
        ) -> abnegate_http::Result<HttpResponse> {
            self.requests.lock().unwrap().push(url.to_string());
            let responses = self.responses.lock().unwrap();
            if let Some(response) = responses.get(url) {
                Ok(response.clone())
            } else {
                Ok(HttpResponse::new(404, "Not found"))
            }
        }
    }

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("fixture parses")
    }

    fn fixture_alert() -> CodeScanningAlert {
        serde_json::from_value(fixture()).expect("fixture is a code-scanning alert")
    }

    fn alert_json(number: i64, tool: &str, security_severity: Option<&str>) -> Value {
        let mut alert = fixture();
        alert["number"] = number.into();
        alert["html_url"] =
            format!("https://github.com/appwrite/appwrite/security/code-scanning/{number}").into();
        alert["tool"]["name"] = tool.into();
        alert["rule"]["security_severity_level"] = security_severity.into();
        alert
    }

    fn list_url(tool: Option<&str>, page: usize) -> String {
        let mut url = format!("{LIST_URL}&page={page}");
        if let Some(tool) = tool {
            url.push_str(&format!("&tool_name={}", urlencoding::encode(tool)));
        }
        url
    }

    fn test_config() -> GitHubCodeScanningConfig {
        GitHubCodeScanningConfig {
            enabled: true,
            repos: vec![REPO.to_string()],
            tool_names: vec![TOOL.to_string()],
            ..GitHubCodeScanningConfig::default()
        }
    }

    fn source_with(
        config: GitHubCodeScanningConfig,
    ) -> (GitHubCodeScanningSource<MockHttpClient>, MockHttpClient) {
        let mock = MockHttpClient::default();
        let source = GitHubCodeScanningSource::with_http_client(
            config,
            GitHubConfig::test_default(),
            mock.clone(),
        );
        (source, mock)
    }

    #[test]
    fn test_format_alert_id() {
        assert_eq!(format_alert_id(REPO, 17), "appwrite:appwrite:alert#17");
    }

    #[test]
    fn test_alert_id_roundtrip() {
        let alert_id = format_alert_id("my-org/my.repo", 99);
        assert_eq!(
            parse_alert_id(&alert_id).unwrap(),
            ("my-org/my.repo".to_string(), 99)
        );
        assert!(claudear_core::types::validate_issue_id(&alert_id).is_ok());
    }

    #[test]
    fn test_alert_id_does_not_collide_with_issue_id() {
        let issue_id = "appwrite:appwrite#17";
        assert_ne!(format_alert_id(REPO, 17), issue_id);
        let error = parse_alert_id(issue_id).unwrap_err().to_string();
        assert!(error.contains("Invalid alert ID format"), "{error}");
    }

    #[test]
    fn test_parse_alert_id_rejects_malformed() {
        for alert_id in [
            "",
            "appwrite:appwrite:alert#",
            "appwrite:alert#17",
            ":appwrite:alert#17",
            "a:b:c:alert#17",
        ] {
            assert!(parse_alert_id(alert_id).is_err(), "{alert_id}");
        }
        let error = parse_alert_id("appwrite:appwrite:alert#abc")
            .unwrap_err()
            .to_string();
        assert!(error.contains("Invalid alert number"), "{error}");
    }

    #[test]
    fn test_map_alert_from_fixture() {
        let issue = map_alert(REPO, &fixture_alert());

        assert_eq!(issue.id, "appwrite:appwrite:alert#17");
        assert_eq!(issue.short_id, issue.id);
        assert_eq!(
            issue.title,
            "Missing authorization check on team membership update"
        );
        assert_eq!(
            issue.url,
            "https://github.com/appwrite/appwrite/security/code-scanning/17"
        );
        assert_eq!(issue.source, GitHubCodeScanningConfig::SOURCE_NAME);
        assert_eq!(issue.status, IssueStatus::Open);
        assert_eq!(issue.priority, IssuePriority::High);
        assert_eq!(
            issue.created_at.map(|time| time.to_rfc3339()),
            Some("2026-10-30T02:14:09+00:00".to_string())
        );

        let description = issue.description.as_deref().unwrap();
        assert!(description
            .starts_with("PATCH /v1/teams/{teamId}/memberships/{membershipId} updates roles"));
        assert!(description.contains("**Location:** `app/controllers/api/teams.php:812`"));
        assert!(description.contains("**Rule:** `ureview/missing-authorization-check`"));
        assert!(description.contains("**Repository:** appwrite/appwrite"));
        assert!(description
            .contains("**Alert:** https://github.com/appwrite/appwrite/security/code-scanning/17"));

        assert_eq!(
            issue.get_metadata::<String>(metadata::STATE),
            Some("open".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>(metadata::TOOL),
            Some(TOOL.to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>(metadata::SEVERITY),
            Some("high".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>(metadata::FILENAME),
            Some("app/controllers/api/teams.php".to_string())
        );
        assert_eq!(issue.get_metadata::<i64>(metadata::NUMBER), Some(17));
        assert_eq!(
            issue.get_metadata::<Vec<String>>(metadata::LABELS),
            Some(vec![
                "code-scanning".to_string(),
                "high".to_string(),
                "security".to_string(),
                "external/cwe/cwe-862".to_string(),
            ])
        );
    }

    #[test]
    fn test_map_alert_terminal_states() {
        let cases = [
            ("fixed", IssueStatus::Resolved),
            ("closed", IssueStatus::Resolved),
            ("dismissed", IssueStatus::Ignored),
        ];
        for (state, status) in cases {
            let mut alert = fixture();
            alert["state"] = state.into();
            let alert: CodeScanningAlert = serde_json::from_value(alert).unwrap();
            assert_eq!(map_alert(REPO, &alert).status, status, "{state}");
        }
    }

    #[test]
    fn test_map_alert_without_instance() {
        let mut alert = fixture();
        alert["most_recent_instance"] = Value::Null;
        alert["rule"]["description"] = Value::Null;
        let alert: CodeScanningAlert = serde_json::from_value(alert).unwrap();

        let issue = map_alert(REPO, &alert);
        assert_eq!(issue.title, "missing-authorization-check");
        let description = issue.description.clone().unwrap();
        assert!(!description.contains("**Location:**"));
        assert!(description.contains("**Rule:** `ureview/missing-authorization-check`"));
        assert_eq!(issue.get_metadata::<String>(metadata::FILENAME), None);
    }

    #[test]
    fn test_severity_falls_back_to_rule_severity() {
        let mut alert = fixture();
        alert["rule"]["security_severity_level"] = Value::Null;
        let alert: CodeScanningAlert = serde_json::from_value(alert).unwrap();
        assert_eq!(alert_severity(&alert), Some(CodeScanningSeverity::Error));

        let mut alert = fixture();
        alert["rule"]["security_severity_level"] = Value::Null;
        alert["rule"]["severity"] = Value::Null;
        let alert: CodeScanningAlert = serde_json::from_value(alert).unwrap();
        assert_eq!(alert_severity(&alert), None);
    }

    #[test]
    fn test_severity_priorities() {
        let cases = [
            (
                CodeScanningSeverity::Critical,
                IssuePriority::Critical,
                MatchPriority::Urgent,
            ),
            (
                CodeScanningSeverity::High,
                IssuePriority::High,
                MatchPriority::High,
            ),
            (
                CodeScanningSeverity::Medium,
                IssuePriority::Medium,
                MatchPriority::Normal,
            ),
            (
                CodeScanningSeverity::Low,
                IssuePriority::Low,
                MatchPriority::Low,
            ),
            (
                CodeScanningSeverity::Error,
                IssuePriority::High,
                MatchPriority::High,
            ),
        ];
        for (severity, issue, matched) in cases {
            assert_eq!(issue_priority(Some(severity)), issue, "{severity}");
            assert_eq!(match_priority(Some(severity)), matched, "{severity}");
        }
        assert_eq!(issue_priority(None), IssuePriority::None);
    }

    #[test]
    fn test_filter_rejection_by_tool_and_severity() {
        let config = test_config();
        assert_eq!(
            filter_rejection(&config, TOOL, Some(CodeScanningSeverity::High)),
            None
        );
        assert!(
            filter_rejection(&config, "CodeQL", Some(CodeScanningSeverity::High))
                .unwrap()
                .contains("not in tool_names")
        );
        assert!(
            filter_rejection(&config, TOOL, Some(CodeScanningSeverity::Low))
                .unwrap()
                .contains("not in severities")
        );
        assert!(filter_rejection(&config, TOOL, None)
            .unwrap()
            .contains("no severity"));

        let all_tools = GitHubCodeScanningConfig {
            tool_names: Vec::new(),
            ..test_config()
        };
        assert_eq!(
            filter_rejection(&all_tools, "CodeQL", Some(CodeScanningSeverity::Medium)),
            None
        );
    }

    #[test]
    fn test_matches_criteria() {
        let (source, _) = source_with(test_config());
        let issue = map_alert(REPO, &fixture_alert());

        let result = source.matches_criteria(&issue);
        assert!(result.matches, "{}", result.reason);
        assert_eq!(result.priority, MatchPriority::High);

        let mut dismissed = issue.clone();
        dismissed.set_metadata(metadata::STATE, "dismissed");
        assert!(!source.matches_criteria(&dismissed).matches);

        let mut other_tool = issue.clone();
        other_tool.set_metadata(metadata::TOOL, "CodeQL");
        assert!(!source.matches_criteria(&other_tool).matches);

        let mut low = issue;
        low.set_metadata(metadata::SEVERITY, "low");
        assert!(!source.matches_criteria(&low).matches);
    }

    #[test]
    fn test_is_terminal_status() {
        let (source, _) = source_with(test_config());
        assert!(source.is_terminal_status("fixed"));
        assert!(source.is_terminal_status("dismissed"));
        assert!(source.is_terminal_status("closed"));
        assert!(!source.is_terminal_status("open"));
        assert!(!source.is_terminal_status(""));
        assert!(!source.is_terminal_status("resolved"));
    }

    #[test]
    fn test_source_name() {
        let (source, _) = source_with(test_config());
        assert_eq!(source.name(), "github_code_scanning");
        assert_eq!(source.display_name(), "GitHub Code Scanning");
    }

    #[test]
    fn test_format_context_from_fixture() {
        let alert = fixture_alert();
        let issue = map_alert(REPO, &alert);
        let context = format_code_scanning_context(&issue, Some(&alert));

        assert!(context.contains("# GitHub Code-Scanning Alert: appwrite:appwrite:alert#17"));
        assert!(context.contains("**Repository:** appwrite/appwrite"));
        assert!(context.contains("**Tool:** ureview security review"));
        assert!(context.contains(
            "**Rule:** ureview/missing-authorization-check (missing-authorization-check)"
        ));
        assert!(context.contains("**Severity:** high"));
        assert!(context.contains("**Location:** app/controllers/api/teams.php:812-840"));
        assert!(context.contains("**Commit:** 4f1c2b9e8d7a6b5c4d3e2f1a0b9c8d7e6f5a4b3c"));
        assert!(context.contains("## Finding\nPATCH /v1/teams/{teamId}/memberships"));
        assert!(context.contains("## Rule Description\nThe membership update endpoint"));
        assert!(context.contains("## Rule Help\nCheck that the caller holds the owner role"));
        assert!(context.contains("**Reference:** https://cwe.mitre.org/data/definitions/862.html"));
    }

    #[test]
    fn test_format_context_without_alert_details() {
        let issue = map_alert(REPO, &fixture_alert());
        let context = format_code_scanning_context(&issue, None);

        assert!(context.contains("## Finding\nPATCH /v1/teams/{teamId}/memberships"));
        assert!(!context.contains("## Rule Help"));
        assert!(!context.contains("## Rule Description"));
    }

    #[tokio::test]
    async fn test_fetch_issues_filters_by_tool_and_severity() {
        let (source, mock) = source_with(test_config());
        let alerts = Value::Array(vec![
            alert_json(1, TOOL, Some("critical")),
            alert_json(2, TOOL, Some("low")),
            alert_json(3, "CodeQL", Some("high")),
            alert_json(4, TOOL, Some("medium")),
        ]);
        mock.mock_response(list_url(Some(TOOL), 1), 200, alerts.to_string());

        let issues = source.fetch_issues().await.unwrap();
        let ids: Vec<&str> = issues.iter().map(|issue| issue.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["appwrite:appwrite:alert#1", "appwrite:appwrite:alert#4"]
        );
        assert_eq!(issues[0].priority, IssuePriority::Critical);
        assert_eq!(
            mock.requests(),
            vec![
                "https://api.github.com/repos/appwrite/appwrite/code-scanning/alerts?state=open&per_page=100&page=1&tool_name=ureview%20security%20review"
                    .to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_fetch_issues_one_request_per_tool() {
        let config = GitHubCodeScanningConfig {
            tool_names: vec![TOOL.to_string(), "CodeQL".to_string()],
            ..test_config()
        };
        let (source, mock) = source_with(config);
        mock.mock_response(
            list_url(Some(TOOL), 1),
            200,
            Value::Array(vec![alert_json(1, TOOL, Some("high"))]).to_string(),
        );
        mock.mock_response(
            list_url(Some("CodeQL"), 1),
            200,
            Value::Array(vec![alert_json(2, "CodeQL", Some("high"))]).to_string(),
        );

        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(
            mock.requests(),
            vec![list_url(Some(TOOL), 1), list_url(Some("CodeQL"), 1)]
        );
    }

    #[tokio::test]
    async fn test_fetch_issues_without_tool_names_queries_all_tools() {
        let config = GitHubCodeScanningConfig {
            tool_names: Vec::new(),
            ..test_config()
        };
        let (source, mock) = source_with(config);
        mock.mock_response(
            list_url(None, 1),
            200,
            Value::Array(vec![
                alert_json(1, TOOL, Some("high")),
                alert_json(2, "CodeQL", Some("medium")),
            ])
            .to_string(),
        );

        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(mock.requests(), vec![list_url(None, 1)]);
    }

    #[tokio::test]
    async fn test_fetch_issues_paginates() {
        let (source, mock) = source_with(test_config());
        let first_page: Vec<Value> = (1..=100)
            .map(|number| alert_json(number, TOOL, Some("high")))
            .collect();
        mock.mock_response(
            list_url(Some(TOOL), 1),
            200,
            Value::Array(first_page).to_string(),
        );
        mock.mock_response(
            list_url(Some(TOOL), 2),
            200,
            Value::Array(vec![alert_json(101, TOOL, Some("high"))]).to_string(),
        );

        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 101);
        assert!(issues
            .iter()
            .any(|issue| issue.id == "appwrite:appwrite:alert#101"));
        assert_eq!(
            mock.requests(),
            vec![list_url(Some(TOOL), 1), list_url(Some(TOOL), 2)]
        );
    }

    #[tokio::test]
    async fn test_fetch_issues_propagates_api_error() {
        let (source, mock) = source_with(test_config());
        mock.mock_response(list_url(Some(TOOL), 1), 403, "Resource not accessible");

        let error = source.fetch_issues().await.unwrap_err().to_string();
        assert!(error.contains("code-scanning alerts"), "{error}");
    }

    #[tokio::test]
    async fn test_get_issue_and_status() {
        let (source, mock) = source_with(test_config());
        let mut alert = fixture();
        alert["state"] = "fixed".into();
        mock.mock_response(
            "https://api.github.com/repos/appwrite/appwrite/code-scanning/alerts/17",
            200,
            alert.to_string(),
        );

        let issue = source
            .get_issue("appwrite:appwrite:alert#17")
            .await
            .unwrap();
        assert_eq!(issue.id, "appwrite:appwrite:alert#17");
        assert_eq!(issue.status, IssueStatus::Resolved);

        let status = source
            .get_issue_status("appwrite:appwrite:alert#17")
            .await
            .unwrap();
        assert_eq!(status, "fixed");
        assert!(source.is_terminal_status(&status));
    }

    #[tokio::test]
    async fn test_get_issue_not_found() {
        let (source, _) = source_with(test_config());
        assert!(source
            .get_issue("appwrite:appwrite:alert#999")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_build_issue_context_fetches_rule_help() {
        let (source, mock) = source_with(test_config());
        mock.mock_response(
            "https://api.github.com/repos/appwrite/appwrite/code-scanning/alerts/17",
            200,
            FIXTURE,
        );
        let issue = map_alert(REPO, &fixture_alert());

        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("## Rule Help"));
    }

    #[tokio::test]
    async fn test_build_issue_context_survives_fetch_failure() {
        let (source, _) = source_with(test_config());
        let issue = map_alert(REPO, &fixture_alert());

        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("## Finding"));
        assert!(!context.contains("## Rule Help"));
    }

    #[tokio::test]
    async fn test_resolve_and_comment_make_no_requests() {
        let (source, mock) = source_with(test_config());
        source
            .resolve_issue("appwrite:appwrite:alert#17")
            .await
            .unwrap();
        source
            .add_comment("appwrite:appwrite:alert#17", "PR opened")
            .await
            .unwrap();
        assert!(mock.requests().is_empty());
    }
}
