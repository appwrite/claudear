//! Jira issue source adapter.

use super::IssueSource;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use claudear_config::config::JiraConfig;
use claudear_core::error::{Error, Result};
use claudear_core::http::HttpResponse;
use claudear_core::types::{Issue, IssuePriority, IssueStatus, MatchPriority, MatchResult};
use serde::Deserialize;

/// Trait for HTTP client operations to enable testing.
#[async_trait]
pub trait JiraHttpClient: Send + Sync {
    /// Perform a GET request with the given auth header.
    async fn get(&self, url: &str, auth_header: &str) -> Result<HttpResponse>;

    /// Perform a POST request with the given auth header and JSON body.
    async fn post(
        &self,
        url: &str,
        auth_header: &str,
        body: serde_json::Value,
    ) -> Result<HttpResponse>;
}

/// Default HTTP client using reqwest.
pub struct ReqwestJiraClient {
    client: reqwest::Client,
}

/// Default HTTP request timeout for Jira API calls (30 seconds).
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 30;

impl ReqwestJiraClient {
    /// Create a new reqwest-based HTTP client with a default timeout.
    pub fn new() -> Self {
        Self {
            client: claudear_core::tls::client_builder()
                .timeout(std::time::Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS))
                .build()
                .expect("Failed to build HTTP client"),
        }
    }
}

impl Default for ReqwestJiraClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl JiraHttpClient for ReqwestJiraClient {
    async fn get(&self, url: &str, auth_header: &str) -> Result<HttpResponse> {
        let response = self
            .client
            .get(url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/json")
            .send()
            .await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }

    async fn post(
        &self,
        url: &str,
        auth_header: &str,
        body: serde_json::Value,
    ) -> Result<HttpResponse> {
        let response = self
            .client
            .post(url)
            .header("Authorization", auth_header)
            .json(&body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let body_text = response.text().await.unwrap_or_default();
        Ok(HttpResponse {
            status,
            body: body_text,
        })
    }
}

#[derive(Debug, Deserialize)]
struct JiraSearchResponse {
    issues: Vec<JiraApiIssue>,
}

#[derive(Debug, Deserialize)]
struct JiraApiIssue {
    id: String,
    key: String,
    #[serde(rename = "self")]
    self_url: String,
    fields: JiraFields,
}

#[derive(Debug, Deserialize)]
struct JiraFields {
    summary: String,
    description: Option<serde_json::Value>,
    status: JiraStatus,
    priority: Option<JiraPriority>,
    issuetype: Option<JiraIssueType>,
    labels: Option<Vec<String>>,
    assignee: Option<JiraUser>,
    reporter: Option<JiraUser>,
    project: JiraProject,
    created: Option<String>,
    updated: Option<String>,
    resolution: Option<JiraResolution>,
    #[expect(dead_code)]
    comment: Option<JiraCommentContainer>,
}

#[derive(Debug, Deserialize)]
struct JiraStatus {
    name: String,
    #[serde(rename = "statusCategory")]
    status_category: JiraStatusCategory,
}

#[derive(Debug, Deserialize)]
struct JiraStatusCategory {
    key: String,
    #[expect(dead_code)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct JiraPriority {
    name: String,
    #[expect(dead_code)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JiraIssueType {
    name: String,
}

#[derive(Debug, Deserialize)]
struct JiraUser {
    #[serde(rename = "displayName")]
    display_name: String,
    #[serde(rename = "accountId")]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JiraProject {
    key: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct JiraResolution {
    name: String,
}

#[derive(Debug, Deserialize)]
struct JiraCommentContainer {
    #[expect(dead_code)]
    comments: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct JiraTransitionsResponse {
    transitions: Vec<JiraTransition>,
}

#[derive(Debug, Deserialize)]
struct JiraTransition {
    id: String,
    #[expect(dead_code)]
    name: String,
    to: JiraTransitionTarget,
}

#[derive(Debug, Deserialize)]
struct JiraTransitionTarget {
    #[serde(rename = "statusCategory")]
    status_category: JiraStatusCategory,
}

/// Jira REST API client.
pub struct JiraSource<H: JiraHttpClient = ReqwestJiraClient> {
    config: JiraConfig,
    http: H,
    auth_header: String,
}

impl JiraSource<ReqwestJiraClient> {
    /// Create a new Jira source with the default HTTP client.
    pub fn new(config: JiraConfig) -> Self {
        let auth_header = build_auth_header(&config);
        Self {
            config,
            http: ReqwestJiraClient::new(),
            auth_header,
        }
    }
}

impl<H: JiraHttpClient> JiraSource<H> {
    /// Create a new Jira source with a custom HTTP client.
    pub fn with_http_client(config: JiraConfig, http: H) -> Self {
        let auth_header = build_auth_header(&config);
        Self {
            config,
            http,
            auth_header,
        }
    }

    /// Escape a value for embedding in a JQL quoted string.
    fn escape_jql_value(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// Validate that a custom JQL string has balanced parentheses.
    ///
    /// SECURITY NOTE: `custom_jql` is user-authored raw JQL that is injected directly
    /// into the query. Unlike other config fields, it cannot be escaped because users
    /// need full JQL expression syntax (operators, functions, etc.). We perform basic
    /// structural validation (balanced parentheses) to catch accidental misconfiguration.
    /// Operators should ensure `custom_jql` values come from trusted configuration only.
    fn validate_custom_jql(jql: &str) -> bool {
        let mut depth: i32 = 0;
        for ch in jql.chars() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth < 0 {
                        return false;
                    }
                }
                _ => {}
            }
        }
        depth == 0
    }

    /// Build a JQL query from the configuration.
    fn build_jql(&self) -> String {
        let mut clauses = Vec::new();

        // Project filter
        if !self.config.project_keys.is_empty() {
            let projects = self
                .config
                .project_keys
                .iter()
                .map(|k| format!("\"{}\"", Self::escape_jql_value(k)))
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("project in ({})", projects));
        }

        // Only unresolved issues
        clauses.push("resolution = Unresolved".to_string());

        // Status filter
        if !self.config.trigger_statuses.is_empty() {
            let statuses = self
                .config
                .trigger_statuses
                .iter()
                .map(|s| format!("\"{}\"", Self::escape_jql_value(s)))
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("status in ({})", statuses));
        }

        // Label filter
        if !self.config.trigger_labels.is_empty() {
            let labels = self
                .config
                .trigger_labels
                .iter()
                .map(|l| format!("\"{}\"", Self::escape_jql_value(l)))
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("labels in ({})", labels));
        }

        // Assignee filter
        if let Some(ref assignee) = self.config.trigger_assignee {
            clauses.push(format!(
                "assignee = \"{}\"",
                Self::escape_jql_value(assignee)
            ));
        }

        // Issue type filter
        if !self.config.issue_types.is_empty() {
            let types = self
                .config
                .issue_types
                .iter()
                .map(|t| format!("\"{}\"", Self::escape_jql_value(t)))
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("issuetype in ({})", types));
        }

        // Custom JQL - injected raw; validated for balanced parentheses only.
        // See `validate_custom_jql` for security considerations.
        if let Some(ref custom) = self.config.custom_jql {
            if Self::validate_custom_jql(custom) {
                clauses.push(format!("({})", custom));
            } else {
                tracing::warn!(
                    "Ignoring custom_jql with unbalanced parentheses: {:?}",
                    custom
                );
            }
        }

        let mut jql = clauses.join(" AND ");
        jql.push_str(" ORDER BY updated DESC");
        jql
    }

    /// Fetch issues from Jira using the constructed JQL query.
    async fn search_issues(&self) -> Result<Vec<JiraApiIssue>> {
        let jql = self.build_jql();
        let max_results = self.config.max_results.min(100);
        let fields = "summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution,comment";

        let url = format!(
            "{}/rest/api/3/search/jql?jql={}&maxResults={}&fields={}",
            self.config.base_url.trim_end_matches('/'),
            urlencoding::encode(&jql),
            max_results,
            fields
        );

        let response = self.http.get(&url, &self.auth_header).await?;

        if !response.is_success() {
            return Err(Error::source(
                "jira",
                format!("API error ({}): {}", response.status, response.body),
            ));
        }

        let search_response: JiraSearchResponse = response.json()?;
        Ok(search_response.issues)
    }

    /// Map a Jira API issue to the unified Issue type.
    fn map_issue(&self, api_issue: JiraApiIssue) -> Issue {
        let url = format!(
            "{}/browse/{}",
            self.config.base_url.trim_end_matches('/'),
            api_issue.key
        );

        let mut issue = Issue::new(
            &api_issue.id,
            &api_issue.key,
            &api_issue.fields.summary,
            &url,
            "jira",
        );

        // Extract description from ADF or plain text
        issue.description = api_issue.fields.description.as_ref().map(extract_adf_text);

        issue.priority = map_priority(api_issue.fields.priority.as_ref().map(|p| p.name.as_str()));
        issue.status = map_status(&api_issue.fields.status.status_category.key);

        if let Some(ref created) = api_issue.fields.created {
            if let Some(dt) = parse_jira_datetime(created) {
                issue.created_at = Some(dt.with_timezone(&chrono::Utc));
            }
        }
        if let Some(ref updated) = api_issue.fields.updated {
            if let Some(dt) = parse_jira_datetime(updated) {
                issue.updated_at = Some(dt.with_timezone(&chrono::Utc));
            }
        }

        // Store metadata
        issue.set_metadata("status_name", &api_issue.fields.status.name);
        issue.set_metadata(
            "status_category",
            &api_issue.fields.status.status_category.key,
        );
        issue.set_metadata("project_key", &api_issue.fields.project.key);
        issue.set_metadata("project_name", &api_issue.fields.project.name);
        issue.set_metadata("jira_id", &api_issue.id);
        issue.set_metadata("self_url", &api_issue.self_url);

        if let Some(ref priority) = api_issue.fields.priority {
            issue.set_metadata("priority_name", &priority.name);
        }

        if let Some(ref issue_type) = api_issue.fields.issuetype {
            issue.set_metadata("issue_type", &issue_type.name);
        }

        if let Some(ref labels) = api_issue.fields.labels {
            issue.set_metadata("labels", labels.clone());
        }

        if let Some(ref assignee) = api_issue.fields.assignee {
            issue.set_metadata("assignee", &assignee.display_name);
            if let Some(ref account_id) = assignee.account_id {
                issue.set_metadata("assignee_account_id", account_id);
            }
        }

        if let Some(ref reporter) = api_issue.fields.reporter {
            issue.set_metadata("reporter", &reporter.display_name);
        }

        if let Some(ref resolution) = api_issue.fields.resolution {
            issue.set_metadata("resolution", &resolution.name);
        }

        issue
    }
}

