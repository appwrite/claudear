//! Linear issue source adapter.

use super::IssueSource;
use async_trait::async_trait;
use claudear_config::config::LinearConfig;
use claudear_core::error::{Error, Result};
use claudear_core::http::HttpResponse;
use claudear_core::types::{Issue, IssuePriority, IssueStatus, MatchPriority, MatchResult};
use serde::{Deserialize, Serialize};

/// Trait for GraphQL client operations to enable testing.
#[async_trait]
pub trait LinearHttpClient: Send + Sync {
    /// Perform a POST request with JSON body.
    async fn post(&self, url: &str, api_key: &str, body: serde_json::Value)
        -> Result<HttpResponse>;
}

/// Default HTTP client using reqwest.
pub struct ReqwestLinearClient {
    client: reqwest::Client,
}

/// Default HTTP request timeout for Linear API calls (30 seconds).
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 30;

impl ReqwestLinearClient {
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

impl Default for ReqwestLinearClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LinearHttpClient for ReqwestLinearClient {
    async fn post(
        &self,
        url: &str,
        api_key: &str,
        body: serde_json::Value,
    ) -> Result<HttpResponse> {
        let response = self
            .client
            .post(url)
            .header("Authorization", api_key)
            .header("Content-Type", "application/json")
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

/// Linear GraphQL API client.
pub struct LinearSource<H: LinearHttpClient = ReqwestLinearClient> {
    config: LinearConfig,
    http: H,
}

// GraphQL types
#[derive(Debug, Serialize)]
struct GraphQLRequest {
    query: &'static str,
    variables: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct GraphQLResponse<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQLError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct IssuesResponse {
    issues: IssuesConnection,
}

#[derive(Debug, Deserialize)]
struct IssuesConnection {
    nodes: Vec<LinearIssue>,
}

#[derive(Debug, Deserialize)]
struct IssueResponse {
    issue: Option<LinearIssue>,
}

#[derive(Debug, Deserialize)]
struct LinearIssue {
    id: String,
    identifier: String,
    title: String,
    description: Option<String>,
    url: String,
    priority: i32,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    state: Option<LinearState>,
    labels: LabelsConnection,
    team: Option<LinearTeam>,
    project: Option<LinearProject>,
    assignee: Option<LinearUser>,
}

#[derive(Debug, Deserialize)]
struct LinearState {
    name: String,
    #[serde(rename = "type")]
    state_type: String,
}

#[derive(Debug, Deserialize)]
struct LabelsConnection {
    nodes: Vec<LinearLabel>,
}

#[derive(Debug, Deserialize)]
struct LinearLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct LinearTeam {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct LinearProject {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct LinearUser {
    name: String,
}

const ISSUES_QUERY: &str = r#"
query Issues($filter: IssueFilter, $first: Int) {
  issues(filter: $filter, first: $first, orderBy: updatedAt) {
    nodes {
      id
      identifier
      title
      description
      url
      priority
      createdAt
      updatedAt
      state {
        name
        type
      }
      labels {
        nodes {
          name
        }
      }
      team {
        id
        name
      }
      project {
        id
        name
      }
      assignee {
        name
      }
    }
  }
}
"#;

const ISSUE_QUERY: &str = r#"
query Issue($id: String!) {
  issue(id: $id) {
    id
    identifier
    title
    description
    url
    priority
    createdAt
    updatedAt
    state {
      name
      type
    }
    labels {
      nodes {
        name
      }
    }
    team {
      id
      name
    }
    project {
      id
      name
    }
    assignee {
      name
    }
  }
}
"#;

impl LinearSource<ReqwestLinearClient> {
    /// Create a new Linear source with the default HTTP client.
    pub fn new(config: LinearConfig) -> Self {
        Self {
            config,
            http: ReqwestLinearClient::new(),
        }
    }
}

impl<H: LinearHttpClient> LinearSource<H> {
    /// Create a new Linear source with a custom HTTP client.
    pub fn with_http_client(config: LinearConfig, http: H) -> Self {
        Self { config, http }
    }

    async fn graphql<T: for<'de> Deserialize<'de>>(
        &self,
        query: &'static str,
        variables: serde_json::Value,
    ) -> Result<T> {
        let request = GraphQLRequest { query, variables };
        let body = serde_json::to_value(&request)
            .map_err(|e| Error::Other(format!("JSON error: {}", e)))?;

        let response = self
            .http
            .post(
                "https://api.linear.app/graphql",
                self.config.api_key.expose(),
                body,
            )
            .await?;

        if !response.is_success() {
            return Err(Error::source(
                "linear",
                format!("API error: {}", response.body),
            ));
        }

        let gql_response: GraphQLResponse<T> = response.json()?;

        if let Some(errors) = gql_response.errors {
            let messages: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
            return Err(Error::source("linear", messages.join(", ")));
        }

        gql_response
            .data
            .ok_or_else(|| Error::source("linear", "No data in response"))
    }

    fn map_issue(&self, issue: LinearIssue) -> Issue {
        let labels: Vec<String> = issue.labels.nodes.iter().map(|l| l.name.clone()).collect();

        let mut mapped = Issue::new(issue.id, issue.identifier, issue.title, issue.url, "linear");

        mapped.description = issue.description;
        mapped.priority = Self::map_priority(issue.priority);
        mapped.status = Self::map_status(issue.state.as_ref().map(|s| s.state_type.as_str()));

        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&issue.created_at) {
            mapped.created_at = Some(dt.with_timezone(&chrono::Utc));
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&issue.updated_at) {
            mapped.updated_at = Some(dt.with_timezone(&chrono::Utc));
        }

        // Store metadata
        if let Some(ref state) = issue.state {
            mapped.set_metadata("state_name", &state.name);
            mapped.set_metadata("state_type", &state.state_type);
        }
        mapped.set_metadata("labels", &labels);
        if let Some(ref team) = issue.team {
            mapped.set_metadata("team", &team.name);
            mapped.set_metadata("team_id", &team.id);
        }
        if let Some(ref project) = issue.project {
            mapped.set_metadata("project", &project.name);
            mapped.set_metadata("project_id", &project.id);
        }
        if let Some(ref assignee) = issue.assignee {
            mapped.set_metadata("assignee", &assignee.name);
        }

        mapped
    }

    fn map_priority(priority: i32) -> IssuePriority {
        match priority {
            1 => IssuePriority::Critical,
            2 => IssuePriority::High,
            3 => IssuePriority::Medium,
            4 => IssuePriority::Low,
            _ => IssuePriority::None,
        }
    }

    fn map_status(state_type: Option<&str>) -> IssueStatus {
        match state_type {
            Some("completed") | Some("canceled") => IssueStatus::Resolved,
            Some("started") => IssueStatus::InProgress,
            _ => IssueStatus::Open,
        }
    }

    /// Check if a Linear state type represents a terminal state.
    /// Terminal states are those where the issue is considered "done" - no further action needed.
    pub fn is_issue_terminal(state_type: &str) -> bool {
        let s = state_type.to_lowercase();
        s == "completed" || s == "canceled" || s == "cancelled"
    }
}

/// Build a context string from a Linear issue's metadata.
/// This is a pure function extracted from the async trait method for testability.
fn format_linear_context(issue: &Issue) -> String {
    let mut context = format!("# Linear Issue: {}\n\n", issue.short_id);
    context.push_str(&format!("**Title:** {}\n", issue.title));
    context.push_str(&format!("**URL:** {}\n", issue.url));
    context.push_str(&format!("**Priority:** {}\n", issue.priority));
    context.push_str(&format!("**Status:** {}\n\n", issue.status));

    if let Some(ref description) = issue.description {
        context.push_str(&format!("## Description\n{}\n\n", description));
    }

    if let Some(team) = issue.get_metadata::<String>("team") {
        context.push_str(&format!("**Team:** {}\n", team));
    }
    if let Some(project) = issue.get_metadata::<String>("project") {
        context.push_str(&format!("**Project:** {}\n", project));
    }
    if let Some(assignee) = issue.get_metadata::<String>("assignee") {
        context.push_str(&format!("**Assignee:** {}\n", assignee));
    }

    context
}

#[async_trait]
impl<H: LinearHttpClient + 'static> IssueSource for LinearSource<H> {
    fn name(&self) -> &str {
        "linear"
    }

    fn display_name(&self) -> &str {
        "Linear"
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        let mut filter = serde_json::Map::new();

        if let Some(ref team_id) = self.config.team_id {
            if !team_id.is_empty() {
                filter.insert(
                    "team".to_string(),
                    serde_json::json!({ "id": { "eq": team_id } }),
                );
            }
        }

        if let Some(ref project_id) = self.config.project_id {
            if !project_id.is_empty() {
                filter.insert(
                    "project".to_string(),
                    serde_json::json!({ "id": { "eq": project_id } }),
                );
            }
        }

        if let Some(ref assignee) = self.config.trigger_assignee {
            if !assignee.is_empty() {
                filter.insert(
                    "assignee".to_string(),
                    serde_json::json!({
                        "displayName": { "eqCaseInsensitive": assignee }
                    }),
                );
            }
        }

        // Only require label filter when trigger_labels is non-empty.
        // When trigger_assignee is set and labels are empty, skip label filtering.
        if !self.config.trigger_labels.is_empty() {
            filter.insert(
                "labels".to_string(),
                serde_json::json!({
                    "some": { "name": { "in": self.config.trigger_labels } }
                }),
            );
        }

        let variables = serde_json::json!({
            "filter": filter,
            "first": 50
        });

        let response: IssuesResponse = self.graphql(ISSUES_QUERY, variables).await?;

        Ok(response
            .issues
            .nodes
            .into_iter()
            .map(|i| self.map_issue(i))
            .collect())
    }

    fn matches_criteria(&self, issue: &Issue) -> MatchResult {
        let state_name: Option<String> = issue.get_metadata("state_name");
        let state_type: Option<String> = issue.get_metadata("state_type");
        let labels: Vec<String> = issue.get_metadata("labels").unwrap_or_default();

        // Check state
        if !self.config.trigger_states.is_empty() {
            let state_name_lower = state_name.as_deref().unwrap_or("").to_lowercase();
            let state_type_lower = state_type.as_deref().unwrap_or("").to_lowercase();

            let state_matches = self.config.trigger_states.iter().any(|s| {
                let s_lower = s.to_lowercase();
                state_name_lower.contains(&s_lower) || state_type_lower.contains(&s_lower)
            });

            if !state_matches {
                return MatchResult::not_matched(format!(
                    "State \"{}\" not in trigger states",
                    state_name.as_deref().unwrap_or("unknown")
                ));
            }
        }

        // Check assignee
        if let Some(ref trigger_assignee) = self.config.trigger_assignee {
            let issue_assignee: Option<String> = issue.get_metadata("assignee");
            let assignee_matches = issue_assignee
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(trigger_assignee));

            if !assignee_matches {
                return MatchResult::not_matched(format!(
                    "Assignee \"{}\" does not match trigger assignee \"{}\"",
                    issue_assignee.as_deref().unwrap_or("unassigned"),
                    trigger_assignee
                ));
            }
        }

        // Check labels (skip if trigger_assignee is set and trigger_labels is empty)
        let skip_label_check =
            self.config.trigger_assignee.is_some() && self.config.trigger_labels.is_empty();
        if !skip_label_check && !self.config.trigger_labels.is_empty() {
            let label_matches = self.config.trigger_labels.iter().any(|trigger| {
                labels
                    .iter()
                    .any(|l| l.to_lowercase() == trigger.to_lowercase())
            });

            if !label_matches {
                return MatchResult::not_matched("No matching trigger labels");
            }
        }

        // Determine priority
        let priority = match issue.priority {
            IssuePriority::Critical => MatchPriority::Urgent,
            IssuePriority::High => MatchPriority::High,
            IssuePriority::Low => MatchPriority::Low,
            _ => MatchPriority::Normal,
        };

        MatchResult::matched("Matches state and label criteria", priority)
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        Ok(format_linear_context(issue))
    }

    async fn get_issue(&self, issue_id: &str) -> Result<Issue> {
        let variables = serde_json::json!({
            "id": issue_id
        });

        let response: IssueResponse = self.graphql(ISSUE_QUERY, variables).await?;

        response
            .issue
            .map(|i| self.map_issue(i))
            .ok_or_else(|| Error::issue_not_found("linear", issue_id))
    }

    async fn resolve_issue(&self, issue_id: &str) -> Result<()> {
        // First, get the "Done" state ID for this issue's team
        let issue = self.get_issue(issue_id).await?;
        let team_id = issue
            .get_metadata::<String>("team_id")
            .ok_or_else(|| Error::source("linear", "Issue has no team_id"))?;

        // Query for team's workflow states to find "Done" state
        #[derive(Debug, Deserialize)]
        struct TeamStatesResponse {
            team: Option<TeamWithStates>,
        }
        #[derive(Debug, Deserialize)]
        struct TeamWithStates {
            states: StatesConnection,
        }
        #[derive(Debug, Deserialize)]
        struct StatesConnection {
            nodes: Vec<WorkflowState>,
        }
        #[derive(Debug, Deserialize)]
        #[expect(dead_code)]
        struct WorkflowState {
            id: String,
            name: String,
            #[serde(rename = "type")]
            state_type: String,
        }

        const TEAM_STATES_QUERY: &str = r#"
            query TeamStates($teamId: String!) {
                team(id: $teamId) {
                    states {
                        nodes {
                            id
                            name
                            type
                        }
                    }
                }
            }
        "#;

        let states_response: TeamStatesResponse = self
            .graphql(TEAM_STATES_QUERY, serde_json::json!({ "teamId": team_id }))
            .await?;

        let done_state = states_response
            .team
            .and_then(|t| {
                t.states
                    .nodes
                    .into_iter()
                    .find(|s| s.state_type == "completed")
            })
            .ok_or_else(|| Error::source("linear", "Could not find completed state for team"))?;

        // Update the issue state to Done
        #[derive(Debug, Deserialize)]
        struct IssueUpdateResponse {
            #[serde(rename = "issueUpdate")]
            issue_update: Option<IssueUpdatePayload>,
        }
        #[derive(Debug, Deserialize)]
        struct IssueUpdatePayload {
            success: bool,
        }

        const ISSUE_UPDATE_MUTATION: &str = r#"
            mutation UpdateIssue($id: String!, $stateId: String!) {
                issueUpdate(id: $id, input: { stateId: $stateId }) {
                    success
                }
            }
        "#;

        let update_response: IssueUpdateResponse = self
            .graphql(
                ISSUE_UPDATE_MUTATION,
                serde_json::json!({
                    "id": issue_id,
                    "stateId": done_state.id
                }),
            )
            .await?;

        if !update_response
            .issue_update
            .map(|u| u.success)
            .unwrap_or(false)
        {
            return Err(Error::source("linear", "Failed to update issue state"));
        }

        Ok(())
    }

    async fn add_comment(&self, issue_id: &str, comment: &str) -> Result<()> {
        #[derive(Debug, Deserialize)]
        struct CommentCreateResponse {
            #[serde(rename = "commentCreate")]
            comment_create: Option<CommentCreatePayload>,
        }
        #[derive(Debug, Deserialize)]
        struct CommentCreatePayload {
            success: bool,
        }

        const COMMENT_CREATE_MUTATION: &str = r#"
            mutation CreateComment($issueId: String!, $body: String!) {
                commentCreate(input: { issueId: $issueId, body: $body }) {
                    success
                }
            }
        "#;

        let response: CommentCreateResponse = self
            .graphql(
                COMMENT_CREATE_MUTATION,
                serde_json::json!({
                    "issueId": issue_id,
                    "body": comment
                }),
            )
            .await?;

        if !response.comment_create.map(|c| c.success).unwrap_or(false) {
            return Err(Error::source("linear", "Failed to create comment"));
        }

        Ok(())
    }

    async fn get_issue_status(&self, issue_id: &str) -> Result<String> {
        let issue = self.get_issue(issue_id).await?;
        let state_type: Option<String> = issue.get_metadata("state_type");
        Ok(state_type.unwrap_or_else(|| "unknown".to_string()))
    }

    fn is_terminal_status(&self, status: &str) -> bool {
        Self::is_issue_terminal(status)
    }

    async fn create_issue(
        &self,
        title: &str,
        description: &str,
        labels: &[String],
    ) -> Result<Issue> {
        let team_id = self
            .config
            .team_id
            .as_ref()
            .ok_or_else(|| Error::source("linear", "team_id required for create_issue"))?;

        // Resolve label IDs
        let mut label_ids = Vec::new();
        for label_name in labels {
            let label_id = self.find_or_create_label(label_name).await?;
            label_ids.push(label_id);
        }

        #[derive(Debug, Deserialize)]
        struct IssueCreateResponse {
            #[serde(rename = "issueCreate")]
            issue_create: Option<IssueCreatePayload>,
        }
        #[derive(Debug, Deserialize)]
        struct IssueCreatePayload {
            success: bool,
            issue: Option<CreatedIssue>,
        }
        #[derive(Debug, Deserialize)]
        struct CreatedIssue {
            id: String,
            identifier: String,
            url: String,
        }

        const ISSUE_CREATE_MUTATION: &str = r#"
            mutation CreateIssue($teamId: String!, $title: String!, $description: String, $labelIds: [String!]) {
                issueCreate(input: { teamId: $teamId, title: $title, description: $description, labelIds: $labelIds }) {
                    success
                    issue {
                        id
                        identifier
                        url
                    }
                }
            }
        "#;

        let response: IssueCreateResponse = self
            .graphql(
                ISSUE_CREATE_MUTATION,
                serde_json::json!({
                    "teamId": team_id,
                    "title": title,
                    "description": description,
                    "labelIds": label_ids,
                }),
            )
            .await?;

        let payload = response
            .issue_create
            .ok_or_else(|| Error::source("linear", "No response from issueCreate"))?;

        if !payload.success {
            return Err(Error::source(
                "linear",
                "issueCreate returned success=false",
            ));
        }

        let created = payload
            .issue
            .ok_or_else(|| Error::source("linear", "issueCreate returned no issue"))?;

        let issue = Issue::new(
            &created.id,
            &created.identifier,
            title,
            &created.url,
            "linear",
        );

        Ok(issue)
    }

    async fn find_or_create_label(&self, name: &str) -> Result<String> {
        // Query existing labels
        #[derive(Debug, Deserialize)]
        struct LabelsResponse {
            #[serde(rename = "issueLabels")]
            issue_labels: LabelsQueryConnection,
        }
        #[derive(Debug, Deserialize)]
        struct LabelsQueryConnection {
            nodes: Vec<LabelNode>,
        }
        #[derive(Debug, Deserialize)]
        struct LabelNode {
            id: String,
            name: String,
        }

        const LABELS_QUERY: &str = r#"
            query Labels($filter: IssueLabelFilter) {
                issueLabels(filter: $filter) {
                    nodes {
                        id
                        name
                    }
                }
            }
        "#;

        let response: LabelsResponse = self
            .graphql(
                LABELS_QUERY,
                serde_json::json!({
                    "filter": { "name": { "containsIgnoreCase": name } }
                }),
            )
            .await?;

        // Case-insensitive match from results
        if let Some(label) = response
            .issue_labels
            .nodes
            .iter()
            .find(|l| l.name.eq_ignore_ascii_case(name))
        {
            return Ok(label.id.clone());
        }

        // Create label if not found
        #[derive(Debug, Deserialize)]
        struct LabelCreateResponse {
            #[serde(rename = "issueLabelCreate")]
            label_create: Option<LabelCreatePayload>,
        }
        #[derive(Debug, Deserialize)]
        struct LabelCreatePayload {
            success: bool,
            #[serde(rename = "issueLabel")]
            issue_label: Option<LabelNode>,
        }

        let team_id = self.config.team_id.as_deref().unwrap_or("");

        const LABEL_CREATE_MUTATION: &str = r#"
            mutation CreateLabel($teamId: String, $name: String!) {
                issueLabelCreate(input: { teamId: $teamId, name: $name }) {
                    success
                    issueLabel {
                        id
                        name
                    }
                }
            }
        "#;

        let create_response: LabelCreateResponse = self
            .graphql(
                LABEL_CREATE_MUTATION,
                serde_json::json!({
                    "teamId": team_id,
                    "name": name,
                }),
            )
            .await?;

        let payload = create_response
            .label_create
            .ok_or_else(|| Error::source("linear", "No response from issueLabelCreate"))?;

        if !payload.success {
            return Err(Error::source(
                "linear",
                "issueLabelCreate returned success=false",
            ));
        }

        payload
            .issue_label
            .map(|l| l.id)
            .ok_or_else(|| Error::source("linear", "issueLabelCreate returned no label"))
    }

    async fn list_open_issues(&self, title_filter: &str) -> Result<Vec<Issue>> {
        let mut filter = serde_json::Map::new();

        if let Some(ref team_id) = self.config.team_id {
            if !team_id.is_empty() {
                filter.insert(
                    "team".to_string(),
                    serde_json::json!({ "id": { "eq": team_id } }),
                );
            }
        }

        // Filter to active states only (not completed/cancelled)
        filter.insert(
            "state".to_string(),
            serde_json::json!({ "type": { "nin": ["completed", "canceled"] } }),
        );

        if !title_filter.is_empty() {
            filter.insert(
                "title".to_string(),
                serde_json::json!({ "containsIgnoreCase": title_filter }),
            );
        }

        let variables = serde_json::json!({
            "filter": filter,
            "first": 50
        });

        let response: IssuesResponse = self.graphql(ISSUES_QUERY, variables).await?;

        Ok(response
            .issues
            .nodes
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
    pub struct MockLinearClient {
        responses: Mutex<HashMap<String, HttpResponse>>,
        requests: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl MockLinearClient {
        pub fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                requests: Mutex::new(Vec::new()),
            }
        }

        pub fn mock_response(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
            let mut responses = self.responses.lock().unwrap();
            responses.insert(
                url.into(),
                HttpResponse {
                    status,
                    body: body.into(),
                },
            );
        }

        pub fn get_requests(&self) -> Vec<(String, serde_json::Value)> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LinearHttpClient for MockLinearClient {
        async fn post(
            &self,
            url: &str,
            _api_key: &str,
            body: serde_json::Value,
        ) -> Result<HttpResponse> {
            self.requests.lock().unwrap().push((url.to_string(), body));
            let responses = self.responses.lock().unwrap();
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

    #[test]
    fn test_http_response_is_success() {
        let response = HttpResponse {
            status: 200,
            body: "{}".to_string(),
        };
        assert!(response.is_success());
        let response = HttpResponse {
            status: 404,
            body: "{}".to_string(),
        };
        assert!(!response.is_success());
    }

    #[tokio::test]
    async fn test_fetch_issues_success() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": [{
                            "id": "123",
                            "identifier": "PROJ-123",
                            "title": "Test Issue",
                            "description": "Description",
                            "url": "https://linear.app/123",
                            "priority": 2,
                            "createdAt": "2024-01-01T00:00:00Z",
                            "updatedAt": "2024-01-02T00:00:00Z",
                            "state": {"name": "Backlog", "type": "backlog"},
                            "labels": {"nodes": [{"name": "auto-implement"}]},
                            "team": {"id": "team1", "name": "Team"},
                            "project": null,
                            "assignee": null
                        }]
                    }
                }
            }"#,
        );

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].short_id, "PROJ-123");
        assert_eq!(issues[0].title, "Test Issue");
    }

    #[tokio::test]
    async fn test_fetch_issues_api_error() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            500,
            "Internal Server Error",
        );

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.fetch_issues().await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_fetch_issues_graphql_error() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "errors": [{"message": "Invalid query"}]
            }"#,
        );

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.fetch_issues().await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid query"));
    }

    #[tokio::test]
    async fn test_fetch_issues_no_data() {
        let mock = MockLinearClient::new();
        mock.mock_response("https://api.linear.app/graphql", 200, r#"{}"#);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.fetch_issues().await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No data"));
    }

    #[tokio::test]
    async fn test_get_issue_success() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issue": {
                        "id": "456",
                        "identifier": "PROJ-456",
                        "title": "Single Issue",
                        "description": "Desc",
                        "url": "https://linear.app/456",
                        "priority": 1,
                        "createdAt": "2024-01-01T00:00:00Z",
                        "updatedAt": "2024-01-02T00:00:00Z",
                        "state": {"name": "In Progress", "type": "started"},
                        "labels": {"nodes": []},
                        "team": null,
                        "project": null,
                        "assignee": {"name": "John Doe"}
                    }
                }
            }"#,
        );

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let issue = source.get_issue("456").await.unwrap();

        assert_eq!(issue.id, "456");
        assert_eq!(issue.short_id, "PROJ-456");
        assert_eq!(issue.status, IssueStatus::InProgress);
    }

    #[tokio::test]
    async fn test_get_issue_not_found() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issue": null
                }
            }"#,
        );

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.get_issue("nonexistent").await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_build_issue_context() {
        let mock = MockLinearClient::new();
        // No HTTP call needed for build_issue_context - it just formats metadata

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Test Issue",
            "https://linear.app/123",
            "linear",
        );
        issue.description = Some("This is the description".to_string());
        issue.set_metadata("team", "Engineering");
        issue.set_metadata("project", "Project Alpha");
        issue.set_metadata("assignee", "John");

        let context = source.build_issue_context(&issue).await.unwrap();

        assert!(context.contains("PROJ-123"));
        assert!(context.contains("Test Issue"));
        assert!(context.contains("This is the description"));
        assert!(context.contains("Engineering"));
        assert!(context.contains("Project Alpha"));
        assert!(context.contains("John"));
    }

    #[tokio::test]
    async fn test_fetch_issues_with_team_filter() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": []
                    }
                }
            }"#,
        );

        let mut config = test_config();
        config.team_id = Some("team-123".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();

        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_issues_with_project_filter() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": []
                    }
                }
            }"#,
        );

        let mut config = test_config();
        config.project_id = Some("proj-123".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();

        assert!(issues.is_empty());
    }

    fn test_config() -> LinearConfig {
        LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec!["auto-implement".to_string(), "claude".to_string()],
            trigger_states: vec!["backlog".to_string(), "todo".to_string()],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_map_priority() {
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(1),
            IssuePriority::Critical
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(2),
            IssuePriority::High
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(3),
            IssuePriority::Medium
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(4),
            IssuePriority::Low
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(0),
            IssuePriority::None
        );
    }

    #[test]
    fn test_map_status() {
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(Some("completed")),
            IssueStatus::Resolved
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(Some("canceled")),
            IssueStatus::Resolved
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(Some("started")),
            IssueStatus::InProgress
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(Some("backlog")),
            IssueStatus::Open
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(None),
            IssueStatus::Open
        );
    }

    #[test]
    fn test_matches_criteria_labels() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.set_metadata("state_name", "Backlog");
        issue.set_metadata("state_type", "backlog");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);

        // No matching labels
        issue.set_metadata("labels", vec!["other-label".to_string()]);
        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
    }

    #[test]
    fn test_matches_criteria_states() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["claude".to_string()]);
        issue.set_metadata("state_name", "Todo");
        issue.set_metadata("state_type", "todo");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);

        // Non-matching state
        issue.set_metadata("state_name", "In Progress");
        issue.set_metadata("state_type", "started");
        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
    }

    #[test]
    fn test_matches_criteria_priority_mapping() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["claude".to_string()]);
        issue.set_metadata("state_type", "backlog");
        issue.priority = IssuePriority::Critical;

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Urgent);

        issue.priority = IssuePriority::High;
        let result = source.matches_criteria(&issue);
        assert_eq!(result.priority, MatchPriority::High);

        issue.priority = IssuePriority::Low;
        let result = source.matches_criteria(&issue);
        assert_eq!(result.priority, MatchPriority::Low);
    }

    #[test]
    fn test_source_name() {
        let source = LinearSource::new(test_config());
        assert_eq!(source.name(), "linear");
        assert_eq!(source.display_name(), "Linear");
    }

    #[test]
    fn test_matches_criteria_empty_trigger_labels() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![], // Empty - matches all
            trigger_states: vec!["backlog".to_string()],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::new(config);

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["any-label".to_string()]);
        issue.set_metadata("state_type", "backlog");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_empty_trigger_states() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec!["auto-implement".to_string()],
            trigger_states: vec![], // Empty - matches all
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::new(config);

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.set_metadata("state_type", "any-state");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_case_insensitive_labels() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["AUTO-IMPLEMENT".to_string()]); // Uppercase
        issue.set_metadata("state_type", "backlog");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_case_insensitive_states() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["claude".to_string()]);
        issue.set_metadata("state_name", "BACKLOG"); // Uppercase
        issue.set_metadata("state_type", "BACKLOG");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_priority_medium() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["claude".to_string()]);
        issue.set_metadata("state_type", "backlog");
        issue.priority = IssuePriority::Medium;

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Normal);
    }

    #[test]
    fn test_matches_criteria_priority_none() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["claude".to_string()]);
        issue.set_metadata("state_type", "backlog");
        issue.priority = IssuePriority::None;

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Normal);
    }

    #[test]
    fn test_matches_criteria_no_metadata() {
        let source = LinearSource::new(test_config());

        // Issue with no metadata set
        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );

        let result = source.matches_criteria(&issue);
        // Should not match because no labels or state
        assert!(!result.matches);
    }

    #[test]
    fn test_matches_criteria_partial_state_match() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["claude".to_string()]);
        issue.set_metadata("state_name", "In Backlog"); // Contains "backlog"
        issue.set_metadata("state_type", "unstarted");

        let result = source.matches_criteria(&issue);
        // Should match because state_name contains "backlog"
        assert!(result.matches);
    }

    #[test]
    fn test_map_priority_all_values() {
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(0),
            IssuePriority::None
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(1),
            IssuePriority::Critical
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(2),
            IssuePriority::High
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(3),
            IssuePriority::Medium
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(4),
            IssuePriority::Low
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(5),
            IssuePriority::None
        ); // Out of range
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(-1),
            IssuePriority::None
        ); // Negative
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(100),
            IssuePriority::None
        );
    }

    #[test]
    fn test_map_status_all_values() {
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(Some("completed")),
            IssueStatus::Resolved
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(Some("canceled")),
            IssueStatus::Resolved
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(Some("started")),
            IssueStatus::InProgress
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(Some("backlog")),
            IssueStatus::Open
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(Some("triage")),
            IssueStatus::Open
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(Some("unstarted")),
            IssueStatus::Open
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(Some("")),
            IssueStatus::Open
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_status(None),
            IssueStatus::Open
        );
    }

    #[test]
    fn test_config_with_filters() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec!["urgent".to_string()],
            trigger_states: vec!["todo".to_string()],
            team_id: Some("team_123".to_string()),
            project_id: Some("project_456".to_string()),
            webhook_secret: Some("secret".into()),
            ..Default::default()
        };
        let source = LinearSource::new(config);

        // Verify source was created (API calls would require mocking)
        assert_eq!(source.name(), "linear");
    }

    #[test]
    fn test_matches_criteria_reason_message() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["claude".to_string()]);
        issue.set_metadata("state_name", "In Progress");
        issue.set_metadata("state_type", "started"); // Not in trigger_states

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(!result.reason.is_empty());
        assert!(result.reason.contains("In Progress"));
    }

    #[test]
    fn test_matches_criteria_no_matching_labels_message() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["unrelated".to_string()]);
        issue.set_metadata("state_type", "backlog");

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(!result.reason.is_empty());
        assert!(result.reason.contains("label"));
    }

    fn create_linear_issue(
        id: &str,
        identifier: &str,
        title: &str,
        priority: i32,
        state_type: &str,
        state_name: &str,
        labels: Vec<&str>,
    ) -> LinearIssue {
        LinearIssue {
            id: id.to_string(),
            identifier: identifier.to_string(),
            title: title.to_string(),
            description: Some("Test description".to_string()),
            url: format!("https://linear.app/team/issue/{}", identifier),
            priority,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
            state: Some(LinearState {
                name: state_name.to_string(),
                state_type: state_type.to_string(),
            }),
            labels: LabelsConnection {
                nodes: labels
                    .iter()
                    .map(|l| LinearLabel {
                        name: l.to_string(),
                    })
                    .collect(),
            },
            team: Some(LinearTeam {
                id: "team_123".to_string(),
                name: "Engineering".to_string(),
            }),
            project: Some(LinearProject {
                id: "proj_456".to_string(),
                name: "Backend".to_string(),
            }),
            assignee: Some(LinearUser {
                name: "John Doe".to_string(),
            }),
        }
    }

    #[test]
    fn test_map_issue_full() {
        let source = LinearSource::new(test_config());
        let linear_issue = create_linear_issue(
            "issue_123",
            "PROJ-123",
            "Fix authentication bug",
            1, // Critical
            "started",
            "In Progress",
            vec!["bug", "auth"],
        );

        let issue = source.map_issue(linear_issue);

        assert_eq!(issue.id, "issue_123");
        assert_eq!(issue.short_id, "PROJ-123");
        assert_eq!(issue.title, "Fix authentication bug");
        assert_eq!(issue.source, "linear");
        assert_eq!(issue.priority, IssuePriority::Critical);
        assert_eq!(issue.status, IssueStatus::InProgress);
        assert_eq!(issue.description, Some("Test description".to_string()));
        assert!(issue.created_at.is_some());
        assert!(issue.updated_at.is_some());

        // Check metadata
        let team: Option<String> = issue.get_metadata("team");
        assert_eq!(team, Some("Engineering".to_string()));

        let project: Option<String> = issue.get_metadata("project");
        assert_eq!(project, Some("Backend".to_string()));

        let assignee: Option<String> = issue.get_metadata("assignee");
        assert_eq!(assignee, Some("John Doe".to_string()));

        let labels: Vec<String> = issue.get_metadata("labels").unwrap_or_default();
        assert_eq!(labels.len(), 2);
        assert!(labels.contains(&"bug".to_string()));
    }

    #[test]
    fn test_map_issue_minimal() {
        let source = LinearSource::new(test_config());
        let linear_issue = LinearIssue {
            id: "123".to_string(),
            identifier: "TEST-1".to_string(),
            title: "Simple issue".to_string(),
            description: None,
            url: "https://linear.app/test/TEST-1".to_string(),
            priority: 0,
            created_at: "invalid-date".to_string(),
            updated_at: "invalid-date".to_string(),
            state: None,
            labels: LabelsConnection { nodes: vec![] },
            team: None,
            project: None,
            assignee: None,
        };

        let issue = source.map_issue(linear_issue);

        assert_eq!(issue.id, "123");
        assert_eq!(issue.short_id, "TEST-1");
        assert_eq!(issue.priority, IssuePriority::None);
        assert_eq!(issue.status, IssueStatus::Open);
        assert!(issue.description.is_none());
        assert!(issue.created_at.is_none()); // Invalid date
        assert!(issue.updated_at.is_none());
    }

    #[test]
    fn test_map_issue_completed_state() {
        let source = LinearSource::new(test_config());
        let linear_issue = create_linear_issue(
            "123",
            "TEST-1",
            "Done issue",
            3,
            "completed",
            "Done",
            vec![],
        );

        let issue = source.map_issue(linear_issue);
        assert_eq!(issue.status, IssueStatus::Resolved);
    }

    #[test]
    fn test_map_issue_canceled_state() {
        let source = LinearSource::new(test_config());
        let mut linear_issue = create_linear_issue(
            "123",
            "TEST-1",
            "Canceled issue",
            4,
            "canceled",
            "Canceled",
            vec![],
        );
        linear_issue.state = Some(LinearState {
            name: "Canceled".to_string(),
            state_type: "canceled".to_string(),
        });

        let issue = source.map_issue(linear_issue);
        assert_eq!(issue.status, IssueStatus::Resolved);
    }

    #[test]
    fn test_map_issue_all_priorities() {
        let source = LinearSource::new(test_config());

        for (priority_num, expected) in [
            (0, IssuePriority::None),
            (1, IssuePriority::Critical),
            (2, IssuePriority::High),
            (3, IssuePriority::Medium),
            (4, IssuePriority::Low),
            (5, IssuePriority::None),
        ] {
            let linear_issue = create_linear_issue(
                "123",
                "TEST-1",
                "Test",
                priority_num,
                "backlog",
                "Backlog",
                vec![],
            );
            let issue = source.map_issue(linear_issue);
            assert_eq!(
                issue.priority, expected,
                "Priority {} should map to {:?}",
                priority_num, expected
            );
        }
    }

    #[test]
    fn test_graphql_request_serialization() {
        let request = GraphQLRequest {
            query: "query { test }",
            variables: serde_json::json!({"id": "123"}),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("query"));
        assert!(json.contains("variables"));
        assert!(json.contains("123"));
    }

    #[test]
    fn test_graphql_error_debug() {
        let error = GraphQLError {
            message: "Test error".to_string(),
        };
        let debug = format!("{:?}", error);
        assert!(debug.contains("Test error"));
    }

    #[test]
    fn test_linear_state_deserialization() {
        let json = r#"{"name": "In Progress", "type": "started"}"#;
        let state: LinearState = serde_json::from_str(json).unwrap();
        assert_eq!(state.name, "In Progress");
        assert_eq!(state.state_type, "started");
    }

    #[test]
    fn test_linear_label_deserialization() {
        let json = r#"{"name": "bug"}"#;
        let label: LinearLabel = serde_json::from_str(json).unwrap();
        assert_eq!(label.name, "bug");
    }

    #[test]
    fn test_linear_team_deserialization() {
        let json = r#"{"id": "team_123", "name": "Engineering"}"#;
        let team: LinearTeam = serde_json::from_str(json).unwrap();
        assert_eq!(team.id, "team_123");
        assert_eq!(team.name, "Engineering");
    }

    #[test]
    fn test_linear_project_deserialization() {
        let json = r#"{"id": "proj_456", "name": "Backend"}"#;
        let project: LinearProject = serde_json::from_str(json).unwrap();
        assert_eq!(project.id, "proj_456");
        assert_eq!(project.name, "Backend");
    }

    #[test]
    fn test_linear_user_deserialization() {
        let json = r#"{"name": "John Doe"}"#;
        let user: LinearUser = serde_json::from_str(json).unwrap();
        assert_eq!(user.name, "John Doe");
    }

    #[test]
    fn test_linear_issue_full_deserialization() {
        let json = r#"{
            "id": "issue_123",
            "identifier": "PROJ-123",
            "title": "Test issue",
            "description": "Description here",
            "url": "https://linear.app/test",
            "priority": 2,
            "createdAt": "2024-01-01T00:00:00Z",
            "updatedAt": "2024-01-02T00:00:00Z",
            "state": {"name": "Todo", "type": "unstarted"},
            "labels": {"nodes": [{"name": "bug"}, {"name": "p1"}]},
            "team": {"id": "t1", "name": "Team"},
            "project": {"id": "p1", "name": "Project"},
            "assignee": {"name": "Jane"}
        }"#;
        let issue: LinearIssue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.id, "issue_123");
        assert_eq!(issue.identifier, "PROJ-123");
        assert_eq!(issue.priority, 2);
        assert_eq!(issue.labels.nodes.len(), 2);
        assert!(issue.team.is_some());
        assert!(issue.project.is_some());
        assert!(issue.assignee.is_some());
    }

    #[test]
    fn test_issues_response_deserialization() {
        let json = r#"{
            "issues": {
                "nodes": [
                    {
                        "id": "1",
                        "identifier": "T-1",
                        "title": "Issue 1",
                        "description": null,
                        "url": "https://linear.app/1",
                        "priority": 3,
                        "createdAt": "2024-01-01T00:00:00Z",
                        "updatedAt": "2024-01-01T00:00:00Z",
                        "state": null,
                        "labels": {"nodes": []},
                        "team": null,
                        "project": null,
                        "assignee": null
                    }
                ]
            }
        }"#;
        let response: IssuesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.issues.nodes.len(), 1);
    }

    #[test]
    fn test_issue_response_deserialization() {
        let json = r#"{
            "issue": {
                "id": "1",
                "identifier": "T-1",
                "title": "Issue 1",
                "description": null,
                "url": "https://linear.app/1",
                "priority": 3,
                "createdAt": "2024-01-01T00:00:00Z",
                "updatedAt": "2024-01-01T00:00:00Z",
                "state": null,
                "labels": {"nodes": []},
                "team": null,
                "project": null,
                "assignee": null
            }
        }"#;
        let response: IssueResponse = serde_json::from_str(json).unwrap();
        assert!(response.issue.is_some());
    }

    #[test]
    fn test_issue_response_null_deserialization() {
        let json = r#"{"issue": null}"#;
        let response: IssueResponse = serde_json::from_str(json).unwrap();
        assert!(response.issue.is_none());
    }

    #[test]
    fn test_graphql_response_with_errors() {
        let json = r#"{
            "data": null,
            "errors": [{"message": "Error 1"}, {"message": "Error 2"}]
        }"#;
        let response: GraphQLResponse<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert!(response.data.is_none());
        assert!(response.errors.is_some());
        assert_eq!(response.errors.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_graphql_response_with_data() {
        let json = r#"{"data": {"test": "value"}, "errors": null}"#;
        let response: GraphQLResponse<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert!(response.data.is_some());
        assert!(response.errors.is_none());
    }

    #[test]
    fn test_map_issue_with_valid_dates() {
        let source = LinearSource::new(test_config());
        let linear_issue = LinearIssue {
            id: "123".to_string(),
            identifier: "TEST-1".to_string(),
            title: "Test".to_string(),
            description: None,
            url: "https://linear.app/test".to_string(),
            priority: 0,
            created_at: "2024-06-15T10:30:00.000Z".to_string(),
            updated_at: "2024-06-16T14:45:00.000Z".to_string(),
            state: None,
            labels: LabelsConnection { nodes: vec![] },
            team: None,
            project: None,
            assignee: None,
        };

        let issue = source.map_issue(linear_issue);
        assert!(issue.created_at.is_some());
        assert!(issue.updated_at.is_some());
    }

    #[test]
    fn test_queries_constant() {
        // Verify queries are valid strings
        assert!(ISSUES_QUERY.contains("query Issues"));
        assert!(ISSUES_QUERY.contains("nodes"));
        assert!(ISSUE_QUERY.contains("query Issue"));
        assert!(ISSUE_QUERY.contains("$id"));
    }

    #[tokio::test]
    async fn test_build_issue_context_with_all_metadata() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-456",
            "Complex Bug",
            "https://linear.app/123",
            "linear",
        );
        issue.description = Some("A very complex bug description\nwith multiple lines".to_string());
        issue.priority = IssuePriority::Critical;
        issue.status = IssueStatus::InProgress;
        issue.set_metadata("team", "Platform");
        issue.set_metadata("project", "Infrastructure");
        issue.set_metadata("assignee", "Alice Smith");

        let context = source.build_issue_context(&issue).await.unwrap();

        assert!(context.contains("# Linear Issue: PROJ-456"));
        assert!(context.contains("**Title:** Complex Bug"));
        assert!(context.contains("https://linear.app/123"));
        assert!(context.contains("Priority")); // Check priority line exists
        assert!(context.contains("A very complex bug description"));
        assert!(context.contains("Platform"));
        assert!(context.contains("Infrastructure"));
        assert!(context.contains("Alice Smith"));
    }

    #[test]
    fn test_is_issue_terminal_various_inputs() {
        assert!(LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "completed"
        ));
        assert!(LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "Completed"
        ));
        assert!(LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "COMPLETED"
        ));
        assert!(LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "canceled"
        ));
        assert!(LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "Canceled"
        ));
        assert!(LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "cancelled"
        ));
        assert!(LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "Cancelled"
        ));
        assert!(!LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "started"
        ));
        assert!(!LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "backlog"
        ));
        assert!(!LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "triage"
        ));
        assert!(!LinearSource::<ReqwestLinearClient>::is_issue_terminal(""));
    }

    #[test]
    fn test_is_terminal_status_delegates_to_is_issue_terminal() {
        let source = LinearSource::new(test_config());
        assert!(source.is_terminal_status("completed"));
        assert!(source.is_terminal_status("canceled"));
        assert!(source.is_terminal_status("cancelled"));
        assert!(!source.is_terminal_status("started"));
        assert!(!source.is_terminal_status("backlog"));
    }

    #[tokio::test]
    async fn test_get_issue_status_returns_state_type() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issue": {
                        "id": "456",
                        "identifier": "PROJ-456",
                        "title": "Issue",
                        "description": null,
                        "url": "https://linear.app/456",
                        "priority": 2,
                        "createdAt": "2024-01-01T00:00:00Z",
                        "updatedAt": "2024-01-02T00:00:00Z",
                        "state": {"name": "Done", "type": "completed"},
                        "labels": {"nodes": []},
                        "team": null,
                        "project": null,
                        "assignee": null
                    }
                }
            }"#,
        );

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let status = source.get_issue_status("456").await.unwrap();
        assert_eq!(status, "completed");
    }

    #[tokio::test]
    async fn test_get_issue_status_unknown_when_no_state() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issue": {
                        "id": "789",
                        "identifier": "PROJ-789",
                        "title": "Issue",
                        "description": null,
                        "url": "https://linear.app/789",
                        "priority": 0,
                        "createdAt": "2024-01-01T00:00:00Z",
                        "updatedAt": "2024-01-02T00:00:00Z",
                        "state": null,
                        "labels": {"nodes": []},
                        "team": null,
                        "project": null,
                        "assignee": null
                    }
                }
            }"#,
        );

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let status = source.get_issue_status("789").await.unwrap();
        assert_eq!(status, "unknown");
    }

    #[tokio::test]
    async fn test_build_issue_context_minimal_no_metadata() {
        let source = LinearSource::new(test_config());

        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Simple Issue",
            "https://linear.app/123",
            "linear",
        );

        let context = source.build_issue_context(&issue).await.unwrap();

        assert!(context.contains("PROJ-123"));
        assert!(context.contains("Simple Issue"));
        assert!(context.contains("https://linear.app/123"));
        // Should not contain metadata sections
        assert!(!context.contains("**Team:**"));
        assert!(!context.contains("**Project:**"));
        assert!(!context.contains("**Assignee:**"));
        assert!(!context.contains("## Description"));
    }

    #[tokio::test]
    async fn test_fetch_issues_with_empty_team_id_string() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": []
                    }
                }
            }"#,
        );

        let mut config = test_config();
        config.team_id = Some("".to_string()); // Empty string should be treated as not set
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_issues_with_empty_project_id_string() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": []
                    }
                }
            }"#,
        );

        let mut config = test_config();
        config.project_id = Some("".to_string()); // Empty string should be treated as not set
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_issues_with_trigger_labels_in_filter() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": []
                    }
                }
            }"#,
        );

        let config = test_config(); // Has trigger_labels: auto-implement, claude
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();
        assert!(issues.is_empty());

        // Verify request was sent (the mock client records requests)
        let requests = source.http.get_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "https://api.linear.app/graphql");
    }

    #[test]
    fn test_http_response_json_parse_failure() {
        let response = HttpResponse {
            status: 200,
            body: "not valid json".to_string(),
        };
        let result: Result<serde_json::Value> = response.json();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("JSON parse error"));
    }

    #[test]
    fn test_http_response_boundary_status_codes() {
        assert!(!HttpResponse {
            status: 199,
            body: String::new()
        }
        .is_success());
        assert!(HttpResponse {
            status: 200,
            body: String::new()
        }
        .is_success());
        assert!(HttpResponse {
            status: 299,
            body: String::new()
        }
        .is_success());
        assert!(!HttpResponse {
            status: 300,
            body: String::new()
        }
        .is_success());
    }

    #[test]
    fn test_map_issue_state_metadata_stored() {
        let source = LinearSource::new(test_config());
        let linear_issue = create_linear_issue(
            "123",
            "PROJ-123",
            "Issue with state",
            2,
            "started",
            "In Progress",
            vec!["bug"],
        );

        let issue = source.map_issue(linear_issue);

        let state_name: Option<String> = issue.get_metadata("state_name");
        assert_eq!(state_name, Some("In Progress".to_string()));

        let state_type: Option<String> = issue.get_metadata("state_type");
        assert_eq!(state_type, Some("started".to_string()));
    }

    #[test]
    fn test_map_issue_no_state_no_state_metadata() {
        let source = LinearSource::new(test_config());
        let linear_issue = LinearIssue {
            id: "123".to_string(),
            identifier: "TEST-1".to_string(),
            title: "No state".to_string(),
            description: None,
            url: "https://linear.app/test".to_string(),
            priority: 0,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
            state: None,
            labels: LabelsConnection { nodes: vec![] },
            team: None,
            project: None,
            assignee: None,
        };

        let issue = source.map_issue(linear_issue);

        let state_name: Option<String> = issue.get_metadata("state_name");
        assert!(state_name.is_none());

        let state_type: Option<String> = issue.get_metadata("state_type");
        assert!(state_type.is_none());
    }

    #[test]
    fn test_map_issue_team_and_project_metadata() {
        let source = LinearSource::new(test_config());
        let linear_issue =
            create_linear_issue("123", "PROJ-123", "Issue", 2, "backlog", "Backlog", vec![]);

        let issue = source.map_issue(linear_issue);

        let team: Option<String> = issue.get_metadata("team");
        assert_eq!(team, Some("Engineering".to_string()));

        let team_id: Option<String> = issue.get_metadata("team_id");
        assert_eq!(team_id, Some("team_123".to_string()));

        let project: Option<String> = issue.get_metadata("project");
        assert_eq!(project, Some("Backend".to_string()));

        let project_id: Option<String> = issue.get_metadata("project_id");
        assert_eq!(project_id, Some("proj_456".to_string()));

        let assignee: Option<String> = issue.get_metadata("assignee");
        assert_eq!(assignee, Some("John Doe".to_string()));
    }

    #[test]
    fn test_matches_criteria_state_match_by_state_type() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["claude".to_string()]);
        issue.set_metadata("state_name", "Custom State");
        issue.set_metadata("state_type", "todo"); // Matches trigger_states

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_no_state_name_still_checks_type() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["claude".to_string()]);
        // Only set state_type, not state_name
        issue.set_metadata("state_type", "backlog");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_fetch_issues_with_both_team_and_project_filter() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": []
                    }
                }
            }"#,
        );

        let mut config = test_config();
        config.team_id = Some("team-1".to_string());
        config.project_id = Some("proj-1".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_issues_empty_trigger_labels_omits_labels_filter() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": []
                    }
                }
            }"#,
        );

        let mut config = test_config();
        config.trigger_labels = vec![]; // Empty
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_build_issue_context_with_description_only() {
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Issue with desc",
            "https://linear.app/123",
            "linear",
        );
        issue.description = Some("Only a description, no other metadata".to_string());

        let context = source.build_issue_context(&issue).await.unwrap();

        assert!(context.contains("## Description"));
        assert!(context.contains("Only a description, no other metadata"));
        assert!(!context.contains("**Team:**"));
    }

    #[test]
    fn test_graphql_response_no_data_no_errors() {
        let json = r#"{}"#;
        let response: GraphQLResponse<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert!(response.data.is_none());
        assert!(response.errors.is_none());
    }

    // --- New tests for coverage ---

    /// Sequential mock HTTP client that returns queued responses in order.
    pub struct SequentialMockLinearClient {
        responses: Mutex<Vec<HttpResponse>>,
        requests: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl SequentialMockLinearClient {
        pub fn new(responses: Vec<(u16, &str)>) -> Self {
            Self {
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .rev() // Reverse so we can pop from the end
                        .map(|(status, body)| HttpResponse {
                            status,
                            body: body.to_string(),
                        })
                        .collect(),
                ),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LinearHttpClient for SequentialMockLinearClient {
        async fn post(
            &self,
            url: &str,
            _api_key: &str,
            body: serde_json::Value,
        ) -> Result<HttpResponse> {
            self.requests.lock().unwrap().push((url.to_string(), body));
            let mut responses = self.responses.lock().unwrap();
            if let Some(response) = responses.pop() {
                Ok(response)
            } else {
                Ok(HttpResponse {
                    status: 500,
                    body: "No more mock responses".to_string(),
                })
            }
        }
    }

    #[tokio::test]
    async fn test_resolve_issue_success() {
        // Exercises the full resolve_issue path (lines 498-597):
        // 1. get_issue (to find team_id)
        // 2. query team states (to find completed state)
        // 3. update issue state
        let mock = SequentialMockLinearClient::new(vec![
            // Response 1: get_issue
            (
                200,
                r#"{
                    "data": {
                        "issue": {
                            "id": "resolve-1",
                            "identifier": "PROJ-R1",
                            "title": "To Resolve",
                            "description": null,
                            "url": "https://linear.app/resolve-1",
                            "priority": 2,
                            "createdAt": "2024-01-01T00:00:00Z",
                            "updatedAt": "2024-01-02T00:00:00Z",
                            "state": {"name": "In Progress", "type": "started"},
                            "labels": {"nodes": []},
                            "team": {"id": "team-abc", "name": "Engineering"},
                            "project": null,
                            "assignee": null
                        }
                    }
                }"#,
            ),
            // Response 2: team states query
            (
                200,
                r#"{
                    "data": {
                        "team": {
                            "states": {
                                "nodes": [
                                    {"id": "state-1", "name": "Backlog", "type": "backlog"},
                                    {"id": "state-2", "name": "In Progress", "type": "started"},
                                    {"id": "state-3", "name": "Done", "type": "completed"}
                                ]
                            }
                        }
                    }
                }"#,
            ),
            // Response 3: issue update mutation
            (
                200,
                r#"{
                    "data": {
                        "issueUpdate": {
                            "success": true
                        }
                    }
                }"#,
            ),
        ]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.resolve_issue("resolve-1").await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_resolve_issue_no_team_id() {
        // Issue has no team metadata, so resolve_issue should error (line 501-503).
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "issue": {
                        "id": "no-team",
                        "identifier": "PROJ-NT",
                        "title": "No Team",
                        "description": null,
                        "url": "https://linear.app/no-team",
                        "priority": 2,
                        "createdAt": "2024-01-01T00:00:00Z",
                        "updatedAt": "2024-01-02T00:00:00Z",
                        "state": {"name": "Backlog", "type": "backlog"},
                        "labels": {"nodes": []},
                        "team": null,
                        "project": null,
                        "assignee": null
                    }
                }
            }"#,
        )]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.resolve_issue("no-team").await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("team_id"));
    }

    #[tokio::test]
    async fn test_resolve_issue_no_completed_state() {
        // Team has no completed state (line 544-552).
        let mock = SequentialMockLinearClient::new(vec![
            // get_issue
            (
                200,
                r#"{
                    "data": {
                        "issue": {
                            "id": "no-done",
                            "identifier": "PROJ-ND",
                            "title": "No Done State",
                            "description": null,
                            "url": "https://linear.app/no-done",
                            "priority": 2,
                            "createdAt": "2024-01-01T00:00:00Z",
                            "updatedAt": "2024-01-02T00:00:00Z",
                            "state": {"name": "In Progress", "type": "started"},
                            "labels": {"nodes": []},
                            "team": {"id": "team-xyz", "name": "Ops"},
                            "project": null,
                            "assignee": null
                        }
                    }
                }"#,
            ),
            // team states - no completed state
            (
                200,
                r#"{
                    "data": {
                        "team": {
                            "states": {
                                "nodes": [
                                    {"id": "s1", "name": "Backlog", "type": "backlog"},
                                    {"id": "s2", "name": "In Progress", "type": "started"}
                                ]
                            }
                        }
                    }
                }"#,
            ),
        ]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.resolve_issue("no-done").await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("completed state"));
    }

    #[tokio::test]
    async fn test_resolve_issue_update_fails() {
        // Issue update returns success: false (line 583-588).
        let mock = SequentialMockLinearClient::new(vec![
            // get_issue
            (
                200,
                r#"{
                    "data": {
                        "issue": {
                            "id": "fail-update",
                            "identifier": "PROJ-FU",
                            "title": "Fail Update",
                            "description": null,
                            "url": "https://linear.app/fail-update",
                            "priority": 2,
                            "createdAt": "2024-01-01T00:00:00Z",
                            "updatedAt": "2024-01-02T00:00:00Z",
                            "state": {"name": "Todo", "type": "unstarted"},
                            "labels": {"nodes": []},
                            "team": {"id": "team-q", "name": "Team"},
                            "project": null,
                            "assignee": null
                        }
                    }
                }"#,
            ),
            // team states
            (
                200,
                r#"{
                    "data": {
                        "team": {
                            "states": {
                                "nodes": [
                                    {"id": "s1", "name": "Done", "type": "completed"}
                                ]
                            }
                        }
                    }
                }"#,
            ),
            // issue update - fails
            (
                200,
                r#"{
                    "data": {
                        "issueUpdate": {
                            "success": false
                        }
                    }
                }"#,
            ),
        ]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.resolve_issue("fail-update").await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to update issue state"));
    }

    #[tokio::test]
    async fn test_resolve_issue_update_null_payload() {
        // Issue update returns null payload (line 583-586 unwrap_or(false)).
        let mock = SequentialMockLinearClient::new(vec![
            (
                200,
                r#"{
                    "data": {
                        "issue": {
                            "id": "null-upd",
                            "identifier": "PROJ-NU",
                            "title": "Null Update",
                            "description": null,
                            "url": "https://linear.app/null-upd",
                            "priority": 2,
                            "createdAt": "2024-01-01T00:00:00Z",
                            "updatedAt": "2024-01-02T00:00:00Z",
                            "state": {"name": "Todo", "type": "unstarted"},
                            "labels": {"nodes": []},
                            "team": {"id": "t1", "name": "T"},
                            "project": null,
                            "assignee": null
                        }
                    }
                }"#,
            ),
            (
                200,
                r#"{
                    "data": {
                        "team": {
                            "states": {
                                "nodes": [{"id": "done-1", "name": "Done", "type": "completed"}]
                            }
                        }
                    }
                }"#,
            ),
            // issueUpdate is null
            (
                200,
                r#"{
                    "data": {
                        "issueUpdate": null
                    }
                }"#,
            ),
        ]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.resolve_issue("null-upd").await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to update issue state"));
    }

    #[tokio::test]
    async fn test_add_comment_success() {
        // Exercises the add_comment path (lines 600-634).
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "commentCreate": {
                        "success": true
                    }
                }
            }"#,
        )]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source
            .add_comment("issue-1", "This is a test comment")
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_comment_failure() {
        // Comment creation returns success: false (line 629-630).
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "commentCreate": {
                        "success": false
                    }
                }
            }"#,
        )]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.add_comment("issue-1", "This will fail").await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to create comment"));
    }

    #[tokio::test]
    async fn test_add_comment_null_payload() {
        // commentCreate is null (line 629 unwrap_or(false)).
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "commentCreate": null
                }
            }"#,
        )]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.add_comment("issue-1", "Null payload").await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to create comment"));
    }

    #[tokio::test]
    async fn test_add_comment_api_error() {
        // API returns non-200 status (line 277-281 in graphql method).
        let mock = SequentialMockLinearClient::new(vec![(500, "Internal Server Error")]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.add_comment("issue-1", "Error comment").await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_build_issue_context_no_description() {
        // Exercises the build_issue_context with no description (line 468 branch).
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "No Desc Issue",
            "https://linear.app/123",
            "linear",
        );
        issue.set_metadata("team", "Backend");

        let context = source.build_issue_context(&issue).await.unwrap();

        assert!(context.contains("PROJ-123"));
        assert!(context.contains("No Desc Issue"));
        assert!(!context.contains("## Description"));
        assert!(context.contains("**Team:** Backend"));
    }

    #[tokio::test]
    async fn test_build_issue_context_with_team_only() {
        // Exercises just the team metadata path (line 472-473).
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "t1",
            "PROJ-T1",
            "Team Only",
            "https://linear.app/t1",
            "linear",
        );
        issue.set_metadata("team", "Frontend");

        let context = source.build_issue_context(&issue).await.unwrap();

        assert!(context.contains("**Team:** Frontend"));
        assert!(!context.contains("**Project:**"));
        assert!(!context.contains("**Assignee:**"));
    }

    #[tokio::test]
    async fn test_build_issue_context_with_project_only() {
        // Exercises just the project metadata path (line 475-476).
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "p1",
            "PROJ-P1",
            "Project Only",
            "https://linear.app/p1",
            "linear",
        );
        issue.set_metadata("project", "Infra");

        let context = source.build_issue_context(&issue).await.unwrap();

        assert!(!context.contains("**Team:**"));
        assert!(context.contains("**Project:** Infra"));
        assert!(!context.contains("**Assignee:**"));
    }

    #[tokio::test]
    async fn test_build_issue_context_with_assignee_only() {
        // Exercises just the assignee metadata path (line 478-479).
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "a1",
            "PROJ-A1",
            "Assignee Only",
            "https://linear.app/a1",
            "linear",
        );
        issue.set_metadata("assignee", "Bob");

        let context = source.build_issue_context(&issue).await.unwrap();

        assert!(!context.contains("**Team:**"));
        assert!(!context.contains("**Project:**"));
        assert!(context.contains("**Assignee:** Bob"));
    }

    #[tokio::test]
    async fn test_build_issue_context_priority_and_status_display() {
        // Exercises the priority and status formatting (lines 465-466).
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "ps1",
            "PROJ-PS1",
            "Priority Status",
            "https://linear.app/ps1",
            "linear",
        );
        issue.priority = IssuePriority::High;
        issue.status = IssueStatus::InProgress;

        let context = source.build_issue_context(&issue).await.unwrap();

        assert!(context.contains("**Priority:**"));
        assert!(context.contains("**Status:**"));
    }

    #[tokio::test]
    async fn test_get_issue_api_error() {
        // API error when getting a single issue (line 490).
        let mock = SequentialMockLinearClient::new(vec![(500, "Bad Request")]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.get_issue("bad-id").await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_issue_null_issue() {
        // GraphQL returns null for issue (line 492-495).
        let mock = SequentialMockLinearClient::new(vec![(200, r#"{"data": {"issue": null}}"#)]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.get_issue("missing-id").await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_issue_status_started() {
        // Exercises get_issue_status with "started" state type (lines 637-640).
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "issue": {
                        "id": "status-1",
                        "identifier": "PROJ-ST1",
                        "title": "Started Issue",
                        "description": null,
                        "url": "https://linear.app/status-1",
                        "priority": 2,
                        "createdAt": "2024-01-01T00:00:00Z",
                        "updatedAt": "2024-01-02T00:00:00Z",
                        "state": {"name": "In Progress", "type": "started"},
                        "labels": {"nodes": []},
                        "team": null,
                        "project": null,
                        "assignee": null
                    }
                }
            }"#,
        )]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let status = source.get_issue_status("status-1").await.unwrap();
        assert_eq!(status, "started");
    }

    #[tokio::test]
    async fn test_resolve_issue_team_null_in_states_response() {
        // Team states response returns null team (line 544-546 .and_then path).
        let mock = SequentialMockLinearClient::new(vec![
            (
                200,
                r#"{
                    "data": {
                        "issue": {
                            "id": "tn1",
                            "identifier": "PROJ-TN1",
                            "title": "Team Null States",
                            "description": null,
                            "url": "https://linear.app/tn1",
                            "priority": 2,
                            "createdAt": "2024-01-01T00:00:00Z",
                            "updatedAt": "2024-01-02T00:00:00Z",
                            "state": {"name": "Todo", "type": "unstarted"},
                            "labels": {"nodes": []},
                            "team": {"id": "team-null", "name": "Team"},
                            "project": null,
                            "assignee": null
                        }
                    }
                }"#,
            ),
            // team is null in states response
            (200, r#"{"data": {"team": null}}"#),
        ]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.resolve_issue("tn1").await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("completed state"));
    }

    #[tokio::test]
    async fn test_fetch_issues_maps_multiple_issues() {
        // Exercises the map over issue nodes (lines 406-411).
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": [
                            {
                                "id": "1",
                                "identifier": "T-1",
                                "title": "First",
                                "description": "Desc 1",
                                "url": "https://linear.app/1",
                                "priority": 1,
                                "createdAt": "2024-01-01T00:00:00Z",
                                "updatedAt": "2024-01-02T00:00:00Z",
                                "state": {"name": "Backlog", "type": "backlog"},
                                "labels": {"nodes": [{"name": "auto-implement"}]},
                                "team": {"id": "t1", "name": "Eng"},
                                "project": null,
                                "assignee": null
                            },
                            {
                                "id": "2",
                                "identifier": "T-2",
                                "title": "Second",
                                "description": null,
                                "url": "https://linear.app/2",
                                "priority": 3,
                                "createdAt": "2024-01-01T00:00:00Z",
                                "updatedAt": "2024-01-03T00:00:00Z",
                                "state": {"name": "Todo", "type": "unstarted"},
                                "labels": {"nodes": []},
                                "team": null,
                                "project": {"id": "p1", "name": "Proj"},
                                "assignee": {"name": "Alice"}
                            }
                        ]
                    }
                }
            }"#,
        )]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].id, "1");
        assert_eq!(issues[0].short_id, "T-1");
        assert_eq!(issues[0].priority, IssuePriority::Critical);
        assert_eq!(issues[1].id, "2");
        assert_eq!(issues[1].short_id, "T-2");
        assert_eq!(issues[1].priority, IssuePriority::Medium);
    }

    #[test]
    fn test_matches_criteria_no_state_metadata_with_trigger_states() {
        // Exercises the branch where state_name/state_type are both None
        // but trigger_states is non-empty (line 421-434).
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "No State",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", vec!["claude".to_string()]);
        // No state_name or state_type set

        let result = source.matches_criteria(&issue);
        // Should not match because trigger_states is non-empty and state is "unknown"
        assert!(!result.matches);
        assert!(result.reason.contains("not in trigger states"));
    }

    #[test]
    fn test_matches_criteria_empty_labels_on_issue_with_trigger_labels() {
        // Issue has empty labels list but trigger_labels is non-empty (line 438-447).
        let source = LinearSource::new(test_config());

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "No Labels",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("labels", Vec::<String>::new());
        issue.set_metadata("state_type", "backlog");

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("label"));
    }

    #[test]
    fn test_format_linear_context_basic() {
        let issue = Issue::new(
            "100",
            "PROJ-100",
            "Basic Issue",
            "https://linear.app/100",
            "linear",
        );

        let context = format_linear_context(&issue);

        assert!(context.contains("# Linear Issue: PROJ-100"));
        assert!(context.contains("**Title:** Basic Issue"));
        assert!(context.contains("**URL:** https://linear.app/100"));
        assert!(context.contains("**Priority:**"));
        assert!(context.contains("**Status:**"));
        // No metadata set, so these should not appear
        assert!(!context.contains("## Description"));
        assert!(!context.contains("**Team:**"));
        assert!(!context.contains("**Project:**"));
        assert!(!context.contains("**Assignee:**"));
    }

    #[test]
    fn test_format_linear_context_with_description() {
        let mut issue = Issue::new(
            "200",
            "PROJ-200",
            "Described Issue",
            "https://linear.app/200",
            "linear",
        );
        issue.description =
            Some("This is a detailed description\nwith multiple lines.".to_string());

        let context = format_linear_context(&issue);

        assert!(context.contains("## Description"));
        assert!(context.contains("This is a detailed description"));
        assert!(context.contains("with multiple lines."));
    }

    #[test]
    fn test_format_linear_context_with_all_metadata() {
        let mut issue = Issue::new(
            "300",
            "PROJ-300",
            "Full Issue",
            "https://linear.app/300",
            "linear",
        );
        issue.description = Some("Full description".to_string());
        issue.set_metadata("team", "Platform");
        issue.set_metadata("project", "Infrastructure");
        issue.set_metadata("assignee", "Jane Doe");

        let context = format_linear_context(&issue);

        assert!(context.contains("**Team:** Platform"));
        assert!(context.contains("**Project:** Infrastructure"));
        assert!(context.contains("**Assignee:** Jane Doe"));
        assert!(context.contains("## Description"));
        assert!(context.contains("Full description"));
    }

    #[test]
    fn test_format_linear_context_team_only() {
        let mut issue = Issue::new(
            "400",
            "PROJ-400",
            "Team Only",
            "https://linear.app/400",
            "linear",
        );
        issue.set_metadata("team", "Backend");

        let context = format_linear_context(&issue);

        assert!(context.contains("**Team:** Backend"));
        assert!(!context.contains("**Project:**"));
        assert!(!context.contains("**Assignee:**"));
    }

    #[test]
    fn test_format_linear_context_project_only() {
        let mut issue = Issue::new(
            "500",
            "PROJ-500",
            "Project Only",
            "https://linear.app/500",
            "linear",
        );
        issue.set_metadata("project", "API v2");

        let context = format_linear_context(&issue);

        assert!(!context.contains("**Team:**"));
        assert!(context.contains("**Project:** API v2"));
        assert!(!context.contains("**Assignee:**"));
    }

    #[test]
    fn test_format_linear_context_assignee_only() {
        let mut issue = Issue::new(
            "600",
            "PROJ-600",
            "Assigned Issue",
            "https://linear.app/600",
            "linear",
        );
        issue.set_metadata("assignee", "Alice");

        let context = format_linear_context(&issue);

        assert!(!context.contains("**Team:**"));
        assert!(!context.contains("**Project:**"));
        assert!(context.contains("**Assignee:** Alice"));
    }

    #[test]
    fn test_format_linear_context_no_description() {
        let issue = Issue::new(
            "700",
            "PROJ-700",
            "No Desc",
            "https://linear.app/700",
            "linear",
        );

        let context = format_linear_context(&issue);

        assert!(!context.contains("## Description"));
    }

    #[test]
    fn test_matches_criteria_trigger_assignee_no_labels() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![],
            trigger_assignee: Some("Jane Smith".to_string()),
            trigger_states: vec!["backlog".to_string()],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::new(config);

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("assignee", "Jane Smith");
        issue.set_metadata("state_type", "backlog");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_trigger_assignee_wrong_assignee() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![],
            trigger_assignee: Some("Jane Smith".to_string()),
            trigger_states: vec!["backlog".to_string()],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::new(config);

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("assignee", "John Doe");
        issue.set_metadata("state_type", "backlog");

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("Assignee"));
    }

    #[test]
    fn test_matches_criteria_trigger_assignee_case_insensitive() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![],
            trigger_assignee: Some("jane smith".to_string()),
            trigger_states: vec!["backlog".to_string()],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::new(config);

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("assignee", "Jane Smith");
        issue.set_metadata("state_type", "backlog");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_trigger_assignee_and_labels_both_required() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec!["auto-implement".to_string()],
            trigger_assignee: Some("Jane Smith".to_string()),
            trigger_states: vec!["backlog".to_string()],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::new(config);

        // Both assignee and label match
        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("assignee", "Jane Smith");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        issue.set_metadata("state_type", "backlog");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);

        // Right assignee, wrong label
        issue.set_metadata("labels", vec!["other".to_string()]);
        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("label"));

        // Wrong assignee, right label
        issue.set_metadata("assignee", "John Doe");
        issue.set_metadata("labels", vec!["auto-implement".to_string()]);
        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("Assignee"));
    }

    #[test]
    fn test_matches_criteria_trigger_assignee_unassigned_issue() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![],
            trigger_assignee: Some("Jane Smith".to_string()),
            trigger_states: vec!["backlog".to_string()],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::new(config);

        let mut issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        // No assignee metadata set
        issue.set_metadata("state_type", "backlog");

        let result = source.matches_criteria(&issue);
        assert!(!result.matches);
        assert!(result.reason.contains("unassigned"));
    }

    #[tokio::test]
    async fn test_fetch_issues_with_assignee_filter() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": []
                    }
                }
            }"#,
        );

        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![],
            trigger_assignee: Some("Jane Smith".to_string()),
            trigger_states: vec!["backlog".to_string()],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::with_http_client(config, mock);
        let _issues = source.fetch_issues().await.unwrap();

        // Verify the GraphQL variables include assignee filter
        let requests = source.http.get_requests();
        assert_eq!(requests.len(), 1);

        let body = &requests[0].1;
        let filter = &body["variables"]["filter"];
        let assignee_filter = &filter["assignee"];
        assert_eq!(
            assignee_filter["displayName"]["eqCaseInsensitive"],
            "Jane Smith"
        );
        // Should NOT have a labels filter since trigger_labels is empty
        assert!(filter.get("labels").is_none());
    }

    #[tokio::test]
    async fn test_fetch_issues_with_assignee_and_labels_filter() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": []
                    }
                }
            }"#,
        );

        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec!["auto-implement".to_string()],
            trigger_assignee: Some("Jane Smith".to_string()),
            trigger_states: vec!["backlog".to_string()],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::with_http_client(config, mock);
        let _issues = source.fetch_issues().await.unwrap();

        let requests = source.http.get_requests();
        let body = &requests[0].1;
        let filter = &body["variables"]["filter"];
        // Both assignee and labels filters should be present
        assert!(filter.get("assignee").is_some());
        assert!(filter.get("labels").is_some());
    }

    #[test]
    fn test_source_name_and_display_name() {
        let config = test_config();
        let source = LinearSource::with_http_client(config, MockLinearClient::new());
        assert_eq!(source.name(), "linear");
        assert_eq!(source.display_name(), "Linear");
    }

    #[test]
    fn test_is_terminal_status_completed() {
        let config = test_config();
        let source = LinearSource::with_http_client(config, MockLinearClient::new());
        assert!(source.is_terminal_status("completed"));
    }

    #[test]
    fn test_is_terminal_status_canceled() {
        let config = test_config();
        let source = LinearSource::with_http_client(config, MockLinearClient::new());
        assert!(source.is_terminal_status("canceled"));
    }

    #[test]
    fn test_is_terminal_status_cancelled_british() {
        let config = test_config();
        let source = LinearSource::with_http_client(config, MockLinearClient::new());
        assert!(source.is_terminal_status("cancelled"));
    }

    #[test]
    fn test_is_terminal_status_started_is_not_terminal() {
        let config = test_config();
        let source = LinearSource::with_http_client(config, MockLinearClient::new());
        assert!(!source.is_terminal_status("started"));
        assert!(!source.is_terminal_status("backlog"));
        assert!(!source.is_terminal_status("triage"));
    }

    #[test]
    fn test_is_terminal_status_case_insensitive() {
        assert!(LinearSource::<MockLinearClient>::is_issue_terminal(
            "Completed"
        ));
        assert!(LinearSource::<MockLinearClient>::is_issue_terminal(
            "CANCELED"
        ));
        assert!(LinearSource::<MockLinearClient>::is_issue_terminal(
            "Cancelled"
        ));
    }

    #[tokio::test]
    async fn test_get_issue_status_returns_started_type() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issue": {
                        "id": "abc-123",
                        "identifier": "LIN-1",
                        "title": "Test",
                        "description": null,
                        "url": "https://linear.app/team/LIN-1",
                        "priority": 2,
                        "createdAt": "2025-01-01T00:00:00Z",
                        "updatedAt": "2025-01-02T00:00:00Z",
                        "state": {
                            "name": "In Progress",
                            "type": "started"
                        },
                        "labels": { "nodes": [] },
                        "team": { "id": "team-1", "name": "Team" },
                        "project": null,
                        "assignee": null
                    }
                }
            }"#,
        );
        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let status = source.get_issue_status("abc-123").await.unwrap();
        assert_eq!(status, "started");
    }

    #[tokio::test]
    async fn test_get_issue_status_no_state_returns_unknown() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{
                "data": {
                    "issue": {
                        "id": "abc-456",
                        "identifier": "LIN-2",
                        "title": "Test No State",
                        "description": null,
                        "url": "https://linear.app/team/LIN-2",
                        "priority": 0,
                        "createdAt": "2025-01-01T00:00:00Z",
                        "updatedAt": "2025-01-02T00:00:00Z",
                        "state": null,
                        "labels": { "nodes": [] },
                        "team": null,
                        "project": null,
                        "assignee": null
                    }
                }
            }"#,
        );
        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let status = source.get_issue_status("abc-456").await.unwrap();
        assert_eq!(status, "unknown");
    }

    #[tokio::test]
    async fn test_fetch_issues_empty_team_id_skipped() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{"data":{"issues":{"nodes":[]}}}"#,
        );
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![],
            trigger_assignee: None,
            trigger_states: vec![],
            team_id: Some("".to_string()), // empty team_id should be skipped
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();
        assert!(issues.is_empty());

        // Verify the filter does NOT include team
        let requests = source.http.get_requests();
        let body = &requests[0].1;
        let filter = &body["variables"]["filter"];
        assert!(
            filter.get("team").is_none(),
            "Empty team_id should not produce a team filter"
        );
    }

    #[tokio::test]
    async fn test_fetch_issues_empty_project_id_skipped() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{"data":{"issues":{"nodes":[]}}}"#,
        );
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![],
            trigger_assignee: None,
            trigger_states: vec![],
            team_id: None,
            project_id: Some("".to_string()), // empty project_id should be skipped
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.fetch_issues().await.unwrap();
        assert!(issues.is_empty());

        let requests = source.http.get_requests();
        let body = &requests[0].1;
        let filter = &body["variables"]["filter"];
        assert!(
            filter.get("project").is_none(),
            "Empty project_id should not produce a project filter"
        );
    }

    #[test]
    fn test_matches_criteria_low_priority_mapping() {
        let mut config = test_config();
        config.trigger_states = vec![];
        config.trigger_labels = vec![];
        config.trigger_assignee = None;
        let source = LinearSource::with_http_client(config, MockLinearClient::new());

        let mut issue = Issue::new("1", "LIN-1", "Test", "url", "linear");
        issue.set_metadata("state_name", "Backlog");
        issue.set_metadata("state_type", "backlog");
        issue.priority = IssuePriority::Low;

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Low);
    }

    #[test]
    fn test_matches_criteria_assignee_set_labels_empty_skips_label_check() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test".into(),
            trigger_labels: vec![], // empty labels
            trigger_assignee: Some("Jane Smith".to_string()),
            trigger_states: vec![],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::with_http_client(config, MockLinearClient::new());

        let mut issue = Issue::new("1", "LIN-1", "Test", "url", "linear");
        issue.set_metadata("state_name", "Backlog");
        issue.set_metadata("state_type", "backlog");
        issue.set_metadata("assignee", "Jane Smith");
        issue.set_metadata("labels", Vec::<String>::new());

        let result = source.matches_criteria(&issue);
        assert!(
            result.matches,
            "Should match because assignee matches and label check is skipped"
        );
    }

    #[test]
    fn test_format_linear_context_all_metadata() {
        let mut issue = Issue::new(
            "1",
            "LIN-1",
            "Test Issue",
            "https://linear.app/team/LIN-1",
            "linear",
        );
        issue.description = Some("A description".to_string());
        issue.priority = IssuePriority::High;
        issue.status = IssueStatus::InProgress;
        issue.set_metadata("team", "Engineering");
        issue.set_metadata("project", "Backend");
        issue.set_metadata("assignee", "Alice");

        let context = format_linear_context(&issue);
        assert!(context.contains("# Linear Issue: LIN-1"));
        assert!(context.contains("**Title:** Test Issue"));
        assert!(context.contains("## Description"));
        assert!(context.contains("A description"));
        assert!(context.contains("**Team:** Engineering"));
        assert!(context.contains("**Project:** Backend"));
        assert!(context.contains("**Assignee:** Alice"));
    }

    #[test]
    fn test_format_linear_context_without_description() {
        let issue = Issue::new("1", "LIN-1", "No Desc", "url", "linear");
        let context = format_linear_context(&issue);
        assert!(!context.contains("## Description"));
    }

    #[test]
    fn test_format_linear_context_no_metadata() {
        let issue = Issue::new("1", "LIN-1", "Minimal", "url", "linear");
        let context = format_linear_context(&issue);
        assert!(!context.contains("**Team:**"));
        assert!(!context.contains("**Project:**"));
        assert!(!context.contains("**Assignee:**"));
    }

    #[test]
    fn test_map_issue_with_all_optional_fields() {
        let config = test_config();
        let source = LinearSource::with_http_client(config, MockLinearClient::new());

        let issue = LinearIssue {
            id: "uuid-1".to_string(),
            identifier: "LIN-99".to_string(),
            title: "Full Issue".to_string(),
            description: Some("Detailed description".to_string()),
            url: "https://linear.app/team/LIN-99".to_string(),
            priority: 1,
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-02-01T00:00:00Z".to_string(),
            state: Some(LinearState {
                name: "In Progress".to_string(),
                state_type: "started".to_string(),
            }),
            labels: LabelsConnection {
                nodes: vec![
                    LinearLabel {
                        name: "bug".to_string(),
                    },
                    LinearLabel {
                        name: "priority".to_string(),
                    },
                ],
            },
            team: Some(LinearTeam {
                id: "team-1".to_string(),
                name: "Engineering".to_string(),
            }),
            project: Some(LinearProject {
                id: "proj-1".to_string(),
                name: "Backend".to_string(),
            }),
            assignee: Some(LinearUser {
                name: "Alice".to_string(),
            }),
        };

        let mapped = source.map_issue(issue);
        assert_eq!(mapped.id, "uuid-1");
        assert_eq!(mapped.short_id, "LIN-99");
        assert_eq!(mapped.description, Some("Detailed description".to_string()));
        assert_eq!(mapped.priority, IssuePriority::Critical);
        assert_eq!(mapped.status, IssueStatus::InProgress);
        assert!(mapped.created_at.is_some());
        assert!(mapped.updated_at.is_some());
        assert_eq!(
            mapped.get_metadata::<String>("state_name"),
            Some("In Progress".to_string())
        );
        assert_eq!(
            mapped.get_metadata::<String>("team"),
            Some("Engineering".to_string())
        );
        assert_eq!(
            mapped.get_metadata::<String>("project"),
            Some("Backend".to_string())
        );
        assert_eq!(
            mapped.get_metadata::<String>("assignee"),
            Some("Alice".to_string())
        );
    }

    #[test]
    fn test_map_priority_zero_and_five() {
        assert_eq!(
            LinearSource::<MockLinearClient>::map_priority(0),
            IssuePriority::None
        );
        assert_eq!(
            LinearSource::<MockLinearClient>::map_priority(5),
            IssuePriority::None
        );
        assert_eq!(
            LinearSource::<MockLinearClient>::map_priority(-1),
            IssuePriority::None
        );
    }

    #[test]
    fn test_map_status_none() {
        assert_eq!(
            LinearSource::<MockLinearClient>::map_status(None),
            IssueStatus::Open
        );
    }

    #[test]
    fn test_map_status_canceled() {
        assert_eq!(
            LinearSource::<MockLinearClient>::map_status(Some("canceled")),
            IssueStatus::Resolved
        );
    }

    #[tokio::test]
    async fn test_fetch_issues_assignee_only_no_labels() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{"data":{"issues":{"nodes":[]}}}"#,
        );
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![],
            trigger_assignee: Some("John".to_string()),
            trigger_states: vec![],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::with_http_client(config, mock);
        let _issues = source.fetch_issues().await.unwrap();

        let requests = source.http.get_requests();
        let body = &requests[0].1;
        let filter = &body["variables"]["filter"];
        assert!(filter.get("assignee").is_some());
        assert!(
            filter.get("labels").is_none(),
            "No labels configured, should not include labels filter"
        );
    }

    #[tokio::test]
    async fn test_fetch_issues_empty_assignee_skipped() {
        let mock = MockLinearClient::new();
        mock.mock_response(
            "https://api.linear.app/graphql",
            200,
            r#"{"data":{"issues":{"nodes":[]}}}"#,
        );
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![],
            trigger_assignee: Some("".to_string()), // empty assignee
            trigger_states: vec![],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::with_http_client(config, mock);
        let _issues = source.fetch_issues().await.unwrap();

        let requests = source.http.get_requests();
        let body = &requests[0].1;
        let filter = &body["variables"]["filter"];
        assert!(
            filter.get("assignee").is_none(),
            "Empty assignee should not produce filter"
        );
    }

    #[test]
    fn test_reqwest_linear_client_default() {
        let client = ReqwestLinearClient::default();
        assert!(std::mem::size_of_val(&client) > 0);
    }

    #[test]
    fn test_graphql_response_data_only_no_errors_key() {
        let json = r#"{"data": {"value": 42}}"#;
        let response: GraphQLResponse<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert!(response.data.is_some());
        assert!(response.errors.is_none());
    }

    #[test]
    fn test_graphql_response_empty_errors_list() {
        let json = r#"{"data": null, "errors": []}"#;
        let response: GraphQLResponse<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert!(response.data.is_none());
        assert!(response.errors.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_graphql_error_message_access() {
        let json = r#"{"message": "Something went wrong with the query"}"#;
        let error: GraphQLError = serde_json::from_str(json).unwrap();
        assert_eq!(error.message, "Something went wrong with the query");
    }

    #[test]
    fn test_linear_issue_deserialization_null_optional_fields() {
        let json = r#"{
            "id": "abc",
            "identifier": "TEST-99",
            "title": "Null optionals",
            "description": null,
            "url": "https://linear.app/abc",
            "priority": 0,
            "createdAt": "2024-05-01T00:00:00Z",
            "updatedAt": "2024-05-02T00:00:00Z",
            "state": null,
            "labels": {"nodes": []},
            "team": null,
            "project": null,
            "assignee": null
        }"#;
        let issue: LinearIssue = serde_json::from_str(json).unwrap();
        assert!(issue.description.is_none());
        assert!(issue.state.is_none());
        assert!(issue.team.is_none());
        assert!(issue.project.is_none());
        assert!(issue.assignee.is_none());
        assert!(issue.labels.nodes.is_empty());
    }

    #[test]
    fn test_linear_issue_deserialization_many_labels() {
        let json = r#"{
            "id": "lbl",
            "identifier": "LBL-1",
            "title": "Many labels",
            "description": "Has labels",
            "url": "https://linear.app/lbl",
            "priority": 2,
            "createdAt": "2024-01-01T00:00:00Z",
            "updatedAt": "2024-01-02T00:00:00Z",
            "state": {"name": "Todo", "type": "unstarted"},
            "labels": {"nodes": [{"name": "bug"}, {"name": "p0"}, {"name": "frontend"}, {"name": "regression"}]},
            "team": null,
            "project": null,
            "assignee": null
        }"#;
        let issue: LinearIssue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.labels.nodes.len(), 4);
        assert_eq!(issue.labels.nodes[0].name, "bug");
        assert_eq!(issue.labels.nodes[3].name, "regression");
    }

    #[test]
    fn test_labels_connection_empty_deserialization() {
        let json = r#"{"nodes": []}"#;
        let conn: LabelsConnection = serde_json::from_str(json).unwrap();
        assert!(conn.nodes.is_empty());
    }

    #[test]
    fn test_issues_response_empty_nodes() {
        let json = r#"{"issues": {"nodes": []}}"#;
        let response: IssuesResponse = serde_json::from_str(json).unwrap();
        assert!(response.issues.nodes.is_empty());
    }

    #[test]
    fn test_issues_connection_deserialization() {
        let json = r#"{"nodes": []}"#;
        let conn: IssuesConnection = serde_json::from_str(json).unwrap();
        assert!(conn.nodes.is_empty());
    }

    #[test]
    fn test_issue_response_with_full_issue() {
        let json = r#"{
            "issue": {
                "id": "ir1",
                "identifier": "IR-1",
                "title": "Response test",
                "description": "desc",
                "url": "https://linear.app/ir1",
                "priority": 1,
                "createdAt": "2024-01-01T00:00:00Z",
                "updatedAt": "2024-01-02T00:00:00Z",
                "state": {"name": "Backlog", "type": "backlog"},
                "labels": {"nodes": []},
                "team": {"id": "t", "name": "T"},
                "project": {"id": "p", "name": "P"},
                "assignee": {"name": "A"}
            }
        }"#;
        let response: IssueResponse = serde_json::from_str(json).unwrap();
        let issue = response.issue.unwrap();
        assert_eq!(issue.id, "ir1");
        assert_eq!(issue.identifier, "IR-1");
        assert!(issue.team.is_some());
        assert!(issue.project.is_some());
        assert!(issue.assignee.is_some());
    }

    #[test]
    fn test_graphql_request_variables_preserved() {
        let vars = serde_json::json!({"filter": {"team": {"id": {"eq": "t1"}}}, "first": 50});
        let request = GraphQLRequest {
            query: "query Issues($filter: IssueFilter) { issues }",
            variables: vars.clone(),
        };
        let serialized = serde_json::to_value(&request).unwrap();
        assert_eq!(serialized["variables"], vars);
        assert_eq!(
            serialized["query"],
            "query Issues($filter: IssueFilter) { issues }"
        );
    }

    #[test]
    fn test_map_issue_labels_preserved_in_metadata() {
        let source = LinearSource::new(test_config());
        let linear_issue = create_linear_issue(
            "labels-test",
            "LT-1",
            "Labels test",
            2,
            "backlog",
            "Backlog",
            vec!["alpha", "beta", "gamma"],
        );
        let issue = source.map_issue(linear_issue);
        let labels: Vec<String> = issue.get_metadata("labels").unwrap_or_default();
        assert_eq!(labels, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn test_map_issue_url_preserved() {
        let source = LinearSource::new(test_config());
        let linear_issue = create_linear_issue(
            "url-test",
            "UT-1",
            "URL test",
            3,
            "backlog",
            "Backlog",
            vec![],
        );
        let issue = source.map_issue(linear_issue);
        assert_eq!(issue.url, "https://linear.app/team/issue/UT-1");
    }

    #[test]
    fn test_map_priority_boundary_values() {
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(i32::MAX),
            IssuePriority::None
        );
        assert_eq!(
            LinearSource::<ReqwestLinearClient>::map_priority(i32::MIN),
            IssuePriority::None
        );
    }

    #[test]
    fn test_format_linear_context_priority_status_values() {
        let mut issue = Issue::new(
            "pri-1",
            "PRI-1",
            "Priority test",
            "https://linear.app/pri",
            "linear",
        );
        issue.priority = IssuePriority::Critical;
        issue.status = IssueStatus::Resolved;

        let context = format_linear_context(&issue);
        assert!(context.contains("**Priority:** critical"));
        assert!(context.contains("**Status:** resolved"));
    }

    #[test]
    fn test_format_linear_context_url_included() {
        let issue = Issue::new(
            "url-1",
            "URL-1",
            "URL test",
            "https://linear.app/my-org/issue/URL-1",
            "linear",
        );

        let context = format_linear_context(&issue);
        assert!(context.contains("**URL:** https://linear.app/my-org/issue/URL-1"));
    }

    #[test]
    fn test_linear_state_various_types() {
        for (state_type, expected_name) in [
            ("unstarted", "Todo"),
            ("backlog", "Backlog"),
            ("started", "In Progress"),
            ("completed", "Done"),
            ("canceled", "Cancelled"),
        ] {
            let json = format!(
                r#"{{"name": "{}", "type": "{}"}}"#,
                expected_name, state_type
            );
            let state: LinearState = serde_json::from_str(&json).unwrap();
            assert_eq!(state.state_type, state_type);
            assert_eq!(state.name, expected_name);
        }
    }

    #[test]
    fn test_is_issue_terminal_not_exact_prefix() {
        assert!(!LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "Complete"
        ));
        assert!(!LinearSource::<ReqwestLinearClient>::is_issue_terminal(
            "cancel"
        ));
    }

    #[test]
    fn test_map_issue_invalid_created_valid_updated() {
        let source = LinearSource::new(test_config());
        let linear_issue = LinearIssue {
            id: "mixed".to_string(),
            identifier: "MX-1".to_string(),
            title: "Mixed dates".to_string(),
            description: None,
            url: "https://linear.app/mixed".to_string(),
            priority: 2,
            created_at: "not-a-date".to_string(),
            updated_at: "2024-06-15T10:30:00.000Z".to_string(),
            state: None,
            labels: LabelsConnection { nodes: vec![] },
            team: None,
            project: None,
            assignee: None,
        };
        let issue = source.map_issue(linear_issue);
        assert!(issue.created_at.is_none());
        assert!(issue.updated_at.is_some());
    }

    #[test]
    fn test_map_issue_valid_created_invalid_updated() {
        let source = LinearSource::new(test_config());
        let linear_issue = LinearIssue {
            id: "mixed2".to_string(),
            identifier: "MX-2".to_string(),
            title: "Mixed dates 2".to_string(),
            description: None,
            url: "https://linear.app/mixed2".to_string(),
            priority: 3,
            created_at: "2024-06-15T10:30:00.000Z".to_string(),
            updated_at: "bad-date".to_string(),
            state: None,
            labels: LabelsConnection { nodes: vec![] },
            team: None,
            project: None,
            assignee: None,
        };
        let issue = source.map_issue(linear_issue);
        assert!(issue.created_at.is_some());
        assert!(issue.updated_at.is_none());
    }

    #[test]
    fn test_map_issue_source_field() {
        let source = LinearSource::new(test_config());
        let linear_issue = create_linear_issue(
            "src-test",
            "SRC-1",
            "Source check",
            2,
            "backlog",
            "Backlog",
            vec![],
        );
        let issue = source.map_issue(linear_issue);
        assert_eq!(issue.source, "linear");
    }

    #[test]
    fn test_matches_criteria_trigger_assignee_skip_label_check() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![],
            trigger_assignee: Some("Alice".to_string()),
            trigger_states: vec![],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::new(config);

        let mut issue = Issue::new(
            "skip-lbl",
            "SL-1",
            "Skip label",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("assignee", "Alice");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_matches_criteria_multiple_trigger_states_one_matches() {
        let config = LinearConfig {
            enabled: true,
            api_key: "test_key".into(),
            trigger_labels: vec![],
            trigger_assignee: None,
            trigger_states: vec![
                "backlog".to_string(),
                "todo".to_string(),
                "triage".to_string(),
            ],
            team_id: None,
            project_id: None,
            webhook_secret: None,
            ..Default::default()
        };
        let source = LinearSource::new(config);

        let mut issue = Issue::new(
            "multi-state",
            "MS-1",
            "Multi state",
            "https://example.com",
            "linear",
        );
        issue.set_metadata("state_type", "triage");

        let result = source.matches_criteria(&issue);
        assert!(result.matches);
    }

    #[test]
    fn test_linear_issue_description_with_special_chars() {
        let json = r#"{
            "id": "special",
            "identifier": "SP-1",
            "title": "Special chars",
            "description": "Line 1\nLine 2\tTabbed\n\"Quoted\"",
            "url": "https://linear.app/special",
            "priority": 2,
            "createdAt": "2024-01-01T00:00:00Z",
            "updatedAt": "2024-01-02T00:00:00Z",
            "state": null,
            "labels": {"nodes": []},
            "team": null,
            "project": null,
            "assignee": null
        }"#;
        let issue: LinearIssue = serde_json::from_str(json).unwrap();
        assert!(issue.description.as_ref().unwrap().contains('\n'));
        assert!(issue.description.as_ref().unwrap().contains('\t'));
        assert!(issue.description.as_ref().unwrap().contains('"'));
    }

    #[test]
    fn test_graphql_response_with_both_data_and_errors() {
        let json = r#"{
            "data": {"partial": "value"},
            "errors": [{"message": "Partial failure"}]
        }"#;
        let response: GraphQLResponse<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert!(response.data.is_some());
        assert!(response.errors.is_some());
        assert_eq!(
            response.errors.as_ref().unwrap()[0].message,
            "Partial failure"
        );
    }

    #[test]
    fn test_with_http_client_constructor() {
        let mock = MockLinearClient::new();
        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        assert_eq!(source.name(), "linear");
        assert_eq!(source.display_name(), "Linear");
    }

    #[test]
    fn test_linear_config_default() {
        let config = LinearConfig::default();
        assert!(config.enabled);
        assert!(config.api_key.expose().is_empty());
        assert_eq!(
            config.trigger_labels,
            vec!["auto-implement".to_string(), "claude".to_string()]
        );
        assert_eq!(
            config.trigger_states,
            vec!["backlog".to_string(), "todo".to_string()]
        );
        assert!(config.team_id.is_none());
        assert!(config.project_id.is_none());
        assert!(config.webhook_secret.is_none());
        assert!(config.trigger_assignee.is_none());
    }

    // --- Tests for create_issue coverage ---

    #[tokio::test]
    async fn test_create_issue_success() {
        let mock = SequentialMockLinearClient::new(vec![
            // find_or_create_label: labels query returns existing label
            (
                200,
                r#"{
                    "data": {
                        "issueLabels": {
                            "nodes": [{"id": "label-1", "name": "bug"}]
                        }
                    }
                }"#,
            ),
            // issueCreate mutation
            (
                200,
                r#"{
                    "data": {
                        "issueCreate": {
                            "success": true,
                            "issue": {
                                "id": "new-issue-id",
                                "identifier": "PROJ-999",
                                "url": "https://linear.app/team/issue/PROJ-999"
                            }
                        }
                    }
                }"#,
            ),
        ]);

        let mut config = test_config();
        config.team_id = Some("team-abc".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let issue = source
            .create_issue("New bug", "Bug description", &["bug".to_string()])
            .await
            .unwrap();

        assert_eq!(issue.id, "new-issue-id");
        assert_eq!(issue.short_id, "PROJ-999");
        assert_eq!(issue.title, "New bug");
        assert_eq!(issue.source, "linear");
    }

    #[tokio::test]
    async fn test_create_issue_no_team_id() {
        let mock = SequentialMockLinearClient::new(vec![]);

        let mut config = test_config();
        config.team_id = None;
        let source = LinearSource::with_http_client(config, mock);
        let result = source.create_issue("Title", "Desc", &[]).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("team_id required"));
    }

    #[tokio::test]
    async fn test_create_issue_success_false() {
        let mock = SequentialMockLinearClient::new(vec![
            // issueCreate returns success: false
            (
                200,
                r#"{
                    "data": {
                        "issueCreate": {
                            "success": false,
                            "issue": null
                        }
                    }
                }"#,
            ),
        ]);

        let mut config = test_config();
        config.team_id = Some("team-1".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let result = source.create_issue("Title", "Desc", &[]).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("success=false"));
    }

    #[tokio::test]
    async fn test_create_issue_null_payload() {
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "issueCreate": null
                }
            }"#,
        )]);

        let mut config = test_config();
        config.team_id = Some("team-1".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let result = source.create_issue("Title", "Desc", &[]).await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No response from issueCreate"));
    }

    #[tokio::test]
    async fn test_create_issue_no_issue_in_payload() {
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "issueCreate": {
                        "success": true,
                        "issue": null
                    }
                }
            }"#,
        )]);

        let mut config = test_config();
        config.team_id = Some("team-1".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let result = source.create_issue("Title", "Desc", &[]).await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("returned no issue"));
    }

    // --- Tests for find_or_create_label coverage ---

    #[tokio::test]
    async fn test_find_or_create_label_existing_label() {
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "issueLabels": {
                        "nodes": [
                            {"id": "lbl-1", "name": "Bug"},
                            {"id": "lbl-2", "name": "Feature"}
                        ]
                    }
                }
            }"#,
        )]);

        let mut config = test_config();
        config.team_id = Some("team-1".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let label_id = source.find_or_create_label("bug").await.unwrap();

        assert_eq!(label_id, "lbl-1");
    }

    #[tokio::test]
    async fn test_find_or_create_label_creates_new() {
        let mock = SequentialMockLinearClient::new(vec![
            // labels query returns no matching label
            (
                200,
                r#"{
                    "data": {
                        "issueLabels": {
                            "nodes": [{"id": "lbl-other", "name": "Other"}]
                        }
                    }
                }"#,
            ),
            // label create mutation
            (
                200,
                r#"{
                    "data": {
                        "issueLabelCreate": {
                            "success": true,
                            "issueLabel": {
                                "id": "lbl-new",
                                "name": "new-label"
                            }
                        }
                    }
                }"#,
            ),
        ]);

        let mut config = test_config();
        config.team_id = Some("team-1".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let label_id = source.find_or_create_label("new-label").await.unwrap();

        assert_eq!(label_id, "lbl-new");
    }

    #[tokio::test]
    async fn test_find_or_create_label_create_fails() {
        let mock = SequentialMockLinearClient::new(vec![
            // labels query returns empty
            (
                200,
                r#"{
                    "data": {
                        "issueLabels": {
                            "nodes": []
                        }
                    }
                }"#,
            ),
            // label create returns success: false
            (
                200,
                r#"{
                    "data": {
                        "issueLabelCreate": {
                            "success": false,
                            "issueLabel": null
                        }
                    }
                }"#,
            ),
        ]);

        let mut config = test_config();
        config.team_id = Some("team-1".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let result = source.find_or_create_label("fail-label").await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("success=false"));
    }

    #[tokio::test]
    async fn test_find_or_create_label_create_null_payload() {
        let mock = SequentialMockLinearClient::new(vec![
            (
                200,
                r#"{
                    "data": {
                        "issueLabels": {
                            "nodes": []
                        }
                    }
                }"#,
            ),
            (
                200,
                r#"{
                    "data": {
                        "issueLabelCreate": null
                    }
                }"#,
            ),
        ]);

        let mut config = test_config();
        config.team_id = Some("team-1".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let result = source.find_or_create_label("null-label").await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No response from issueLabelCreate"));
    }

    #[tokio::test]
    async fn test_find_or_create_label_create_no_label_in_payload() {
        let mock = SequentialMockLinearClient::new(vec![
            (
                200,
                r#"{
                    "data": {
                        "issueLabels": {
                            "nodes": []
                        }
                    }
                }"#,
            ),
            (
                200,
                r#"{
                    "data": {
                        "issueLabelCreate": {
                            "success": true,
                            "issueLabel": null
                        }
                    }
                }"#,
            ),
        ]);

        let mut config = test_config();
        config.team_id = Some("team-1".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let result = source.find_or_create_label("ghost-label").await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("returned no label"));
    }

    // --- Tests for list_open_issues coverage ---

    #[tokio::test]
    async fn test_list_open_issues_no_filter() {
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": [
                            {
                                "id": "open-1",
                                "identifier": "PROJ-O1",
                                "title": "Open issue 1",
                                "description": null,
                                "url": "https://linear.app/o1",
                                "priority": 2,
                                "createdAt": "2024-01-01T00:00:00Z",
                                "updatedAt": "2024-01-02T00:00:00Z",
                                "state": {"name": "Backlog", "type": "backlog"},
                                "labels": {"nodes": []},
                                "team": null,
                                "project": null,
                                "assignee": null
                            }
                        ]
                    }
                }
            }"#,
        )]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.list_open_issues("").await.unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].short_id, "PROJ-O1");
    }

    #[tokio::test]
    async fn test_list_open_issues_with_title_filter() {
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": []
                    }
                }
            }"#,
        )]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.list_open_issues("search term").await.unwrap();

        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_list_open_issues_with_team_filter() {
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "data": {
                    "issues": {
                        "nodes": []
                    }
                }
            }"#,
        )]);

        let mut config = test_config();
        config.team_id = Some("team-filter".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.list_open_issues("").await.unwrap();

        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_list_open_issues_api_error() {
        let mock = SequentialMockLinearClient::new(vec![(500, "Server Error")]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.list_open_issues("").await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_issue_with_multiple_labels() {
        let mock = SequentialMockLinearClient::new(vec![
            // find label "bug" - exists
            (
                200,
                r#"{"data":{"issueLabels":{"nodes":[{"id":"l1","name":"bug"}]}}}"#,
            ),
            // find label "priority" - does not exist, create
            (200, r#"{"data":{"issueLabels":{"nodes":[]}}}"#),
            (
                200,
                r#"{"data":{"issueLabelCreate":{"success":true,"issueLabel":{"id":"l2","name":"priority"}}}}"#,
            ),
            // issueCreate
            (
                200,
                r#"{"data":{"issueCreate":{"success":true,"issue":{"id":"multi-lbl","identifier":"ML-1","url":"https://linear.app/ml"}}}}"#,
            ),
        ]);

        let mut config = test_config();
        config.team_id = Some("team-1".to_string());
        let source = LinearSource::with_http_client(config, mock);
        let issue = source
            .create_issue(
                "Multi label issue",
                "Desc",
                &["bug".to_string(), "priority".to_string()],
            )
            .await
            .unwrap();

        assert_eq!(issue.short_id, "ML-1");
    }

    #[tokio::test]
    async fn test_find_or_create_label_no_team_id_uses_empty() {
        let mock = SequentialMockLinearClient::new(vec![
            // labels query empty
            (200, r#"{"data":{"issueLabels":{"nodes":[]}}}"#),
            // create label
            (
                200,
                r#"{"data":{"issueLabelCreate":{"success":true,"issueLabel":{"id":"new-id","name":"test"}}}}"#,
            ),
        ]);

        let mut config = test_config();
        config.team_id = None; // No team_id, uses empty string
        let source = LinearSource::with_http_client(config, mock);
        let label_id = source.find_or_create_label("test").await.unwrap();

        assert_eq!(label_id, "new-id");
    }

    #[tokio::test]
    async fn test_list_open_issues_empty_team_id_skipped() {
        let mock =
            SequentialMockLinearClient::new(vec![(200, r#"{"data":{"issues":{"nodes":[]}}}"#)]);

        let mut config = test_config();
        config.team_id = Some("".to_string()); // Empty team_id
        let source = LinearSource::with_http_client(config, mock);
        let issues = source.list_open_issues("").await.unwrap();

        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_graphql_multiple_errors_joined() {
        let mock = SequentialMockLinearClient::new(vec![(
            200,
            r#"{
                "errors": [
                    {"message": "Error one"},
                    {"message": "Error two"}
                ]
            }"#,
        )]);

        let config = test_config();
        let source = LinearSource::with_http_client(config, mock);
        let result = source.fetch_issues().await;

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Error one"));
        assert!(err_msg.contains("Error two"));
    }
}