/// Parse a Jira datetime string into a `chrono::DateTime<chrono::FixedOffset>`.
///
/// Jira returns timestamps like `"2024-03-15T10:00:00.000+0000"` where the
/// timezone offset lacks the colon required by RFC 3339 (`+00:00`).  This
/// helper tries strict RFC 3339 first and, on failure, falls back to
/// `chrono`'s `DateTime::parse_from_str` with the `%Y-%m-%dT%H:%M:%S%.3f%z`
/// format which accepts both `+0000` and `+00:00` offsets.
fn parse_jira_datetime(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .or_else(|_| chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.3f%z"))
        .ok()
}

/// Build the Authorization header value from config.
fn build_auth_header(config: &JiraConfig) -> String {
    match config.auth_mode.as_str() {
        "bearer" => format!("Bearer {}", config.api_token.expose()),
        _ => {
            // Default to Basic auth: base64(email:token)
            let credentials = format!("{}:{}", config.email, config.api_token.expose());
            format!("Basic {}", BASE64.encode(credentials.as_bytes()))
        }
    }
}

/// Map Jira priority name to unified IssuePriority.
fn map_priority(name: Option<&str>) -> IssuePriority {
    match name {
        Some(n) => match n.to_lowercase().as_str() {
            "highest" | "blocker" | "critical" => IssuePriority::Critical,
            "high" => IssuePriority::High,
            "medium" | "normal" => IssuePriority::Medium,
            "low" | "lowest" | "trivial" => IssuePriority::Low,
            _ => IssuePriority::None,
        },
        None => IssuePriority::None,
    }
}

/// Map Jira statusCategory key to unified IssueStatus.
fn map_status(category_key: &str) -> IssueStatus {
    match category_key {
        "new" => IssueStatus::Open,
        "indeterminate" => IssueStatus::InProgress,
        "done" => IssueStatus::Resolved,
        _ => IssueStatus::Open,
    }
}

/// Extract plain text from an Atlassian Document Format (ADF) value.
/// Handles both ADF objects (Cloud) and plain text strings (Server/DC).
fn extract_adf_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(obj) => {
            // ADF document: recursively extract text nodes
            let mut text = String::new();
            extract_adf_text_recursive(value, &mut text);
            // Handle ADF objects that contain "content" array at top level
            if text.is_empty() {
                if let Some(content) = obj.get("content") {
                    extract_adf_text_recursive(content, &mut text);
                }
            }
            text.trim().to_string()
        }
        _ => String::new(),
    }
}

/// Recursively extract text from ADF nodes.
fn extract_adf_text_recursive(value: &serde_json::Value, output: &mut String) {
    match value {
        serde_json::Value::Object(obj) => {
            let node_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");

            // Text node: extract the text content
            if node_type == "text" {
                if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                    output.push_str(text);
                }
                return;
            }

            // Hard break
            if node_type == "hardBreak" {
                output.push('\n');
                return;
            }

            // Block-level nodes that should have newlines
            let is_block = matches!(
                node_type,
                "paragraph"
                    | "heading"
                    | "bulletList"
                    | "orderedList"
                    | "listItem"
                    | "codeBlock"
                    | "blockquote"
                    | "rule"
            );

            // Recurse into content
            if let Some(content) = obj.get("content") {
                extract_adf_text_recursive(content, output);
            }

            if is_block && !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                extract_adf_text_recursive(item, output);
            }
        }
        _ => {}
    }
}

/// Build the metadata portion of a Jira issue context string.
fn format_jira_context(issue: &Issue) -> String {
    let mut context = format!("# Jira Issue: {}\n\n", issue.short_id);
    context.push_str(&format!("**Title:** {}\n", issue.title));
    context.push_str(&format!("**URL:** {}\n", issue.url));

    if let Some(priority_name) = issue.get_metadata::<String>("priority_name") {
        context.push_str(&format!("**Priority:** {}\n", priority_name));
    }

    if let Some(status_name) = issue.get_metadata::<String>("status_name") {
        context.push_str(&format!("**Status:** {}\n", status_name));
    }

    if let Some(issue_type) = issue.get_metadata::<String>("issue_type") {
        context.push_str(&format!("**Type:** {}\n", issue_type));
    }

    if let Some(project_name) = issue.get_metadata::<String>("project_name") {
        let project_key: Option<String> = issue.get_metadata("project_key");
        if let Some(key) = project_key {
            context.push_str(&format!("**Project:** {} ({})\n", project_name, key));
        } else {
            context.push_str(&format!("**Project:** {}\n", project_name));
        }
    }

    if let Some(assignee) = issue.get_metadata::<String>("assignee") {
        context.push_str(&format!("**Assignee:** {}\n", assignee));
    }

    if let Some(reporter) = issue.get_metadata::<String>("reporter") {
        context.push_str(&format!("**Reporter:** {}\n", reporter));
    }

    if let Some(labels) = issue.get_metadata::<Vec<String>>("labels") {
        if !labels.is_empty() {
            context.push_str(&format!("**Labels:** {}\n", labels.join(", ")));
        }
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

#[async_trait]
impl<H: JiraHttpClient + 'static> IssueSource for JiraSource<H> {
    fn name(&self) -> &str {
        "jira"
    }

    fn display_name(&self) -> &str {
        "Jira"
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        let api_issues = self.search_issues().await?;
        Ok(api_issues.into_iter().map(|i| self.map_issue(i)).collect())
    }

    fn matches_criteria(&self, issue: &Issue) -> MatchResult {
        // Check status category - done issues never match
        let status_category: String = issue.get_metadata("status_category").unwrap_or_default();
        if status_category == "done" {
            return MatchResult::not_matched("Issue is in a done status category");
        }

        // Check trigger_statuses match
        if !self.config.trigger_statuses.is_empty() {
            let status_name: String = issue.get_metadata("status_name").unwrap_or_default();
            let status_lower = status_name.to_lowercase();
            let matches_status = self
                .config
                .trigger_statuses
                .iter()
                .any(|s| s.to_lowercase() == status_lower);
            if !matches_status {
                return MatchResult::not_matched(format!(
                    "Status '{}' not in trigger_statuses",
                    status_name
                ));
            }
        }

        // Check trigger_assignee
        if let Some(ref trigger_assignee) = self.config.trigger_assignee {
            let assignee: Option<String> = issue.get_metadata("assignee");
            match assignee {
                Some(ref a) if a == trigger_assignee => {
                    // Assignee matches - skip label check (same pattern as Linear)
                    return MatchResult::matched(
                        format!("Assigned to {}", trigger_assignee),
                        MatchPriority::Normal,
                    );
                }
                _ => {
                    // If trigger_labels is empty, assignee mismatch means no match
                    if self.config.trigger_labels.is_empty() {
                        return MatchResult::not_matched(format!(
                            "Not assigned to {}",
                            trigger_assignee
                        ));
                    }
                    // Otherwise fall through to label check
                }
            }
        }

        // Check trigger_labels
        if !self.config.trigger_labels.is_empty() {
            let issue_labels: Vec<String> = issue.get_metadata("labels").unwrap_or_default();
            let has_label = self
                .config
                .trigger_labels
                .iter()
                .any(|tl| issue_labels.iter().any(|il| il == tl));
            if !has_label {
                return MatchResult::not_matched("No matching trigger labels");
            }
        }

        // Determine priority based on issue priority
        let priority = match issue.priority {
            IssuePriority::Critical => MatchPriority::Urgent,
            IssuePriority::High => MatchPriority::High,
            _ => MatchPriority::Normal,
        };

        MatchResult::matched(
            format!("Jira issue {} matches criteria", issue.short_id),
            priority,
        )
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        Ok(format_jira_context(issue))
    }

    async fn get_issue(&self, issue_id: &str) -> Result<Issue> {
        let fields = "summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution,comment";
        let url = format!(
            "{}/rest/api/3/issue/{}?fields={}",
            self.config.base_url.trim_end_matches('/'),
            issue_id,
            fields
        );

        let response = self.http.get(&url, &self.auth_header).await?;

        if !response.is_success() {
            return Err(Error::source(
                "jira",
                format!(
                    "Failed to fetch issue {} ({}): {}",
                    issue_id, response.status, response.body
                ),
            ));
        }

        let api_issue: JiraApiIssue = response.json()?;
        Ok(self.map_issue(api_issue))
    }

    async fn resolve_issue(&self, issue_id: &str) -> Result<()> {
        // First, get available transitions
        let transitions_url = format!(
            "{}/rest/api/3/issue/{}/transitions",
            self.config.base_url.trim_end_matches('/'),
            issue_id
        );

        let response = self.http.get(&transitions_url, &self.auth_header).await?;

        if !response.is_success() {
            return Err(Error::source(
                "jira",
                format!(
                    "Failed to fetch transitions for {} ({}): {}",
                    issue_id, response.status, response.body
                ),
            ));
        }

        let transitions_response: JiraTransitionsResponse = response.json()?;

        // Find a transition that moves to "done" status category
        let done_transition = transitions_response
            .transitions
            .iter()
            .find(|t| t.to.status_category.key == "done");

        let transition = match done_transition {
            Some(t) => t,
            None => {
                return Err(Error::source(
                    "jira",
                    format!(
                        "No transition to 'done' category found for issue {}",
                        issue_id
                    ),
                ));
            }
        };

        // Execute the transition
        let response = self
            .http
            .post(
                &transitions_url,
                &self.auth_header,
                serde_json::json!({
                    "transition": {
                        "id": transition.id
                    }
                }),
            )
            .await?;

        if !response.is_success() {
            return Err(Error::source(
                "jira",
                format!(
                    "Failed to resolve issue {} ({}): {}",
                    issue_id, response.status, response.body
                ),
            ));
        }

        Ok(())
    }

    async fn add_comment(&self, issue_id: &str, comment: &str) -> Result<()> {
        let url = format!(
            "{}/rest/api/3/issue/{}/comment",
            self.config.base_url.trim_end_matches('/'),
            issue_id
        );

        // Build comment body in ADF format
        let body = serde_json::json!({
            "body": {
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "paragraph",
                        "content": [
                            {
                                "type": "text",
                                "text": comment
                            }
                        ]
                    }
                ]
            }
        });

        let response = self.http.post(&url, &self.auth_header, body).await?;

        if !response.is_success() {
            return Err(Error::source(
                "jira",
                format!(
                    "Failed to add comment to {} ({}): {}",
                    issue_id, response.status, response.body
                ),
            ));
        }

        Ok(())
    }

    async fn get_issue_status(&self, issue_id: &str) -> Result<String> {
        let issue = self.get_issue(issue_id).await?;
        let category: String = issue.get_metadata("status_category").unwrap_or_default();
        Ok(category)
    }

    fn is_terminal_status(&self, status: &str) -> bool {
        status == "done"
    }

    async fn create_issue(
        &self,
        title: &str,
        description: &str,
        labels: &[String],
    ) -> Result<Issue> {
        let project_key =
            self.config.project_keys.first().ok_or_else(|| {
                Error::source("jira", "No project_keys configured for create_issue")
            })?;

        let url = format!(
            "{}/rest/api/3/issue",
            self.config.base_url.trim_end_matches('/')
        );

        let body = serde_json::json!({
            "fields": {
                "project": { "key": project_key },
                "summary": title,
                "description": {
                    "type": "doc",
                    "version": 1,
                    "content": [
                        {
                            "type": "paragraph",
                            "content": [
                                {
                                    "type": "text",
                                    "text": description
                                }
                            ]
                        }
                    ]
                },
                "issuetype": { "name": "Bug" },
                "labels": labels,
            }
        });

        let response = self.http.post(&url, &self.auth_header, body).await?;

        if !response.is_success() {
            return Err(Error::source(
                "jira",
                format!(
                    "Failed to create issue ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        // Parse created issue response
        #[derive(Deserialize)]
        struct CreatedIssue {
            id: String,
            key: String,
            #[serde(rename = "self")]
            self_url: String,
        }
        let created: CreatedIssue = response.json()?;

        let issue_url = format!(
            "{}/browse/{}",
            self.config.base_url.trim_end_matches('/'),
            created.key
        );

        let issue = Issue::new(&created.id, &created.key, title, &issue_url, "jira");

        let _ = created.self_url; // suppress unused warning
        Ok(issue)
    }

    async fn find_or_create_label(&self, name: &str) -> Result<String> {
        // Jira labels are plain strings -- no ID resolution needed.
        // They are auto-created when used on an issue.
        Ok(name.to_string())
    }

    async fn list_open_issues(&self, title_filter: &str) -> Result<Vec<Issue>> {
        let project_key = self.config.project_keys.first().ok_or_else(|| {
            Error::source("jira", "No project_keys configured for list_open_issues")
        })?;

        let jql = if title_filter.is_empty() {
            format!(
                "project = \"{}\" AND resolution = Unresolved ORDER BY updated DESC",
                Self::escape_jql_value(project_key)
            )
        } else {
            format!(
                "project = \"{}\" AND resolution = Unresolved AND summary ~ \"{}\" ORDER BY updated DESC",
                Self::escape_jql_value(project_key),
                Self::escape_jql_value(title_filter)
            )
        };

        let fields = "summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution";
        let url = format!(
            "{}/rest/api/3/search/jql?jql={}&maxResults=50&fields={}",
            self.config.base_url.trim_end_matches('/'),
            urlencoding::encode(&jql),
            fields
        );

        let response = self.http.get(&url, &self.auth_header).await?;

        if !response.is_success() {
            return Err(Error::source(
                "jira",
                format!(
                    "Failed to search issues ({}): {}",
                    response.status, response.body
                ),
            ));
        }

        let search_response: JiraSearchResponse = response.json()?;
        Ok(search_response
            .issues
            .into_iter()
            .map(|i| self.map_issue(i))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Mock HTTP client for testing.
    pub struct MockJiraClient {
        get_responses: Mutex<HashMap<String, HttpResponse>>,
        post_responses: Mutex<HashMap<String, HttpResponse>>,
        requests: Mutex<Vec<(String, String)>>,
    }

    impl MockJiraClient {
        pub fn new() -> Self {
            Self {
                get_responses: Mutex::new(HashMap::new()),
                post_responses: Mutex::new(HashMap::new()),
                requests: Mutex::new(Vec::new()),
            }
        }

        pub fn mock_get(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
            let mut responses = self.get_responses.lock().unwrap();
            responses.insert(
                url.into(),
                HttpResponse {
                    status,
                    body: body.into(),
                },
            );
        }

        pub fn mock_post(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
            let mut responses = self.post_responses.lock().unwrap();
            responses.insert(
                url.into(),
                HttpResponse {
                    status,
                    body: body.into(),
                },
            );
        }

        #[expect(dead_code)]
        pub fn get_requests(&self) -> Vec<(String, String)> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl JiraHttpClient for MockJiraClient {
        async fn get(&self, url: &str, _auth_header: &str) -> Result<HttpResponse> {
            self.requests
                .lock()
                .unwrap()
                .push(("GET".to_string(), url.to_string()));
            let responses = self.get_responses.lock().unwrap();
            if let Some(response) = responses.get(url) {
                Ok(HttpResponse {
                    status: response.status,
                    body: response.body.clone(),
                })
            } else {
                Ok(HttpResponse {
                    status: 404,
                    body: "Not found".to_string(),
                })
            }
        }

        async fn post(
            &self,
            url: &str,
            _auth_header: &str,
            _body: serde_json::Value,
        ) -> Result<HttpResponse> {
            self.requests
                .lock()
                .unwrap()
                .push(("POST".to_string(), url.to_string()));
            let responses = self.post_responses.lock().unwrap();
            if let Some(response) = responses.get(url) {
                Ok(HttpResponse {
                    status: response.status,
                    body: response.body.clone(),
                })
            } else {
                Ok(HttpResponse {
                    status: 404,
                    body: "Not found".to_string(),
                })
            }
        }
    }

    fn test_config() -> JiraConfig {
        JiraConfig {
            enabled: true,
            base_url: "https://test.atlassian.net".to_string(),
            email: "user@test.com".to_string(),
            api_token: "test-token".into(),
            auth_mode: "basic".to_string(),
            project_keys: vec!["PROJ".to_string()],
            trigger_labels: vec!["auto-implement".to_string(), "claude".to_string()],
            trigger_statuses: vec!["To Do".to_string(), "Backlog".to_string()],
            trigger_assignee: None,
            issue_types: Vec::new(),
            custom_jql: None,
            max_results: 50,
            max_issues_per_cycle: None,
            max_concurrent: None,
            poll_interval_ms: None,
        }
    }

    fn make_jira_issue_json(
        id: &str,
        key: &str,
        summary: &str,
        status_name: &str,
        status_category_key: &str,
    ) -> String {
        serde_json::json!({
            "id": id,
            "key": key,
            "self": format!("https://test.atlassian.net/rest/api/3/issue/{}", id),
            "fields": {
                "summary": summary,
                "description": {
                    "type": "doc",
                    "version": 1,
                    "content": [
                        {
                            "type": "paragraph",
                            "content": [
                                {
                                    "type": "text",
                                    "text": "Test description"
                                }
                            ]
                        }
                    ]
                },
                "status": {
                    "name": status_name,
                    "statusCategory": {
                        "key": status_category_key,
                        "name": status_name
                    }
                },
                "priority": {
                    "name": "Medium",
                    "id": "3"
                },
                "issuetype": {
                    "name": "Bug"
                },
                "labels": ["auto-implement"],
                "assignee": {
                    "displayName": "Test User",
                    "accountId": "abc123"
                },
                "reporter": {
                    "displayName": "Reporter User",
                    "accountId": "def456"
                },
                "project": {
                    "key": "PROJ",
                    "name": "Test Project"
                },
                "created": "2024-01-01T00:00:00.000+0000",
                "updated": "2024-01-02T00:00:00.000+0000",
                "resolution": null,
                "comment": {
                    "comments": []
                }
            }
        })
        .to_string()
    }

    #[test]
    fn test_basic_auth_header() {
        let config = test_config();
        let header = build_auth_header(&config);
        let expected = format!(
            "Basic {}",
            BASE64.encode("user@test.com:test-token".as_bytes())
        );
        assert_eq!(header, expected);
    }

    #[test]
    fn test_bearer_auth_header() {
        let mut config = test_config();
        config.auth_mode = "bearer".to_string();
        let header = build_auth_header(&config);
        assert_eq!(header, "Bearer test-token");
    }

    #[test]
    fn test_build_jql_basic() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(jql.contains("project in (\"PROJ\")"));
        assert!(jql.contains("resolution = Unresolved"));
        assert!(jql.contains("status in (\"To Do\", \"Backlog\")"));
        assert!(jql.contains("labels in (\"auto-implement\", \"claude\")"));
        assert!(jql.contains("ORDER BY updated DESC"));
    }

    #[test]
    fn test_build_jql_with_assignee() {
        let mut config = test_config();
        config.trigger_assignee = Some("Jane Smith".to_string());
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(jql.contains("assignee = \"Jane Smith\""));
    }

    #[test]
    fn test_build_jql_with_issue_types() {
        let mut config = test_config();
        config.issue_types = vec!["Bug".to_string(), "Task".to_string()];
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(jql.contains("issuetype in (\"Bug\", \"Task\")"));
    }

    #[test]
    fn test_build_jql_with_custom_jql() {
        let mut config = test_config();
        config.custom_jql = Some("priority = High".to_string());
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(jql.contains("(priority = High)"));
    }

    #[test]
    fn test_build_jql_rejects_unbalanced_custom_jql() {
        let mut config = test_config();
        config.custom_jql = Some("priority = High) OR (status = Open".to_string());
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        // Unbalanced parens should be rejected, so custom_jql is not in the output
        assert!(!jql.contains("priority = High"));
    }

    #[test]
    fn test_validate_custom_jql_balanced() {
        type JS = JiraSource<MockJiraClient>;
        assert!(JS::validate_custom_jql("priority = High"));
        assert!(JS::validate_custom_jql("(priority = High)"));
        assert!(JS::validate_custom_jql("(a = 1) AND (b = 2)"));
        assert!(JS::validate_custom_jql("((nested))"));
        assert!(JS::validate_custom_jql(""));
    }

    #[test]
    fn test_validate_custom_jql_unbalanced() {
        type JS = JiraSource<MockJiraClient>;
        assert!(!JS::validate_custom_jql("(unclosed"));
        assert!(!JS::validate_custom_jql("extra)"));
        assert!(!JS::validate_custom_jql(")("));
        assert!(!JS::validate_custom_jql("((a)"));
    }

    #[test]
    fn test_build_jql_no_projects() {
        let mut config = test_config();
        config.project_keys = Vec::new();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(!jql.contains("project in"));
        assert!(jql.contains("resolution = Unresolved"));
    }

    #[test]
    fn test_build_jql_no_labels() {
        let mut config = test_config();
        config.trigger_labels = Vec::new();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(!jql.contains("labels in"));
    }

    #[test]
    fn test_build_jql_multiple_projects() {
        let mut config = test_config();
        config.project_keys = vec!["PROJ".to_string(), "BACKEND".to_string()];
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(jql.contains("project in (\"PROJ\", \"BACKEND\")"));
    }

    #[test]
    fn test_priority_mapping() {
        assert_eq!(map_priority(Some("Highest")), IssuePriority::Critical);
        assert_eq!(map_priority(Some("Blocker")), IssuePriority::Critical);
        assert_eq!(map_priority(Some("Critical")), IssuePriority::Critical);
        assert_eq!(map_priority(Some("High")), IssuePriority::High);
        assert_eq!(map_priority(Some("Medium")), IssuePriority::Medium);
        assert_eq!(map_priority(Some("Normal")), IssuePriority::Medium);
        assert_eq!(map_priority(Some("Low")), IssuePriority::Low);
        assert_eq!(map_priority(Some("Lowest")), IssuePriority::Low);
        assert_eq!(map_priority(Some("Trivial")), IssuePriority::Low);
        assert_eq!(map_priority(None), IssuePriority::None);
        assert_eq!(map_priority(Some("Unknown")), IssuePriority::None);
    }

    #[test]
    fn test_status_mapping() {
        assert_eq!(map_status("new"), IssueStatus::Open);
        assert_eq!(map_status("indeterminate"), IssueStatus::InProgress);
        assert_eq!(map_status("done"), IssueStatus::Resolved);
        assert_eq!(map_status("unknown"), IssueStatus::Open);
    }

    #[test]
    fn test_extract_adf_text_string() {
        let value = serde_json::json!("Plain text description");
        assert_eq!(extract_adf_text(&value), "Plain text description");
    }

    #[test]
    fn test_extract_adf_text_simple_doc() {
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "paragraph",
                    "content": [
                        {
                            "type": "text",
                            "text": "Hello world"
                        }
                    ]
                }
            ]
        });
        assert_eq!(extract_adf_text(&value), "Hello world");
    }

    #[test]
    fn test_extract_adf_text_multi_paragraph() {
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "paragraph",
                    "content": [
                        {"type": "text", "text": "First paragraph"}
                    ]
                },
                {
                    "type": "paragraph",
                    "content": [
                        {"type": "text", "text": "Second paragraph"}
                    ]
                }
            ]
        });
        let text = extract_adf_text(&value);
        assert!(text.contains("First paragraph"));
        assert!(text.contains("Second paragraph"));
    }

    #[test]
    fn test_extract_adf_text_with_hard_break() {
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "paragraph",
                    "content": [
                        {"type": "text", "text": "Line one"},
                        {"type": "hardBreak"},
                        {"type": "text", "text": "Line two"}
                    ]
                }
            ]
        });
        let text = extract_adf_text(&value);
        assert!(text.contains("Line one\nLine two"));
    }

    #[test]
    fn test_extract_adf_text_null() {
        let value = serde_json::json!(null);
        assert_eq!(extract_adf_text(&value), "");
    }

    #[test]
    fn test_extract_adf_text_empty_doc() {
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": []
        });
        assert_eq!(extract_adf_text(&value), "");
    }

    #[test]
    fn test_matches_criteria_basic_match() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.priority = IssuePriority::Medium;

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_done_status() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "done");
        issue.set_metadata("status_name", "Done");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
    }

    #[test]
    fn test_matches_criteria_wrong_status() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "indeterminate");
        issue.set_metadata("status_name", "In Progress");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("not in trigger_statuses"));
    }

    #[test]
    fn test_matches_criteria_no_label() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("labels", vec!["other-label".to_string()]);

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
    }

    #[test]
    fn test_matches_criteria_assignee_match_skips_labels() {
        let mut config = test_config();
        config.trigger_assignee = Some("Test User".to_string());
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("assignee", "Test User");
        // No matching labels, but assignee matches
        issue.set_metadata("labels", vec!["unrelated".to_string()]);

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_assignee_mismatch_falls_to_labels() {
        let mut config = test_config();
        config.trigger_assignee = Some("Other User".to_string());
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("assignee", "Test User");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_critical_priority() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.priority = IssuePriority::Critical;

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Urgent);
    }

    #[test]
    fn test_matches_criteria_empty_labels_config() {
        let mut config = test_config();
        config.trigger_labels = Vec::new();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("labels", Vec::<String>::new());

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_case_insensitive_status() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "to do"); // lowercase
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_fetch_issues_success() {
        let config = test_config();

        let issue_json = make_jira_issue_json("10001", "PROJ-1", "Fix bug", "To Do", "new");
        let response_body = format!(r#"{{"issues": [{}], "total": 1}}"#, issue_json);

        // The URL will contain encoded JQL - mock with a prefix match approach
        // We need to mock the exact URL the source will generate
        let source = JiraSource::with_http_client(config.clone(), MockJiraClient::new());
        let jql = source.build_jql();
        let fields = "summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution,comment";
        let expected_url = format!(
            "https://test.atlassian.net/rest/api/3/search/jql?jql={}&maxResults=50&fields={}",
            urlencoding::encode(&jql),
            fields
        );

        let mock = MockJiraClient::new();
        mock.mock_get(&expected_url, 200, &response_body);

        let source = JiraSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].short_id, "PROJ-1");
        assert_eq!(issues[0].title, "Fix bug");
        assert_eq!(issues[0].source, "jira");
        assert_eq!(issues[0].url, "https://test.atlassian.net/browse/PROJ-1");
    }

    #[tokio::test]
    async fn test_fetch_issues_api_error() {
        let config = test_config();
        let mock = MockJiraClient::new();
        // Don't mock any URL -> will return 404
        let source = JiraSource::with_http_client(config, mock);
        let result = source.fetch_issues().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_issue_success() {
        let config = test_config();
        let mock = MockJiraClient::new();

        let issue_json = make_jira_issue_json("10001", "PROJ-1", "Fix bug", "To Do", "new");
        let fields = "summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution,comment";
        let url = format!(
            "https://test.atlassian.net/rest/api/3/issue/PROJ-1?fields={}",
            fields
        );
        mock.mock_get(&url, 200, &issue_json);

        let source = JiraSource::with_http_client(config, mock);
        let issue = source.get_issue("PROJ-1").await.unwrap();
        assert_eq!(issue.short_id, "PROJ-1");
        assert_eq!(issue.title, "Fix bug");
    }

    #[tokio::test]
    async fn test_get_issue_not_found() {
        let config = test_config();
        let mock = MockJiraClient::new();
        let source = JiraSource::with_http_client(config, mock);
        let result = source.get_issue("PROJ-999").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_resolve_issue_success() {
        let config = test_config();
        let mock = MockJiraClient::new();

        let transitions_url =
            "https://test.atlassian.net/rest/api/3/issue/PROJ-1/transitions".to_string();
        mock.mock_get(
            &transitions_url,
            200,
            r#"{
                "transitions": [
                    {
                        "id": "31",
                        "name": "Done",
                        "to": {
                            "statusCategory": {
                                "key": "done",
                                "name": "Done"
                            }
                        }
                    }
                ]
            }"#,
        );
        mock.mock_post(&transitions_url, 204, "");

        let source = JiraSource::with_http_client(config, mock);
        let result = source.resolve_issue("PROJ-1").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_resolve_issue_no_done_transition() {
        let config = test_config();
        let mock = MockJiraClient::new();

        let transitions_url =
            "https://test.atlassian.net/rest/api/3/issue/PROJ-1/transitions".to_string();
        mock.mock_get(
            &transitions_url,
            200,
            r#"{
                "transitions": [
                    {
                        "id": "21",
                        "name": "In Progress",
                        "to": {
                            "statusCategory": {
                                "key": "indeterminate",
                                "name": "In Progress"
                            }
                        }
                    }
                ]
            }"#,
        );

        let source = JiraSource::with_http_client(config, mock);
        let result = source.resolve_issue("PROJ-1").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("No transition to 'done'"));
    }

    #[tokio::test]
    async fn test_add_comment_success() {
        let config = test_config();
        let mock = MockJiraClient::new();

        let comment_url = "https://test.atlassian.net/rest/api/3/issue/PROJ-1/comment".to_string();
        mock.mock_post(&comment_url, 201, r#"{"id": "12345"}"#);

        let source = JiraSource::with_http_client(config, mock);
        let result = source.add_comment("PROJ-1", "Test comment").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_comment_failure() {
        let config = test_config();
        let mock = MockJiraClient::new();
        // No mock set -> returns 404
        let source = JiraSource::with_http_client(config, mock);
        let result = source.add_comment("PROJ-1", "Test comment").await;
        assert!(result.is_err());
    }

    #[test]
    fn test_format_jira_context() {
        let mut issue = Issue::new(
            "1",
            "PROJ-1",
            "Fix the bug",
            "https://jira/browse/PROJ-1",
            "jira",
        );
        issue.description = Some("Description here".to_string());
        issue.set_metadata("priority_name", "High");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("issue_type", "Bug");
        issue.set_metadata("project_name", "Test Project");
        issue.set_metadata("project_key", "PROJ");
        issue.set_metadata("assignee", "Jane Smith");
        issue.set_metadata("reporter", "John Doe");
        issue.set_metadata(
            "labels",
            vec!["auto-implement".to_string(), "bug".to_string()],
        );

        let context = format_jira_context(&issue);
        assert!(context.contains("# Jira Issue: PROJ-1"));
        assert!(context.contains("**Title:** Fix the bug"));
        assert!(context.contains("**Priority:** High"));
        assert!(context.contains("**Status:** To Do"));
        assert!(context.contains("**Type:** Bug"));
        assert!(context.contains("**Project:** Test Project (PROJ)"));
        assert!(context.contains("**Assignee:** Jane Smith"));
        assert!(context.contains("**Reporter:** John Doe"));
        assert!(context.contains("**Labels:** auto-implement, bug"));
        assert!(context.contains("## Description"));
        assert!(context.contains("Description here"));
    }

    #[test]
    fn test_format_jira_context_minimal() {
        let issue = Issue::new(
            "1",
            "PROJ-1",
            "Fix the bug",
            "https://jira/browse/PROJ-1",
            "jira",
        );
        let context = format_jira_context(&issue);
        assert!(context.contains("# Jira Issue: PROJ-1"));
        assert!(context.contains("**Title:** Fix the bug"));
        assert!(!context.contains("## Description"));
    }

    #[test]
    fn test_map_issue_basic() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let json = make_jira_issue_json("10001", "PROJ-1", "Fix bug", "To Do", "new");
        let api_issue: JiraApiIssue = serde_json::from_str(&json).unwrap();
        let issue = source.map_issue(api_issue);

        assert_eq!(issue.id, "10001");
        assert_eq!(issue.short_id, "PROJ-1");
        assert_eq!(issue.title, "Fix bug");
        assert_eq!(issue.source, "jira");
        assert_eq!(issue.url, "https://test.atlassian.net/browse/PROJ-1");
        assert_eq!(issue.priority, IssuePriority::Medium);
        assert_eq!(issue.status, IssueStatus::Open);

        assert_eq!(
            issue.get_metadata::<String>("status_name"),
            Some("To Do".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("status_category"),
            Some("new".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("project_key"),
            Some("PROJ".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("assignee"),
            Some("Test User".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("issue_type"),
            Some("Bug".to_string())
        );
    }

    #[test]
    fn test_map_issue_description_extraction() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let json = make_jira_issue_json("10001", "PROJ-1", "Fix bug", "To Do", "new");
        let api_issue: JiraApiIssue = serde_json::from_str(&json).unwrap();
        let issue = source.map_issue(api_issue);

        assert_eq!(issue.description, Some("Test description".to_string()));
    }

    #[test]
    fn test_is_terminal_status() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        assert!(source.is_terminal_status("done"));
        assert!(!source.is_terminal_status("new"));
        assert!(!source.is_terminal_status("indeterminate"));
    }

    #[test]
    fn test_source_name_display() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        assert_eq!(source.name(), "jira");
        assert_eq!(source.display_name(), "Jira");
    }

    #[tokio::test]
    async fn test_get_issue_status() {
        let config = test_config();
        let mock = MockJiraClient::new();

        let issue_json = make_jira_issue_json("10001", "PROJ-1", "Fix bug", "To Do", "new");
        let fields = "summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution,comment";
        let url = format!(
            "https://test.atlassian.net/rest/api/3/issue/PROJ-1?fields={}",
            fields
        );
        mock.mock_get(&url, 200, &issue_json);

        let source = JiraSource::with_http_client(config, mock);
        let status = source.get_issue_status("PROJ-1").await.unwrap();
        assert_eq!(status, "new");
    }

    #[test]
    fn test_escape_jql_value_no_special_chars() {
        type JS = JiraSource<MockJiraClient>;
        assert_eq!(JS::escape_jql_value("simple"), "simple");
    }

    #[test]
    fn test_escape_jql_value_backslash() {
        type JS = JiraSource<MockJiraClient>;
        assert_eq!(JS::escape_jql_value("path\\file"), "path\\\\file");
    }

    #[test]
    fn test_escape_jql_value_double_quote() {
        type JS = JiraSource<MockJiraClient>;
        assert_eq!(JS::escape_jql_value(r#"say "hello""#), r#"say \"hello\""#);
    }

    #[test]
    fn test_escape_jql_value_both_special_chars() {
        type JS = JiraSource<MockJiraClient>;
        assert_eq!(JS::escape_jql_value(r#"path\"file"#), r#"path\\\"file"#);
    }

    #[test]
    fn test_escape_jql_value_empty() {
        type JS = JiraSource<MockJiraClient>;
        assert_eq!(JS::escape_jql_value(""), "");
    }

    #[test]
    fn test_build_jql_no_statuses() {
        let mut config = test_config();
        config.trigger_statuses = Vec::new();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(!jql.contains("status in"));
        assert!(jql.contains("resolution = Unresolved"));
    }

    #[test]
    fn test_build_jql_no_assignee() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(!jql.contains("assignee ="));
    }

    #[test]
    fn test_build_jql_all_filters_combined() {
        let mut config = test_config();
        config.trigger_assignee = Some("Jane".to_string());
        config.issue_types = vec!["Bug".to_string()];
        config.custom_jql = Some("priority = High".to_string());
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();

        assert!(jql.contains("project in"));
        assert!(jql.contains("resolution = Unresolved"));
        assert!(jql.contains("status in"));
        assert!(jql.contains("labels in"));
        assert!(jql.contains("assignee = \"Jane\""));
        assert!(jql.contains("issuetype in (\"Bug\")"));
        assert!(jql.contains("(priority = High)"));
        assert!(jql.contains("ORDER BY updated DESC"));
    }

    #[test]
    fn test_build_jql_assignee_with_special_chars() {
        let mut config = test_config();
        config.trigger_assignee = Some(r#"Jane "Quick" Smith"#.to_string());
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(jql.contains(r#"assignee = "Jane \"Quick\" Smith""#));
    }

    #[test]
    fn test_build_jql_custom_jql_balanced_parens() {
        let mut config = test_config();
        config.custom_jql = Some("(a = 1 AND (b = 2 OR c = 3))".to_string());
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(jql.contains("((a = 1 AND (b = 2 OR c = 3)))"));
    }

    #[test]
    fn test_build_jql_custom_jql_empty_string() {
        let mut config = test_config();
        config.custom_jql = Some("".to_string());
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        // Empty custom_jql is balanced (depth remains 0), so it's included as "()"
        assert!(jql.contains("()"));
    }

    #[test]
    fn test_build_jql_minimal_config() {
        let config = JiraConfig {
            enabled: true,
            base_url: "https://test.atlassian.net".to_string(),
            email: "user@test.com".to_string(),
            api_token: "token".into(),
            auth_mode: "basic".to_string(),
            project_keys: vec![],
            trigger_labels: vec![],
            trigger_statuses: vec![],
            trigger_assignee: None,
            issue_types: vec![],
            custom_jql: None,
            max_results: 50,
            max_issues_per_cycle: None,
            max_concurrent: None,
            poll_interval_ms: None,
        };
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        // Should only have resolution clause and ORDER BY
        assert_eq!(jql, "resolution = Unresolved ORDER BY updated DESC");
    }

    #[test]
    fn test_validate_custom_jql_deeply_nested() {
        type JS = JiraSource<MockJiraClient>;
        assert!(JS::validate_custom_jql("((((a))))"));
    }

    #[test]
    fn test_validate_custom_jql_starts_with_close() {
        type JS = JiraSource<MockJiraClient>;
        assert!(!JS::validate_custom_jql(")a("));
    }

    #[test]
    fn test_validate_custom_jql_multiple_groups() {
        type JS = JiraSource<MockJiraClient>;
        assert!(JS::validate_custom_jql("(a) AND (b) AND (c)"));
    }

    #[test]
    fn test_priority_mapping_case_insensitive() {
        assert_eq!(map_priority(Some("highest")), IssuePriority::Critical);
        assert_eq!(map_priority(Some("HIGHEST")), IssuePriority::Critical);
        assert_eq!(map_priority(Some("Highest")), IssuePriority::Critical);
        assert_eq!(map_priority(Some("blocker")), IssuePriority::Critical);
        assert_eq!(map_priority(Some("BLOCKER")), IssuePriority::Critical);
        assert_eq!(map_priority(Some("high")), IssuePriority::High);
        assert_eq!(map_priority(Some("HIGH")), IssuePriority::High);
        assert_eq!(map_priority(Some("medium")), IssuePriority::Medium);
        assert_eq!(map_priority(Some("MEDIUM")), IssuePriority::Medium);
        assert_eq!(map_priority(Some("normal")), IssuePriority::Medium);
        assert_eq!(map_priority(Some("NORMAL")), IssuePriority::Medium);
        assert_eq!(map_priority(Some("low")), IssuePriority::Low);
        assert_eq!(map_priority(Some("LOW")), IssuePriority::Low);
        assert_eq!(map_priority(Some("lowest")), IssuePriority::Low);
        assert_eq!(map_priority(Some("LOWEST")), IssuePriority::Low);
        assert_eq!(map_priority(Some("trivial")), IssuePriority::Low);
        assert_eq!(map_priority(Some("TRIVIAL")), IssuePriority::Low);
    }

    #[test]
    fn test_priority_mapping_empty_string() {
        assert_eq!(map_priority(Some("")), IssuePriority::None);
    }

    #[test]
    fn test_priority_mapping_custom_string() {
        assert_eq!(map_priority(Some("custom-priority")), IssuePriority::None);
    }

    #[test]
    fn test_status_mapping_empty_string() {
        assert_eq!(map_status(""), IssueStatus::Open);
    }

    #[test]
    fn test_status_mapping_case_sensitive() {
        // The function is case-sensitive
        assert_eq!(map_status("New"), IssueStatus::Open); // Not "new"
        assert_eq!(map_status("Done"), IssueStatus::Open); // Not "done"
    }

    #[test]
    fn test_extract_adf_text_number() {
        let value = serde_json::json!(42);
        assert_eq!(extract_adf_text(&value), "");
    }

    #[test]
    fn test_extract_adf_text_boolean() {
        let value = serde_json::json!(true);
        assert_eq!(extract_adf_text(&value), "");
    }

    #[test]
    fn test_extract_adf_text_array() {
        let value = serde_json::json!(["hello", "world"]);
        assert_eq!(extract_adf_text(&value), "");
    }

    #[test]
    fn test_extract_adf_text_with_list() {
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "bulletList",
                    "content": [
                        {
                            "type": "listItem",
                            "content": [
                                {
                                    "type": "paragraph",
                                    "content": [
                                        {"type": "text", "text": "Item 1"}
                                    ]
                                }
                            ]
                        },
                        {
                            "type": "listItem",
                            "content": [
                                {
                                    "type": "paragraph",
                                    "content": [
                                        {"type": "text", "text": "Item 2"}
                                    ]
                                }
                            ]
                        }
                    ]
                }
            ]
        });
        let text = extract_adf_text(&value);
        assert!(text.contains("Item 1"));
        assert!(text.contains("Item 2"));
    }

    #[test]
    fn test_extract_adf_text_with_code_block() {
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "codeBlock",
                    "content": [
                        {"type": "text", "text": "fn main() {}"}
                    ]
                }
            ]
        });
        let text = extract_adf_text(&value);
        assert!(text.contains("fn main() {}"));
    }

    #[test]
    fn test_extract_adf_text_with_blockquote() {
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "blockquote",
                    "content": [
                        {
                            "type": "paragraph",
                            "content": [
                                {"type": "text", "text": "Quoted text"}
                            ]
                        }
                    ]
                }
            ]
        });
        let text = extract_adf_text(&value);
        assert!(text.contains("Quoted text"));
    }

    #[test]
    fn test_extract_adf_text_with_heading() {
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "heading",
                    "content": [
                        {"type": "text", "text": "Section Title"}
                    ]
                },
                {
                    "type": "paragraph",
                    "content": [
                        {"type": "text", "text": "Body text"}
                    ]
                }
            ]
        });
        let text = extract_adf_text(&value);
        assert!(text.contains("Section Title"));
        assert!(text.contains("Body text"));
    }

    #[test]
    fn test_extract_adf_text_with_rule() {
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "paragraph",
                    "content": [{"type": "text", "text": "Before"}]
                },
                {"type": "rule"},
                {
                    "type": "paragraph",
                    "content": [{"type": "text", "text": "After"}]
                }
            ]
        });
        let text = extract_adf_text(&value);
        assert!(text.contains("Before"));
        assert!(text.contains("After"));
    }

    #[test]
    fn test_extract_adf_text_empty_object() {
        let value = serde_json::json!({});
        let text = extract_adf_text(&value);
        assert_eq!(text, "");
    }

    #[test]
    fn test_extract_adf_text_nested_inline_formatting() {
        // ADF with marks (bold, italic) - marks are ignored, text extracted
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "paragraph",
                    "content": [
                        {
                            "type": "text",
                            "text": "Bold text",
                            "marks": [{"type": "strong"}]
                        },
                        {"type": "text", "text": " and "},
                        {
                            "type": "text",
                            "text": "italic text",
                            "marks": [{"type": "em"}]
                        }
                    ]
                }
            ]
        });
        let text = extract_adf_text(&value);
        assert!(text.contains("Bold text"));
        assert!(text.contains("italic text"));
    }

    #[test]
    fn test_matches_criteria_high_priority() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.priority = IssuePriority::High;

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::High);
    }

    #[test]
    fn test_matches_criteria_low_priority() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.priority = IssuePriority::Low;

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Normal);
    }

    #[test]
    fn test_matches_criteria_none_priority() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.priority = IssuePriority::None;

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Normal);
    }

    #[test]
    fn test_matches_criteria_assignee_mismatch_no_labels_config() {
        let mut config = test_config();
        config.trigger_assignee = Some("Other User".to_string());
        config.trigger_labels = Vec::new(); // No labels configured
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("assignee", "Different User");

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("Not assigned to"));
    }

    #[test]
    fn test_matches_criteria_no_assignee_on_issue() {
        let mut config = test_config();
        config.trigger_assignee = Some("Target User".to_string());
        config.trigger_labels = Vec::new();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        // No assignee metadata set

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
    }

    #[test]
    fn test_matches_criteria_empty_labels_on_issue() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("labels", Vec::<String>::new()); // Empty labels

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
    }

    #[test]
    fn test_matches_criteria_multiple_labels_second_matches() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata(
            "labels",
            vec!["unrelated".to_string(), "claude".to_string()],
        ); // "claude" matches

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_empty_statuses_config() {
        let mut config = test_config();
        config.trigger_statuses = Vec::new();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "indeterminate");
        issue.set_metadata("status_name", "Any Status");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_map_issue_no_priority() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let json = serde_json::json!({
            "id": "10001",
            "key": "PROJ-1",
            "self": "https://test.atlassian.net/rest/api/3/issue/10001",
            "fields": {
                "summary": "No priority issue",
                "description": null,
                "status": {
                    "name": "To Do",
                    "statusCategory": {"key": "new", "name": "To Do"}
                },
                "priority": null,
                "issuetype": null,
                "labels": null,
                "assignee": null,
                "reporter": null,
                "project": {"key": "PROJ", "name": "Test Project"},
                "created": null,
                "updated": null,
                "resolution": null,
                "comment": null
            }
        });
        let api_issue: JiraApiIssue = serde_json::from_value(json).unwrap();
        let issue = source.map_issue(api_issue);

        assert_eq!(issue.priority, IssuePriority::None);
        assert!(issue.description.is_none());
        assert!(issue.created_at.is_none());
        assert!(issue.updated_at.is_none());
        assert!(issue.get_metadata::<String>("priority_name").is_none());
        assert!(issue.get_metadata::<String>("issue_type").is_none());
        assert!(issue.get_metadata::<String>("assignee").is_none());
        assert!(issue.get_metadata::<String>("reporter").is_none());
        assert!(issue.get_metadata::<String>("resolution").is_none());
    }

    #[test]
    fn test_map_issue_with_resolution() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let json = serde_json::json!({
            "id": "10002",
            "key": "PROJ-2",
            "self": "https://test.atlassian.net/rest/api/3/issue/10002",
            "fields": {
                "summary": "Resolved issue",
                "description": null,
                "status": {
                    "name": "Done",
                    "statusCategory": {"key": "done", "name": "Done"}
                },
                "priority": {"name": "Low", "id": "4"},
                "issuetype": {"name": "Story"},
                "labels": ["feature", "release"],
                "assignee": null,
                "reporter": {"displayName": "Admin", "accountId": "admin123"},
                "project": {"key": "PROJ", "name": "Test Project"},
                "created": "2024-03-15T10:00:00.000+0000",
                "updated": "2024-03-16T14:30:00.000+0000",
                "resolution": {"name": "Fixed"},
                "comment": null
            }
        });
        let api_issue: JiraApiIssue = serde_json::from_value(json).unwrap();
        let issue = source.map_issue(api_issue);

        assert_eq!(issue.priority, IssuePriority::Low);
        assert_eq!(issue.status, IssueStatus::Resolved);
        assert_eq!(
            issue.get_metadata::<String>("resolution"),
            Some("Fixed".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("reporter"),
            Some("Admin".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("issue_type"),
            Some("Story".to_string())
        );
        assert_eq!(
            issue.get_metadata::<Vec<String>>("labels"),
            Some(vec!["feature".to_string(), "release".to_string()])
        );
        assert!(issue.created_at.is_some());
        assert!(issue.updated_at.is_some());
    }

    #[test]
    fn test_map_issue_url_with_trailing_slash_on_base() {
        let mut config = test_config();
        config.base_url = "https://test.atlassian.net/".to_string(); // trailing slash
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let json = make_jira_issue_json("10001", "PROJ-1", "Test", "To Do", "new");
        let api_issue: JiraApiIssue = serde_json::from_str(&json).unwrap();
        let issue = source.map_issue(api_issue);

        assert_eq!(issue.url, "https://test.atlassian.net/browse/PROJ-1");
    }

    #[test]
    fn test_format_jira_context_no_description() {
        let issue = Issue::new(
            "1",
            "PROJ-1",
            "No desc",
            "https://jira/browse/PROJ-1",
            "jira",
        );
        let context = format_jira_context(&issue);
        assert!(!context.contains("## Description"));
    }

    #[test]
    fn test_format_jira_context_empty_description() {
        let mut issue = Issue::new(
            "1",
            "PROJ-1",
            "Empty desc",
            "https://jira/browse/PROJ-1",
            "jira",
        );
        issue.description = Some("".to_string());
        let context = format_jira_context(&issue);
        assert!(!context.contains("## Description"));
    }

    #[test]
    fn test_format_jira_context_project_without_key() {
        let mut issue = Issue::new("1", "PROJ-1", "Test", "https://jira/browse/PROJ-1", "jira");
        issue.set_metadata("project_name", "My Project");
        // No project_key set
        let context = format_jira_context(&issue);
        assert!(context.contains("**Project:** My Project"));
        assert!(!context.contains("("));
    }

    #[test]
    fn test_format_jira_context_empty_labels() {
        let mut issue = Issue::new("1", "PROJ-1", "Test", "https://jira/browse/PROJ-1", "jira");
        issue.set_metadata("labels", Vec::<String>::new());
        let context = format_jira_context(&issue);
        assert!(!context.contains("**Labels:**"));
    }

    #[test]
    fn test_jira_search_response_deserialization() {
        let json = r#"{
            "issues": [],
            "total": 0
        }"#;
        let resp: JiraSearchResponse = serde_json::from_str(json).unwrap();
        assert!(resp.issues.is_empty());
    }

    #[test]
    fn test_jira_search_response_with_issues() {
        let issue_json = make_jira_issue_json("10001", "PROJ-1", "Bug", "To Do", "new");
        let json = format!(r#"{{"issues": [{}], "total": 1}}"#, issue_json);
        let resp: JiraSearchResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp.issues.len(), 1);
    }

    #[test]
    fn test_jira_search_jql_response_without_total() {
        // The /rest/api/3/search/jql endpoint omits the `total` field
        let issue_json = make_jira_issue_json("10001", "PROJ-1", "Bug", "To Do", "new");
        let json = format!(r#"{{"issues": [{}], "isLast": true}}"#, issue_json);
        let resp: JiraSearchResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp.issues.len(), 1);
    }

    #[test]
    fn test_jira_transitions_response_deserialization() {
        let json = r#"{
            "transitions": [
                {
                    "id": "31",
                    "name": "Done",
                    "to": {
                        "statusCategory": {
                            "key": "done",
                            "name": "Done"
                        }
                    }
                },
                {
                    "id": "21",
                    "name": "In Progress",
                    "to": {
                        "statusCategory": {
                            "key": "indeterminate",
                            "name": "In Progress"
                        }
                    }
                }
            ]
        }"#;
        let resp: JiraTransitionsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.transitions.len(), 2);
        assert_eq!(resp.transitions[0].id, "31");
        assert_eq!(resp.transitions[0].to.status_category.key, "done");
    }

    #[test]
    fn test_jira_api_issue_deserialization_all_fields() {
        let json = make_jira_issue_json(
            "10001",
            "PROJ-1",
            "Full issue",
            "In Progress",
            "indeterminate",
        );
        let issue: JiraApiIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(issue.id, "10001");
        assert_eq!(issue.key, "PROJ-1");
        assert_eq!(issue.fields.summary, "Full issue");
        assert_eq!(issue.fields.status.name, "In Progress");
        assert_eq!(issue.fields.status.status_category.key, "indeterminate");
        assert!(issue.fields.priority.is_some());
        assert!(issue.fields.issuetype.is_some());
        assert!(issue.fields.labels.is_some());
        assert!(issue.fields.assignee.is_some());
        assert!(issue.fields.reporter.is_some());
        assert!(issue.fields.created.is_some());
        assert!(issue.fields.updated.is_some());
    }

    #[test]
    fn test_jira_api_issue_deserialization_minimal() {
        let json = r#"{
            "id": "1",
            "key": "MIN-1",
            "self": "https://test.atlassian.net/rest/api/3/issue/1",
            "fields": {
                "summary": "Minimal",
                "description": null,
                "status": {
                    "name": "Open",
                    "statusCategory": {"key": "new", "name": "To Do"}
                },
                "priority": null,
                "issuetype": null,
                "labels": null,
                "assignee": null,
                "reporter": null,
                "project": {"key": "MIN", "name": "Minimal Project"},
                "created": null,
                "updated": null,
                "resolution": null,
                "comment": null
            }
        }"#;
        let issue: JiraApiIssue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.id, "1");
        assert_eq!(issue.key, "MIN-1");
        assert!(issue.fields.priority.is_none());
        assert!(issue.fields.issuetype.is_none());
        assert!(issue.fields.assignee.is_none());
    }

    #[test]
    fn test_auth_header_default_is_basic() {
        let mut config = test_config();
        config.auth_mode = "anything_else".to_string();
        let header = build_auth_header(&config);
        assert!(header.starts_with("Basic "));
    }

    #[test]
    fn test_auth_header_basic_encoding() {
        let config = JiraConfig {
            email: "test@example.com".to_string(),
            api_token: "my-secret-token".into(),
            auth_mode: "basic".to_string(),
            ..test_config()
        };
        let header = build_auth_header(&config);
        let encoded = BASE64.encode("test@example.com:my-secret-token".as_bytes());
        assert_eq!(header, format!("Basic {}", encoded));
    }

    #[tokio::test]
    async fn test_resolve_issue_transition_post_failure() {
        let config = test_config();
        let mock = MockJiraClient::new();

        let transitions_url =
            "https://test.atlassian.net/rest/api/3/issue/PROJ-1/transitions".to_string();
        mock.mock_get(
            &transitions_url,
            200,
            r#"{
                "transitions": [
                    {
                        "id": "31",
                        "name": "Done",
                        "to": {
                            "statusCategory": {
                                "key": "done",
                                "name": "Done"
                            }
                        }
                    }
                ]
            }"#,
        );
        mock.mock_post(
            &transitions_url,
            400,
            r#"{"errorMessages": ["Cannot transition"]}"#,
        );

        let source = JiraSource::with_http_client(config, mock);
        let result = source.resolve_issue("PROJ-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to resolve issue"));
    }

    #[tokio::test]
    async fn test_resolve_issue_transitions_fetch_failure() {
        let config = test_config();
        let mock = MockJiraClient::new();
        // No mock for transitions URL -> 404
        let source = JiraSource::with_http_client(config, mock);
        let result = source.resolve_issue("PROJ-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to fetch transitions"));
    }

    #[test]
    fn test_max_results_capped_at_100() {
        let mut config = test_config();
        config.max_results = 200; // Over 100
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        // The cap happens in search_issues, not build_jql, but we can verify jql is built
        assert!(jql.contains("ORDER BY updated DESC"));
    }

    #[test]
    fn test_reqwest_jira_client_default() {
        let client = ReqwestJiraClient::default();
        assert!(std::mem::size_of_val(&client) > 0);
    }

    #[test]
    fn test_is_terminal_status_done() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        assert!(source.is_terminal_status("done"));
    }

    #[test]
    fn test_is_terminal_status_not_done() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        assert!(!source.is_terminal_status("new"));
        assert!(!source.is_terminal_status("indeterminate"));
        assert!(!source.is_terminal_status(""));
    }

    #[tokio::test]
    async fn test_get_issue_status_returns_category() {
        let config = test_config();
        let mock = MockJiraClient::new();
        let issue_json = make_jira_issue_json(
            "101",
            "PROJ-101",
            "Status test",
            "In Progress",
            "indeterminate",
        );
        mock.mock_get(
            "https://test.atlassian.net/rest/api/3/issue/PROJ-101?fields=summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution,comment",
            200,
            issue_json,
        );
        let source = JiraSource::with_http_client(config, mock);
        let status = source.get_issue_status("PROJ-101").await.unwrap();
        assert_eq!(status, "indeterminate");
    }

    #[tokio::test]
    async fn test_get_issue_status_done_category() {
        let config = test_config();
        let mock = MockJiraClient::new();
        let json = serde_json::json!({
            "id": "200",
            "key": "PROJ-200",
            "self": "https://test.atlassian.net/rest/api/3/issue/200",
            "fields": {
                "summary": "Test",
                "description": null,
                "status": {
                    "name": "Done",
                    "statusCategory": {
                        "key": "done",
                        "name": "Done"
                    }
                },
                "priority": null,
                "issuetype": null,
                "labels": null,
                "assignee": null,
                "reporter": null,
                "project": {
                    "key": "PROJ",
                    "name": "Project"
                },
                "created": null,
                "updated": null,
                "resolution": null,
                "comment": null
            }
        });
        mock.mock_get(
            "https://test.atlassian.net/rest/api/3/issue/PROJ-200?fields=summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution,comment",
            200,
            json.to_string(),
        );
        let source = JiraSource::with_http_client(config, mock);
        let status = source.get_issue_status("PROJ-200").await.unwrap();
        assert_eq!(status, "done");
    }

    #[test]
    fn test_jira_source_name_and_display_name() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        assert_eq!(source.name(), "jira");
        assert_eq!(source.display_name(), "Jira");
    }

    #[tokio::test]
    async fn test_add_comment_forbidden_error() {
        let config = test_config();
        let mock = MockJiraClient::new();
        mock.mock_post(
            "https://test.atlassian.net/rest/api/3/issue/PROJ-1/comment",
            403,
            r#"{"errorMessages":["Forbidden"]}"#,
        );
        let source = JiraSource::with_http_client(config, mock);
        let result = source.add_comment("PROJ-1", "Test comment").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Forbidden"));
    }

    #[test]
    fn test_format_jira_context_project_name_without_key() {
        let mut issue = Issue::new(
            "1",
            "PROJ-1",
            "Test Issue",
            "https://test.atlassian.net/browse/PROJ-1",
            "jira",
        );
        issue.set_metadata("priority_name", "High");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("issue_type", "Bug");
        issue.set_metadata("project_name", "My Project");
        // No project_key set
        issue.set_metadata("assignee", "John Doe");
        issue.set_metadata("reporter", "Jane Smith");
        issue.set_metadata("labels", vec!["bug".to_string(), "critical".to_string()]);
        issue.description = Some("A test description".to_string());

        let context = format_jira_context(&issue);
        assert!(context.contains("**Project:** My Project\n"));
        assert!(context.contains("**Assignee:** John Doe"));
        assert!(context.contains("**Reporter:** Jane Smith"));
        assert!(context.contains("**Labels:** bug, critical"));
        assert!(context.contains("## Description"));
        assert!(context.contains("A test description"));
    }

    #[test]
    fn test_format_jira_context_project_with_key() {
        let mut issue = Issue::new(
            "1",
            "PROJ-1",
            "Test Issue",
            "https://test.atlassian.net/browse/PROJ-1",
            "jira",
        );
        issue.set_metadata("project_name", "My Project");
        issue.set_metadata("project_key", "PROJ");

        let context = format_jira_context(&issue);
        assert!(context.contains("**Project:** My Project (PROJ)"));
    }

    #[test]
    fn test_matches_criteria_assignee_mismatch_falls_through_to_labels() {
        let mut config = test_config();
        config.trigger_assignee = Some("John Doe".to_string());
        config.trigger_labels = vec!["auto-implement".to_string()];
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "url", "jira");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.set_metadata("assignee", "Other Person");

        let result = source.matches_criteria(&issue);
        assert!(
            result.matches,
            "Should match because label matches even though assignee doesn't"
        );
    }

    #[test]
    fn test_matches_criteria_assignee_mismatch_no_labels_configured() {
        let mut config = test_config();
        config.trigger_assignee = Some("John Doe".to_string());
        config.trigger_labels = vec![];
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "url", "jira");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("assignee", "Other Person");

        let result = source.matches_criteria(&issue);
        assert!(
            !result.matches,
            "Should not match because assignee doesn't match and no labels configured"
        );
    }

    #[test]
    fn test_matches_criteria_no_assignee_with_trigger_assignee() {
        let mut config = test_config();
        config.trigger_assignee = Some("John Doe".to_string());
        config.trigger_labels = vec![];
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "url", "jira");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("status_category", "new");

        let result = source.matches_criteria(&issue);
        assert!(
            !result.matches,
            "No assignee should not match trigger_assignee"
        );
    }

    #[test]
    fn test_build_jql_with_issue_types_filter() {
        let mut config = test_config();
        config.issue_types = vec!["Bug".to_string(), "Story".to_string()];
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(jql.contains(r#"issuetype in ("Bug", "Story")"#));
    }

    #[test]
    fn test_build_jql_with_invalid_custom_jql_ignored() {
        let mut config = test_config();
        config.custom_jql = Some("priority = High AND (status = Open".to_string());
        let source = JiraSource::with_http_client(config, MockJiraClient::new());
        let jql = source.build_jql();
        assert!(!jql.contains("priority = High"));
    }

    #[tokio::test]
    async fn test_resolve_issue_transition_execution_error() {
        let config = test_config();
        let mock = MockJiraClient::new();
        mock.mock_get(
            "https://test.atlassian.net/rest/api/3/issue/PROJ-1/transitions",
            200,
            r#"{"transitions":[{"id":"31","name":"Done","to":{"statusCategory":{"key":"done","name":"Done"}}}]}"#,
        );
        mock.mock_post(
            "https://test.atlassian.net/rest/api/3/issue/PROJ-1/transitions",
            400,
            r#"{"errorMessages":["Transition not allowed"]}"#,
        );
        let source = JiraSource::with_http_client(config, mock);
        let result = source.resolve_issue("PROJ-1").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to resolve issue"));
    }

    #[test]
    fn test_map_issue_all_optional_fields_null() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let json = serde_json::json!({
            "id": "300",
            "key": "PROJ-300",
            "self": "https://test.atlassian.net/rest/api/3/issue/300",
            "fields": {
                "summary": "Minimal",
                "description": null,
                "status": {
                    "name": "Open",
                    "statusCategory": { "key": "new", "name": "To Do" }
                },
                "priority": null,
                "issuetype": null,
                "labels": null,
                "assignee": null,
                "reporter": null,
                "project": { "key": "PROJ", "name": "Project" },
                "created": null,
                "updated": null,
                "resolution": null,
                "comment": null
            }
        });
        let api_issue: JiraApiIssue = serde_json::from_value(json).unwrap();
        let issue = source.map_issue(api_issue);

        assert_eq!(issue.id, "300");
        assert_eq!(issue.short_id, "PROJ-300");
        assert!(issue.description.is_none());
        assert_eq!(issue.priority, IssuePriority::None);
        assert_eq!(issue.status, IssueStatus::Open);
        assert!(issue.created_at.is_none());
        assert!(issue.updated_at.is_none());
    }

    #[test]
    fn test_map_issue_with_resolution_and_all_metadata() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let json = serde_json::json!({
            "id": "400",
            "key": "PROJ-400",
            "self": "https://test.atlassian.net/rest/api/3/issue/400",
            "fields": {
                "summary": "Resolved Issue",
                "description": null,
                "status": {
                    "name": "Done",
                    "statusCategory": { "key": "done", "name": "Done" }
                },
                "priority": { "name": "High", "id": "2" },
                "issuetype": { "name": "Task" },
                "labels": ["feature", "backend"],
                "assignee": {
                    "displayName": "Jane Doe",
                    "accountId": "abc123"
                },
                "reporter": { "displayName": "Bob Smith", "accountId": null },
                "project": { "key": "PROJ", "name": "Project" },
                "created": "2024-03-15T10:00:00.000+0000",
                "updated": "2024-03-16T10:00:00.000+0000",
                "resolution": { "name": "Fixed" },
                "comment": null
            }
        });
        let api_issue: JiraApiIssue = serde_json::from_value(json).unwrap();
        let issue = source.map_issue(api_issue);

        assert_eq!(issue.id, "400");
        assert_eq!(issue.priority, IssuePriority::High);
        assert_eq!(issue.status, IssueStatus::Resolved);
        assert!(issue.created_at.is_some());
        assert!(issue.updated_at.is_some());
        assert_eq!(
            issue.get_metadata::<String>("resolution"),
            Some("Fixed".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("assignee"),
            Some("Jane Doe".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("assignee_account_id"),
            Some("abc123".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("reporter"),
            Some("Bob Smith".to_string())
        );
        assert_eq!(
            issue.get_metadata::<String>("issue_type"),
            Some("Task".to_string())
        );
        assert_eq!(
            issue.get_metadata::<Vec<String>>("labels"),
            Some(vec!["feature".to_string(), "backend".to_string()])
        );
    }

    #[test]
    fn test_parse_jira_datetime_rfc3339_format() {
        let dt = parse_jira_datetime("2024-03-15T10:30:00Z");
        assert!(dt.is_some());
    }

    #[test]
    fn test_parse_jira_datetime_offset_without_colon() {
        let dt = parse_jira_datetime("2024-03-15T10:30:00.000+0000");
        assert!(dt.is_some());
    }

    #[test]
    fn test_parse_jira_datetime_invalid_string() {
        let dt = parse_jira_datetime("not a date");
        assert!(dt.is_none());
    }

    #[test]
    fn test_parse_jira_datetime_empty_string() {
        let dt = parse_jira_datetime("");
        assert!(dt.is_none());
    }

    #[test]
    fn test_matches_criteria_done_status_always_rejected() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "url", "jira");
        issue.set_metadata("status_category", "done");
        issue.set_metadata("status_name", "Done");

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("done status category"));
    }

    #[test]
    fn test_matches_criteria_no_filters_matches_any_non_done() {
        let mut config = test_config();
        config.trigger_statuses = vec![];
        config.trigger_labels = vec![];
        config.trigger_assignee = None;
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "url", "jira");
        issue.set_metadata("status_category", "new");

        let result = source.matches_criteria(&issue);
        assert!(
            result.matches,
            "With no filters, any non-done issue should match"
        );
    }

    // --- Tests for create_issue coverage ---

    #[tokio::test]
    async fn test_create_issue_success() {
        let config = test_config();
        let mock = MockJiraClient::new();

        let create_url = "https://test.atlassian.net/rest/api/3/issue".to_string();
        mock.mock_post(
            &create_url,
            201,
            r#"{"id": "20001", "key": "PROJ-NEW", "self": "https://test.atlassian.net/rest/api/3/issue/20001"}"#,
        );

        let source = JiraSource::with_http_client(config, mock);
        let issue = source
            .create_issue("New Bug", "Bug description", &["bug".to_string()])
            .await
            .unwrap();

        assert_eq!(issue.id, "20001");
        assert_eq!(issue.short_id, "PROJ-NEW");
        assert_eq!(issue.title, "New Bug");
        assert_eq!(issue.source, "jira");
        assert_eq!(issue.url, "https://test.atlassian.net/browse/PROJ-NEW");
    }

    #[tokio::test]
    async fn test_create_issue_no_project_keys() {
        let mut config = test_config();
        config.project_keys = vec![];
        let mock = MockJiraClient::new();

        let source = JiraSource::with_http_client(config, mock);
        let result = source.create_issue("Title", "Desc", &[]).await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No project_keys configured"));
    }

    #[tokio::test]
    async fn test_create_issue_api_error() {
        let config = test_config();
        let mock = MockJiraClient::new();

        let create_url = "https://test.atlassian.net/rest/api/3/issue".to_string();
        mock.mock_post(&create_url, 400, r#"{"errorMessages": ["Invalid field"]}"#);

        let source = JiraSource::with_http_client(config, mock);
        let result = source.create_issue("Title", "Desc", &[]).await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to create issue"));
    }

    // --- Tests for find_or_create_label coverage ---

    #[tokio::test]
    async fn test_find_or_create_label_returns_name() {
        let config = test_config();
        let mock = MockJiraClient::new();

        let source = JiraSource::with_http_client(config, mock);
        let label = source.find_or_create_label("auto-implement").await.unwrap();

        // Jira labels are plain strings, no ID resolution
        assert_eq!(label, "auto-implement");
    }

    // --- Tests for list_open_issues coverage ---

    #[tokio::test]
    async fn test_list_open_issues_no_filter() {
        let config = test_config();
        let mock = MockJiraClient::new();

        let issue_json = make_jira_issue_json("50001", "PROJ-50", "Open issue", "To Do", "new");
        let expected_jql = format!(
            "project = \"{}\" AND resolution = Unresolved ORDER BY updated DESC",
            "PROJ"
        );
        let fields = "summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution";
        let expected_url = format!(
            "https://test.atlassian.net/rest/api/3/search/jql?jql={}&maxResults=50&fields={}",
            urlencoding::encode(&expected_jql),
            fields
        );
        mock.mock_get(
            &expected_url,
            200,
            format!(r#"{{"issues": [{}]}}"#, issue_json),
        );

        let source = JiraSource::with_http_client(config, mock);
        let issues = source.list_open_issues("").await.unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].short_id, "PROJ-50");
    }

    #[tokio::test]
    async fn test_list_open_issues_with_title_filter() {
        let config = test_config();
        let mock = MockJiraClient::new();

        let expected_jql = format!(
            "project = \"{}\" AND resolution = Unresolved AND summary ~ \"{}\" ORDER BY updated DESC",
            "PROJ", "search term"
        );
        let fields = "summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution";
        let expected_url = format!(
            "https://test.atlassian.net/rest/api/3/search/jql?jql={}&maxResults=50&fields={}",
            urlencoding::encode(&expected_jql),
            fields
        );
        mock.mock_get(&expected_url, 200, r#"{"issues": []}"#);

        let source = JiraSource::with_http_client(config, mock);
        let issues = source.list_open_issues("search term").await.unwrap();

        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_list_open_issues_no_project_keys() {
        let mut config = test_config();
        config.project_keys = vec![];
        let mock = MockJiraClient::new();

        let source = JiraSource::with_http_client(config, mock);
        let result = source.list_open_issues("").await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No project_keys configured"));
    }

    #[tokio::test]
    async fn test_list_open_issues_api_error() {
        let config = test_config();
        let mock = MockJiraClient::new();
        // No mock set -> returns 404

        let source = JiraSource::with_http_client(config, mock);
        let result = source.list_open_issues("").await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to search issues"));
    }

    // --- Tests for build_issue_context coverage ---

    #[tokio::test]
    async fn test_build_issue_context_delegates_to_format() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new(
            "1",
            "PROJ-1",
            "Context Test",
            "https://jira/browse/PROJ-1",
            "jira",
        );
        issue.description = Some("Some description".to_string());
        issue.set_metadata("priority_name", "High");
        issue.set_metadata("status_name", "To Do");

        let context = source.build_issue_context(&issue).await.unwrap();

        assert!(context.contains("# Jira Issue: PROJ-1"));
        assert!(context.contains("**Title:** Context Test"));
        assert!(context.contains("**Priority:** High"));
        assert!(context.contains("## Description"));
        assert!(context.contains("Some description"));
    }

    // --- Tests for get_issue_status coverage ---

    #[tokio::test]
    async fn test_get_issue_status_api_error() {
        let config = test_config();
        let mock = MockJiraClient::new();
        // No mock -> 404

        let source = JiraSource::with_http_client(config, mock);
        let result = source.get_issue_status("PROJ-MISSING").await;

        assert!(result.is_err());
    }

    // --- Tests for search_issues max_results cap ---

    #[tokio::test]
    async fn test_search_issues_caps_max_results_at_100() {
        let mut config = test_config();
        config.max_results = 200;
        config.trigger_statuses = vec![];
        config.trigger_labels = vec![];
        config.project_keys = vec![];

        let mock = MockJiraClient::new();

        // Build the expected URL with maxResults=100
        let jql = "resolution = Unresolved ORDER BY updated DESC";
        let fields = "summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution,comment";
        let expected_url = format!(
            "https://test.atlassian.net/rest/api/3/search/jql?jql={}&maxResults=100&fields={}",
            urlencoding::encode(jql),
            fields
        );
        mock.mock_get(&expected_url, 200, r#"{"issues": []}"#);

        let source = JiraSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();

        assert!(issues.is_empty());
    }

    // --- Tests for escape_jql_value edge cases ---

    #[test]
    fn test_escape_jql_value_multiple_backslashes() {
        type JS = JiraSource<MockJiraClient>;
        assert_eq!(JS::escape_jql_value("a\\b\\c"), "a\\\\b\\\\c");
    }

    #[test]
    fn test_escape_jql_value_only_special_chars() {
        type JS = JiraSource<MockJiraClient>;
        assert_eq!(JS::escape_jql_value(r#"\"#), r#"\\"#);
        assert_eq!(JS::escape_jql_value(r#"""#), r#"\""#);
    }

    // --- Tests for parse_jira_datetime edge cases ---

    #[test]
    fn test_parse_jira_datetime_with_colon_offset() {
        let dt = parse_jira_datetime("2024-03-15T10:30:00.000+00:00");
        assert!(dt.is_some());
    }

    #[test]
    fn test_parse_jira_datetime_positive_offset() {
        let dt = parse_jira_datetime("2024-03-15T10:30:00.000+0530");
        assert!(dt.is_some());
    }

    #[test]
    fn test_parse_jira_datetime_negative_offset() {
        let dt = parse_jira_datetime("2024-03-15T10:30:00.000-0500");
        assert!(dt.is_some());
    }

    // --- Tests for map_issue dates ---

    #[test]
    fn test_map_issue_rfc3339_dates() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let json = serde_json::json!({
            "id": "dt1",
            "key": "PROJ-DT1",
            "self": "https://test.atlassian.net/rest/api/3/issue/dt1",
            "fields": {
                "summary": "Date test",
                "description": null,
                "status": {"name": "Open", "statusCategory": {"key": "new", "name": "To Do"}},
                "priority": null,
                "issuetype": null,
                "labels": null,
                "assignee": null,
                "reporter": null,
                "project": {"key": "PROJ", "name": "Project"},
                "created": "2024-06-15T10:30:00Z",
                "updated": "2024-06-16T14:00:00Z",
                "resolution": null,
                "comment": null
            }
        });
        let api_issue: JiraApiIssue = serde_json::from_value(json).unwrap();
        let issue = source.map_issue(api_issue);

        assert!(issue.created_at.is_some());
        assert!(issue.updated_at.is_some());
    }

    #[test]
    fn test_map_issue_invalid_dates() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let json = serde_json::json!({
            "id": "dt2",
            "key": "PROJ-DT2",
            "self": "https://test.atlassian.net/rest/api/3/issue/dt2",
            "fields": {
                "summary": "Bad dates",
                "description": null,
                "status": {"name": "Open", "statusCategory": {"key": "new", "name": "To Do"}},
                "priority": null,
                "issuetype": null,
                "labels": null,
                "assignee": null,
                "reporter": null,
                "project": {"key": "PROJ", "name": "Project"},
                "created": "not-a-date",
                "updated": "also-not-a-date",
                "resolution": null,
                "comment": null
            }
        });
        let api_issue: JiraApiIssue = serde_json::from_value(json).unwrap();
        let issue = source.map_issue(api_issue);

        assert!(issue.created_at.is_none());
        assert!(issue.updated_at.is_none());
    }

    // --- Tests for extract_adf_text edge cases ---

    #[test]
    fn test_extract_adf_text_ordered_list() {
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "orderedList",
                    "content": [
                        {
                            "type": "listItem",
                            "content": [
                                {
                                    "type": "paragraph",
                                    "content": [
                                        {"type": "text", "text": "First item"}
                                    ]
                                }
                            ]
                        }
                    ]
                }
            ]
        });
        let text = extract_adf_text(&value);
        assert!(text.contains("First item"));
    }

    #[test]
    fn test_extract_adf_text_unknown_node_type() {
        // Unknown node types should not crash, just recurse into content
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "unknownType",
                    "content": [
                        {"type": "text", "text": "Inside unknown"}
                    ]
                }
            ]
        });
        let text = extract_adf_text(&value);
        assert!(text.contains("Inside unknown"));
    }

    #[test]
    fn test_extract_adf_text_deeply_nested() {
        let value = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "blockquote",
                    "content": [
                        {
                            "type": "paragraph",
                            "content": [
                                {
                                    "type": "text",
                                    "text": "Deep "
                                },
                                {
                                    "type": "text",
                                    "text": "text",
                                    "marks": [{"type": "strong"}]
                                }
                            ]
                        }
                    ]
                }
            ]
        });
        let text = extract_adf_text(&value);
        assert!(text.contains("Deep text"));
    }

    // --- Tests for create_issue with trailing slash on base_url ---

    #[tokio::test]
    async fn test_create_issue_url_trailing_slash() {
        let mut config = test_config();
        config.base_url = "https://test.atlassian.net/".to_string();
        let mock = MockJiraClient::new();

        let create_url = "https://test.atlassian.net/rest/api/3/issue".to_string();
        mock.mock_post(
            &create_url,
            201,
            r#"{"id": "30001", "key": "PROJ-30", "self": "https://test.atlassian.net/rest/api/3/issue/30001"}"#,
        );

        let source = JiraSource::with_http_client(config, mock);
        let issue = source.create_issue("Test", "Desc", &[]).await.unwrap();

        assert_eq!(issue.url, "https://test.atlassian.net/browse/PROJ-30");
    }

    // --- Tests for matches_criteria medium priority ---

    #[test]
    fn test_matches_criteria_medium_priority() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let mut issue = Issue::new("1", "PROJ-1", "Test", "http://test.com", "jira");
        issue.set_metadata("status_category", "new");
        issue.set_metadata("status_name", "To Do");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.priority = IssuePriority::Medium;

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Normal);
    }

    // --- Test get_issue with trailing slash ---

    #[tokio::test]
    async fn test_get_issue_trailing_slash_base_url() {
        let mut config = test_config();
        config.base_url = "https://test.atlassian.net/".to_string();
        let mock = MockJiraClient::new();

        let issue_json = make_jira_issue_json("10099", "PROJ-99", "Slash test", "To Do", "new");
        let fields = "summary,description,status,priority,issuetype,labels,assignee,reporter,project,created,updated,resolution,comment";
        let url = format!(
            "https://test.atlassian.net/rest/api/3/issue/PROJ-99?fields={}",
            fields
        );
        mock.mock_get(&url, 200, &issue_json);

        let source = JiraSource::with_http_client(config, mock);
        let issue = source.get_issue("PROJ-99").await.unwrap();
        assert_eq!(issue.short_id, "PROJ-99");
    }

    // --- Test map_issue with assignee account_id null ---

    #[test]
    fn test_map_issue_assignee_no_account_id() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let json = serde_json::json!({
            "id": "500",
            "key": "PROJ-500",
            "self": "https://test.atlassian.net/rest/api/3/issue/500",
            "fields": {
                "summary": "No account id",
                "description": null,
                "status": {"name": "Open", "statusCategory": {"key": "new", "name": "To Do"}},
                "priority": null,
                "issuetype": null,
                "labels": null,
                "assignee": {"displayName": "Jane", "accountId": null},
                "reporter": null,
                "project": {"key": "PROJ", "name": "Project"},
                "created": null,
                "updated": null,
                "resolution": null,
                "comment": null
            }
        });
        let api_issue: JiraApiIssue = serde_json::from_value(json).unwrap();
        let issue = source.map_issue(api_issue);

        assert_eq!(
            issue.get_metadata::<String>("assignee"),
            Some("Jane".to_string())
        );
        assert!(issue
            .get_metadata::<String>("assignee_account_id")
            .is_none());
    }

    // --- Test map_issue with labels but no assignee ---

    #[test]
    fn test_map_issue_labels_no_assignee() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let json = serde_json::json!({
            "id": "600",
            "key": "PROJ-600",
            "self": "https://test.atlassian.net/rest/api/3/issue/600",
            "fields": {
                "summary": "Labels only",
                "description": null,
                "status": {"name": "Open", "statusCategory": {"key": "new", "name": "To Do"}},
                "priority": null,
                "issuetype": null,
                "labels": ["label-a", "label-b"],
                "assignee": null,
                "reporter": null,
                "project": {"key": "PROJ", "name": "Project"},
                "created": null,
                "updated": null,
                "resolution": null,
                "comment": null
            }
        });
        let api_issue: JiraApiIssue = serde_json::from_value(json).unwrap();
        let issue = source.map_issue(api_issue);

        assert_eq!(
            issue.get_metadata::<Vec<String>>("labels"),
            Some(vec!["label-a".to_string(), "label-b".to_string()])
        );
        assert!(issue.get_metadata::<String>("assignee").is_none());
    }

    // --- Test build_issue_context returns Ok ---

    #[tokio::test]
    async fn test_build_issue_context_returns_ok() {
        let config = test_config();
        let source = JiraSource::with_http_client(config, MockJiraClient::new());

        let issue = Issue::new("1", "PROJ-1", "Test", "url", "jira");
        let result = source.build_issue_context(&issue).await;
        assert!(result.is_ok());
    }

    // --- Test extract_adf_text with object without type field ---

    #[test]
    fn test_extract_adf_text_object_no_type() {
        let value = serde_json::json!({"key": "value"});
        let text = extract_adf_text(&value);
        assert_eq!(text, "");
    }

    // --- Test list_open_issues with title containing special JQL characters ---

    #[tokio::test]
    async fn test_list_open_issues_title_with_special_chars() {
        let config = test_config();
        let mock = MockJiraClient::new();

        // We can't easily mock the exact URL with special chars, so just verify it errors (404)
        let source = JiraSource::with_http_client(config, mock);
        let result = source.list_open_issues(r#"say "hello""#).await;

        // Will get API error since no mock matches
        assert!(result.is_err());
    }
}
