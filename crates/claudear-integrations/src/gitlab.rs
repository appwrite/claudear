//! GitLab API client for MR monitoring and issue management.

use crate::scm::CodeReview;
use crate::scm::PostReviewAction;
use crate::scm::PrInfo;
use crate::scm::PrStatus;
use crate::scm::PrSummary;
use crate::scm::RemoteRepo;
use crate::scm::ReviewComment;
use crate::scm::ReviewUser;
use crate::scm::ScmProvider;
use crate::scm::BODY_LIMIT;
use abnegate_http::HttpClient;
use abnegate_http::ReqwestHttpClient;
use async_trait::async_trait;
use claudear_config::config::GitLabConfig;
use claudear_core::error::Error;
use claudear_core::error::Result;
use serde::Deserialize;

/// GitLab API client for MR monitoring.
pub struct GitLabClient<H: HttpClient = ReqwestHttpClient> {
    config: GitLabConfig,
    http: H,
}

// Internal deserialization structs for GitLab API responses
#[derive(Debug, Deserialize)]
struct GitLabMergeRequest {
    #[allow(dead_code)]
    iid: i64,
    state: String, // "opened", "closed", "merged"
    source_branch: Option<String>,
    target_branch: Option<String>,
    title: Option<String>,
    author: Option<GitLabUser>,
}

#[derive(Debug, Deserialize)]
pub struct GitLabUser {
    pub id: i64,
    pub username: String,
}

#[derive(Debug, Deserialize)]
struct GitLabNote {
    id: i64,
    body: String,
    author: GitLabUser,
    created_at: String,
    updated_at: String,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    note_type: Option<String>,
    system: bool,
    // For diff notes
    position: Option<GitLabNotePosition>,
}

#[derive(Debug, Deserialize)]
struct GitLabNotePosition {
    new_path: Option<String>,
    old_path: Option<String>,
    new_line: Option<i64>,
    old_line: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct GitLabApproval {
    user: GitLabUser,
    created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitLabApprovalsResponse {
    approved_by: Vec<GitLabApproval>,
}

#[derive(Debug, Deserialize)]
struct GitLabProject {
    id: i64,
    path_with_namespace: String,
    name: String,
    default_branch: Option<String>,
    http_url_to_repo: String,
    ssh_url_to_repo: String,
    web_url: String,
    visibility: Option<String>,
    archived: bool,
}

#[derive(Debug, Deserialize)]
struct GitLabDiffResponse {
    changes: Vec<GitLabDiffChange>,
}

#[derive(Debug, Deserialize)]
struct GitLabDiffChange {
    old_path: String,
    new_path: String,
    diff: String,
}

#[derive(Debug, Deserialize)]
pub struct GitLabIssue {
    pub iid: i64,
    pub title: String,
    pub description: Option<String>,
    pub state: String,
    pub web_url: String,
    pub labels: Vec<String>,
    pub author: Option<GitLabUser>,
    pub assignees: Vec<GitLabUser>,
}

impl GitLabClient<ReqwestHttpClient> {
    /// Create a new GitLab client with the default HTTP client.
    pub fn new(config: GitLabConfig) -> Self {
        Self {
            config,
            http: ReqwestHttpClient::new()
                .unwrap_or_else(|_| ReqwestHttpClient::from(reqwest::Client::new()))
                .with_body_limit(BODY_LIMIT),
        }
    }
}

impl<H: HttpClient> GitLabClient<H> {
    /// Create a new GitLab client with a custom HTTP client.
    pub fn with_http_client(config: GitLabConfig, http: H) -> Self {
        Self { config, http }
    }

    /// Check if configured (has token).
    pub fn is_enabled(&self) -> bool {
        self.config.enabled && self.config.token.is_some()
    }

    /// Get the review trigger tag.
    pub fn review_trigger(&self) -> &str {
        &self.config.review_trigger
    }

    /// Get the list of bot handles whose review comments should be processed.
    pub fn allowed_bots(&self) -> &[String] {
        &self.config.allowed_bots
    }

    /// Get the GitLab API base URL.
    fn api_base(&self) -> &str {
        &self.config.base_url
    }

    /// URL-encode a project path for GitLab API calls.
    /// GitLab uses URL-encoded project paths like `group%2Fsubgroup%2Fproject`.
    fn encode_project_path(project: &str) -> String {
        urlencoding::encode(project).into_owned()
    }

    /// Build standard GitLab API headers.
    fn build_headers(&self, token: &str) -> Vec<(&'static str, String)> {
        vec![
            ("PRIVATE-TOKEN", token.to_string()),
            ("Accept", "application/json".to_string()),
            ("Content-Type", "application/json".to_string()),
            ("User-Agent", "claudear".to_string()),
        ]
    }

    /// Get the configured token.
    pub fn token(&self) -> Option<&str> {
        self.config.token.as_ref().map(|s| s.expose())
    }

    /// Get the webhook secret.
    pub fn webhook_secret(&self) -> Option<&str> {
        self.config.webhook_secret.as_ref().map(|s| s.expose())
    }

    /// Fetch project issues from a specific project.
    pub async fn get_project_issues(
        &self,
        project: &str,
        labels: &[String],
        state: Option<&str>,
    ) -> Result<Vec<GitLabIssue>> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let mut url = format!(
            "{}/api/v4/projects/{}/issues?per_page=100",
            self.api_base(),
            encoded
        );

        if !labels.is_empty() {
            let encoded_labels: Vec<String> = labels
                .iter()
                .map(|l| urlencoding::encode(l).into_owned())
                .collect();
            url.push_str(&format!("&labels={}", encoded_labels.join(",")));
        }
        if let Some(s) = state {
            url.push_str(&format!("&state={}", urlencoding::encode(s)));
        }

        let headers = self.build_headers(token.expose());
        let response = self.http.get(&url, headers).await?;

        if !response.is_success() {
            return Err(Error::Other(format!(
                "GitLab API error ({}): {}",
                response.status, response.body
            )));
        }

        Ok(response.json()?)
    }

    /// Fetch group issues.
    pub async fn get_group_issues(
        &self,
        group: &str,
        labels: &[String],
        state: Option<&str>,
    ) -> Result<Vec<GitLabIssue>> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(group);
        let mut url = format!(
            "{}/api/v4/groups/{}/issues?per_page=100",
            self.api_base(),
            encoded
        );

        if !labels.is_empty() {
            let encoded_labels: Vec<String> = labels
                .iter()
                .map(|l| urlencoding::encode(l).into_owned())
                .collect();
            url.push_str(&format!("&labels={}", encoded_labels.join(",")));
        }
        if let Some(s) = state {
            url.push_str(&format!("&state={}", urlencoding::encode(s)));
        }

        let headers = self.build_headers(token.expose());
        let response = self.http.get(&url, headers).await?;

        if !response.is_success() {
            return Err(Error::Other(format!(
                "GitLab API error ({}): {}",
                response.status, response.body
            )));
        }

        Ok(response.json()?)
    }

    /// Get a single issue.
    pub async fn get_issue(&self, project: &str, issue_iid: i64) -> Result<GitLabIssue> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let url = format!(
            "{}/api/v4/projects/{}/issues/{}",
            self.api_base(),
            encoded,
            issue_iid
        );

        let headers = self.build_headers(token.expose());
        let response = self.http.get(&url, headers).await?;

        if !response.is_success() {
            return Err(Error::Other(format!(
                "GitLab API error ({}): {}",
                response.status, response.body
            )));
        }

        Ok(response.json()?)
    }

    /// Get MR notes (comments) for mapping to reviews.
    async fn get_mr_notes(&self, project: &str, mr_iid: i64) -> Result<Vec<GitLabNote>> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let mut all_notes = Vec::new();
        let mut page = 1usize;
        const PER_PAGE: usize = 100;

        loop {
            let url = format!(
                "{}/api/v4/projects/{}/merge_requests/{}/notes?per_page={}&page={}&sort=asc",
                self.api_base(),
                encoded,
                mr_iid,
                PER_PAGE,
                page
            );

            let headers = self.build_headers(token.expose());
            let response = self.http.get(&url, headers).await?;

            if !response.is_success() {
                return Err(Error::Other(format!(
                    "GitLab API error ({}): {}",
                    response.status, response.body
                )));
            }

            let notes: Vec<GitLabNote> = response.json()?;
            let count = notes.len();
            all_notes.extend(notes);

            if count < PER_PAGE {
                break;
            }

            page += 1;
            if page > 100 {
                tracing::warn!(project = %project, mr_iid, "Hit pagination limit for MR notes");
                break;
            }
        }

        Ok(all_notes)
    }

    /// Get MR approvals.
    async fn get_mr_approvals(
        &self,
        project: &str,
        mr_iid: i64,
    ) -> Result<GitLabApprovalsResponse> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{}/approvals",
            self.api_base(),
            encoded,
            mr_iid
        );

        let headers = self.build_headers(token.expose());
        let response = self.http.get(&url, headers).await?;

        if !response.is_success() {
            return Err(Error::Other(format!(
                "GitLab API error ({}): {}",
                response.status, response.body
            )));
        }

        Ok(response.json()?)
    }

    /// Map a GitLab note to a CodeReview (for general notes).
    fn note_to_review(note: &GitLabNote) -> CodeReview {
        CodeReview {
            id: note.id,
            state: "COMMENTED".to_string(),
            body: Some(note.body.clone()),
            user: ReviewUser {
                id: note.author.id,
                login: note.author.username.clone(),
                user_type: None,
            },
            submitted_at: Some(note.created_at.clone()),
            html_url: None,
        }
    }

    /// Map a GitLab note to a ReviewComment (for diff notes).
    fn note_to_comment(note: &GitLabNote) -> ReviewComment {
        let (path, line, start_line) = if let Some(ref pos) = note.position {
            (
                pos.new_path
                    .clone()
                    .or(pos.old_path.clone())
                    .unwrap_or_default(),
                pos.new_line.or(pos.old_line),
                None,
            )
        } else {
            (String::new(), None, None)
        };

        ReviewComment {
            id: note.id,
            path,
            position: None,
            original_position: None,
            body: note.body.clone(),
            user: ReviewUser {
                id: note.author.id,
                login: note.author.username.clone(),
                user_type: None,
            },
            created_at: note.created_at.clone(),
            updated_at: note.updated_at.clone(),
            html_url: String::new(),
            pull_request_review_id: None,
            line,
            start_line,
            side: None,
        }
    }
    /// Get the latest release for a project.
    pub async fn get_latest_release(
        &self,
        project: &str,
    ) -> Result<Option<crate::scm::ScmRelease>> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let url = format!(
            "{}/api/v4/projects/{}/releases?per_page=1&order_by=released_at&sort=desc",
            self.api_base(),
            encoded
        );
        let headers = self.build_headers(token.expose());
        let response = self.http.get(&url, headers).await?;

        if response.status == 404 {
            return Ok(None);
        }

        if !response.is_success() {
            return Err(Error::Other(format!(
                "Failed to get releases for {}: {}",
                project, response.body
            )));
        }

        let releases: Vec<serde_json::Value> = response.json()?;
        let val = match releases.into_iter().next() {
            Some(v) => v,
            None => return Ok(None),
        };

        Ok(Some(crate::scm::ScmRelease {
            tag: val
                .get("tag_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            name: val.get("name").and_then(|v| v.as_str()).map(String::from),
            url: val
                .get("_links")
                .and_then(|l| l.get("self"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            published_at: val
                .get("released_at")
                .and_then(|v| v.as_str())
                .map(String::from),
        }))
    }

    /// Create a release for a project.
    pub async fn create_release(
        &self,
        project: &str,
        tag: &str,
        name: &str,
        body: &str,
    ) -> Result<crate::scm::ScmRelease> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let url = format!("{}/api/v4/projects/{}/releases", self.api_base(), encoded);
        let headers = self.build_headers(token.expose());
        let payload = serde_json::json!({
            "tag_name": tag,
            "name": name,
            "description": body,
        });

        let response = self.http.post(&url, headers, &payload.to_string()).await?;

        if !response.is_success() {
            return Err(Error::Other(format!(
                "Failed to create release for {}: {}",
                project, response.body
            )));
        }

        let val: serde_json::Value = response.json()?;
        Ok(crate::scm::ScmRelease {
            tag: val
                .get("tag_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            name: val.get("name").and_then(|v| v.as_str()).map(String::from),
            url: val
                .get("_links")
                .and_then(|l| l.get("self"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            published_at: val
                .get("released_at")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }
}

#[async_trait]
impl<H: HttpClient> ScmProvider for GitLabClient<H> {
    fn name(&self) -> &str {
        "gitlab"
    }

    fn is_enabled(&self) -> bool {
        self.is_enabled()
    }

    fn review_trigger(&self) -> &str {
        self.review_trigger()
    }

    fn allowed_bots(&self) -> &[String] {
        self.allowed_bots()
    }

    async fn get_pr_status(&self, project: &str, number: i64) -> Result<PrStatus> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{}",
            self.api_base(),
            encoded,
            number
        );
        let headers = self.build_headers(token.expose());
        let response = self.http.get(&url, headers).await?;

        if response.is_not_found() {
            return Err(Error::Other(format!(
                "MR not found: {}!{}",
                project, number
            )));
        }

        if !response.is_success() {
            return Err(Error::Other(format!("GitLab API error: {}", response.body)));
        }

        let mr: GitLabMergeRequest = response.json()?;

        Ok(match mr.state.as_str() {
            "merged" => PrStatus::Merged,
            "closed" => PrStatus::Closed,
            _ => PrStatus::Open,
        })
    }

    async fn get_pr_info(&self, project: &str, number: i64) -> Result<PrInfo> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{}",
            self.api_base(),
            encoded,
            number
        );
        let headers = self.build_headers(token.expose());
        let response = self.http.get(&url, headers).await?;

        if !response.is_success() {
            return Err(Error::Other(format!("GitLab API error: {}", response.body)));
        }

        let mr: GitLabMergeRequest = response.json()?;

        Ok(PrInfo {
            head_branch: mr.source_branch,
            base_branch: mr.target_branch,
            title: mr.title,
            author: mr.author.map(|a| a.username),
        })
    }

    async fn get_pr_diff(&self, project: &str, number: i64) -> Result<String> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{}/changes",
            self.api_base(),
            encoded,
            number
        );
        let headers = self.build_headers(token.expose());
        let response = self.http.get(&url, headers).await?;

        if !response.is_success() {
            return Err(Error::Other(format!(
                "GitLab API error fetching diff for {}!{}: {}",
                project, number, response.body
            )));
        }

        // Parse the changes response and format as unified diff
        let diff_resp: GitLabDiffResponse = response.json()?;
        let mut unified_diff = String::new();
        for change in diff_resp.changes {
            unified_diff.push_str(&format!(
                "--- a/{}\n+++ b/{}\n",
                change.old_path, change.new_path
            ));
            unified_diff.push_str(&change.diff);
            unified_diff.push('\n');
        }

        Ok(unified_diff)
    }

    async fn get_reviews(&self, project: &str, number: i64) -> Result<Vec<CodeReview>> {
        let notes = self.get_mr_notes(project, number).await?;
        let approvals =
            self.get_mr_approvals(project, number)
                .await
                .unwrap_or(GitLabApprovalsResponse {
                    approved_by: vec![],
                });

        let mut reviews = Vec::new();

        // Map general notes (non-diff, non-system) to reviews
        for note in &notes {
            if note.system {
                continue;
            }
            if note.position.is_some() {
                continue; // Diff notes go to review comments
            }
            reviews.push(Self::note_to_review(note));
        }

        // Map approvals to reviews (use negative user ID to avoid collision with note IDs)
        for approval in &approvals.approved_by {
            reviews.push(CodeReview {
                id: -(approval.user.id),
                state: "APPROVED".to_string(),
                body: None,
                user: ReviewUser {
                    id: approval.user.id,
                    login: approval.user.username.clone(),
                    user_type: None,
                },
                submitted_at: approval.created_at.clone(),
                html_url: None,
            });
        }

        Ok(reviews)
    }

    async fn get_review_comments(&self, project: &str, number: i64) -> Result<Vec<ReviewComment>> {
        let notes = self.get_mr_notes(project, number).await?;

        let mut comments = Vec::new();

        for note in &notes {
            if note.system {
                continue;
            }
            if note.position.is_none() {
                continue; // General notes go to reviews
            }
            comments.push(Self::note_to_comment(note));
        }

        Ok(comments)
    }

    async fn list_repos(&self, group: &str) -> Result<Vec<RemoteRepo>> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(group);
        let mut all_repos = Vec::new();
        let mut page = 1;
        const PER_PAGE: usize = 100;

        loop {
            let url = format!(
                "{}/api/v4/groups/{}/projects?per_page={}&page={}&include_subgroups=true",
                self.api_base(),
                encoded,
                PER_PAGE,
                page
            );
            let headers = self.build_headers(token.expose());
            let response = self.http.get(&url, headers).await?;

            if response.is_not_found() {
                return Err(Error::Other(format!("Group not found: {}", group)));
            }

            if !response.is_success() {
                return Err(Error::Other(format!(
                    "GitLab API error ({}): {}",
                    response.status, response.body
                )));
            }

            let projects: Vec<GitLabProject> = response.json()?;
            let count = projects.len();

            let active_repos: Vec<RemoteRepo> = projects
                .into_iter()
                .filter(|p| !p.archived)
                .map(|p| RemoteRepo {
                    id: p.id,
                    full_name: p.path_with_namespace,
                    name: p.name,
                    default_branch: p.default_branch.unwrap_or_else(|| "main".to_string()),
                    clone_url: p.http_url_to_repo,
                    ssh_url: p.ssh_url_to_repo,
                    html_url: p.web_url,
                    private: p.visibility.as_deref() != Some("public"),
                    archived: false,
                })
                .collect();
            all_repos.extend(active_repos);

            if count < PER_PAGE {
                break;
            }

            page += 1;
            if page > 100 {
                tracing::warn!(group = %group, "Hit pagination limit for group projects");
                break;
            }
        }

        tracing::info!(group = %group, count = all_repos.len(), "Fetched group projects");
        Ok(all_repos)
    }

    async fn merge_pr(&self, project: &str, number: i64) -> Result<()> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{}/merge",
            self.api_base(),
            encoded,
            number
        );
        let headers = self.build_headers(token.expose());
        let body = serde_json::json!({"squash": true}).to_string();

        let response = self.http.put(&url, headers, &body).await?;
        if !response.is_success() {
            return Err(Error::Other(format!(
                "Failed to merge MR {}!{}: {}",
                project, number, response.body
            )));
        }
        Ok(())
    }

    async fn close_pr(&self, project: &str, number: i64) -> Result<()> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{}",
            self.api_base(),
            encoded,
            number
        );
        let headers = self.build_headers(token.expose());
        let body = serde_json::json!({"state_event": "close"}).to_string();

        let response = self.http.put(&url, headers, &body).await?;
        if !response.is_success() {
            return Err(Error::Other(format!(
                "Failed to close MR {}!{}: {}",
                project, number, response.body
            )));
        }
        Ok(())
    }

    async fn delete_branch(&self, project: &str, branch: &str) -> Result<()> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let branch_encoded = urlencoding::encode(branch);
        let url = format!(
            "{}/api/v4/projects/{}/repository/branches/{}",
            self.api_base(),
            encoded,
            branch_encoded
        );
        let headers = self.build_headers(token.expose());

        let response = self.http.delete(&url, headers).await?;
        if !response.is_success() && !response.is_not_found() {
            return Err(Error::Other(format!(
                "Failed to delete branch {} in {}: {}",
                branch, project, response.body
            )));
        }
        Ok(())
    }

    async fn post_review(
        &self,
        project: &str,
        number: i64,
        action: PostReviewAction,
        body_text: &str,
    ) -> Result<()> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);

        match action {
            PostReviewAction::Comment => {
                let url = format!(
                    "{}/api/v4/projects/{}/merge_requests/{}/notes",
                    self.api_base(),
                    encoded,
                    number
                );
                let headers = self.build_headers(token.expose());
                let body = serde_json::json!({"body": body_text}).to_string();

                let response = self.http.post(&url, headers, &body).await?;
                if !response.is_success() {
                    return Err(Error::Other(format!(
                        "Failed to post comment on MR {}!{}: {}",
                        project, number, response.body
                    )));
                }
            }
            PostReviewAction::Approve => {
                let url = format!(
                    "{}/api/v4/projects/{}/merge_requests/{}/approve",
                    self.api_base(),
                    encoded,
                    number
                );
                let headers = self.build_headers(token.expose());

                let response = self.http.post(&url, headers, "{}").await?;
                if !response.is_success() {
                    return Err(Error::Other(format!(
                        "Failed to approve MR {}!{}: {}",
                        project, number, response.body
                    )));
                }
            }
            PostReviewAction::RequestChanges => {
                // GitLab doesn't have a direct "request changes" action.
                // Unapprove + post a note indicating changes are requested.
                let unapprove_url = format!(
                    "{}/api/v4/projects/{}/merge_requests/{}/unapprove",
                    self.api_base(),
                    encoded,
                    number
                );
                let headers = self.build_headers(token.expose());
                // Unapprove may fail if not previously approved -- ignore errors
                let _ = self.http.post(&unapprove_url, headers.clone(), "{}").await;

                let note_url = format!(
                    "{}/api/v4/projects/{}/merge_requests/{}/notes",
                    self.api_base(),
                    encoded,
                    number
                );
                let body = serde_json::json!({"body": body_text}).to_string();
                let response = self.http.post(&note_url, headers, &body).await?;
                if !response.is_success() {
                    return Err(Error::Other(format!(
                        "Failed to post changes-requested note on MR {}!{}: {}",
                        project, number, response.body
                    )));
                }
            }
        }
        Ok(())
    }

    async fn list_open_prs(&self, project: &str) -> Result<Vec<PrSummary>> {
        let token = self
            .config
            .token
            .as_ref()
            .ok_or_else(|| Error::config("GitLab token not configured"))?;

        let encoded = Self::encode_project_path(project);
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests?state=opened&per_page=100",
            self.api_base(),
            encoded
        );
        let headers = self.build_headers(token.expose());
        let response = self.http.get(&url, headers).await?;

        if !response.is_success() {
            return Err(Error::Other(format!(
                "Failed to list open MRs for {}: {}",
                project, response.body
            )));
        }

        let mrs: Vec<serde_json::Value> = response.json()?;
        Ok(mrs
            .into_iter()
            .filter_map(|mr| {
                Some(PrSummary {
                    number: mr.get("iid")?.as_i64()?,
                    title: mr.get("title")?.as_str()?.to_string(),
                    branch: mr.get("source_branch")?.as_str()?.to_string(),
                    url: mr.get("web_url")?.as_str()?.to_string(),
                })
            })
            .collect())
    }

    async fn get_pr_branch(&self, project: &str, number: i64) -> Result<String> {
        let info = self.get_pr_info(project, number).await?;
        info.head_branch.ok_or_else(|| {
            Error::Other(format!(
                "No source branch found for MR {} in {}",
                number, project
            ))
        })
    }

    fn pr_url_pattern(&self) -> &str {
        // GitLab MR URLs contain /-/merge_requests/
        "%-/merge_requests/%"
    }

    fn parse_pr_number(&self, url: &str) -> Option<i64> {
        let re = regex_lite::Regex::new(r"/merge_requests/(\d+)").ok()?;
        let caps = re.captures(url)?;
        caps.get(1)?.as_str().parse().ok()
    }

    async fn get_latest_release(&self, project: &str) -> Result<Option<crate::scm::ScmRelease>> {
        GitLabClient::get_latest_release(self, project).await
    }

    async fn create_release(
        &self,
        project: &str,
        tag: &str,
        name: &str,
        body: &str,
    ) -> Result<crate::scm::ScmRelease> {
        GitLabClient::create_release(self, project, tag, name, body).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::loopback_transport;
    use crate::test_support::ok_response;
    use crate::test_support::serve_once;
    use abnegate_http::HttpResponse;
    use claudear_config::config::GitLabConfig;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Mock HTTP client for testing GitLab API calls.
    struct MockHttpClient {
        responses: Mutex<HashMap<String, HttpResponse>>,
        /// Captured headers from the most recent request, keyed by URL.
        captured_headers: Mutex<HashMap<String, Vec<(String, String)>>>,
    }

    impl MockHttpClient {
        fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                captured_headers: Mutex::new(HashMap::new()),
            }
        }

        /// Add a mock response for a URL.
        fn mock_response(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
            self.responses
                .lock()
                .unwrap()
                .insert(url.into(), HttpResponse::new(status, body));
        }

        /// Get captured headers for a given URL.
        fn get_captured_headers(&self, url: &str) -> Option<Vec<(String, String)>> {
            self.captured_headers.lock().unwrap().get(url).cloned()
        }
    }

    #[async_trait]
    impl HttpClient for MockHttpClient {
        async fn get(
            &self,
            url: &str,
            headers: Vec<(&str, String)>,
        ) -> abnegate_http::Result<HttpResponse> {
            // Capture headers for inspection
            {
                let owned: Vec<(String, String)> = headers
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect();
                self.captured_headers
                    .lock()
                    .unwrap()
                    .insert(url.to_string(), owned);
            }

            let responses = self.responses.lock().unwrap();
            if let Some(response) = responses.get(url) {
                Ok(response.clone())
            } else {
                Ok(HttpResponse::new(404, "Not found"))
            }
        }

        async fn post(
            &self,
            url: &str,
            headers: Vec<(&str, String)>,
            _body: &str,
        ) -> abnegate_http::Result<HttpResponse> {
            {
                let owned: Vec<(String, String)> = headers
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect();
                self.captured_headers
                    .lock()
                    .unwrap()
                    .insert(url.to_string(), owned);
            }
            let responses = self.responses.lock().unwrap();
            if let Some(response) = responses.get(url) {
                Ok(response.clone())
            } else {
                Ok(HttpResponse::new(404, "Not found"))
            }
        }

        async fn put(
            &self,
            url: &str,
            headers: Vec<(&str, String)>,
            _body: &str,
        ) -> abnegate_http::Result<HttpResponse> {
            {
                let owned: Vec<(String, String)> = headers
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect();
                self.captured_headers
                    .lock()
                    .unwrap()
                    .insert(url.to_string(), owned);
            }
            let responses = self.responses.lock().unwrap();
            if let Some(response) = responses.get(url) {
                Ok(response.clone())
            } else {
                Ok(HttpResponse::new(404, "Not found"))
            }
        }

        async fn delete(
            &self,
            url: &str,
            headers: Vec<(&str, String)>,
        ) -> abnegate_http::Result<HttpResponse> {
            {
                let owned: Vec<(String, String)> = headers
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect();
                self.captured_headers
                    .lock()
                    .unwrap()
                    .insert(url.to_string(), owned);
            }
            let responses = self.responses.lock().unwrap();
            if let Some(response) = responses.get(url) {
                Ok(response.clone())
            } else {
                Ok(HttpResponse::new(404, "Not found"))
            }
        }
    }

    fn test_config() -> GitLabConfig {
        let mut config = GitLabConfig::test_default();
        config.groups = vec![];
        config.trigger_labels = vec![];
        config.trigger_states = vec![];
        config
    }

    #[test]
    fn the_default_transport_reads_up_to_the_scm_limit() {
        let client = GitLabClient::new(test_config());

        let transport = format!("{:?}", client.http);

        assert!(
            transport.contains(&format!("body_limit: {BODY_LIMIT}")),
            "{transport}"
        );
    }

    #[tokio::test]
    async fn the_scm_limit_admits_a_diff_larger_than_the_crate_default() {
        let size = ReqwestHttpClient::DEFAULT_BODY_LIMIT + 1;
        let url = serve_once(ok_response(size, &vec![b'+'; size])).await;
        let client = GitLabClient::with_http_client(test_config(), loopback_transport(BODY_LIMIT));

        let response = client
            .http
            .get(&url, Vec::new())
            .await
            .expect("a diff under the SCM limit is read");

        assert_eq!(response.body.len(), size);
    }

    #[tokio::test]
    async fn the_scm_limit_refuses_a_larger_body() {
        let url = serve_once(ok_response(BODY_LIMIT + 1, b"")).await;
        let client = GitLabClient::with_http_client(test_config(), loopback_transport(BODY_LIMIT));

        let error = client
            .http
            .get(&url, Vec::new())
            .await
            .expect_err("a declared length over the SCM limit is refused");

        assert!(
            matches!(
                error,
                abnegate_http::Error::OversizedBody { limit, .. } if limit == BODY_LIMIT
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn test_get_mr_status_open() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1, "state": "opened"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let status = client.get_pr_status("group/repo", 1).await.unwrap();
        assert_eq!(status, PrStatus::Open);
    }

    #[tokio::test]
    async fn test_get_mr_status_merged() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1, "state": "merged"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let status = client.get_pr_status("group/repo", 1).await.unwrap();
        assert_eq!(status, PrStatus::Merged);
    }

    #[tokio::test]
    async fn test_get_mr_status_closed() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1, "state": "closed"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let status = client.get_pr_status("group/repo", 1).await.unwrap();
        assert_eq!(status, PrStatus::Closed);
    }

    #[tokio::test]
    async fn test_get_mr_info() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1, "state": "opened", "source_branch": "feature", "target_branch": "main", "title": "Add feature", "author": {"id": 1, "username": "dev"}}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let info = client.get_pr_info("group/repo", 1).await.unwrap();
        assert_eq!(info.head_branch, Some("feature".to_string()));
        assert_eq!(info.base_branch, Some("main".to_string()));
        assert_eq!(info.title, Some("Add feature".to_string()));
        assert_eq!(info.author, Some("dev".to_string()));
    }

    #[test]
    fn test_encode_project_path() {
        assert_eq!(
            GitLabClient::<ReqwestHttpClient>::encode_project_path("group/repo"),
            "group%2Frepo"
        );
        assert_eq!(
            GitLabClient::<ReqwestHttpClient>::encode_project_path("group/subgroup/repo"),
            "group%2Fsubgroup%2Frepo"
        );
    }

    #[tokio::test]
    async fn test_list_repos() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            r#"[{
                "id": 1,
                "path_with_namespace": "mygroup/repo1",
                "name": "repo1",
                "default_branch": "main",
                "http_url_to_repo": "https://gitlab.com/mygroup/repo1.git",
                "ssh_url_to_repo": "git@gitlab.com:mygroup/repo1.git",
                "web_url": "https://gitlab.com/mygroup/repo1",
                "visibility": "private",
                "archived": false
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("mygroup").await.unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].full_name, "mygroup/repo1");
        assert!(repos[0].private);
    }

    #[test]
    fn test_is_enabled_with_token() {
        let config = test_config();
        let client = GitLabClient::new(config);
        assert!(client.is_enabled());
    }

    #[test]
    fn test_is_enabled_without_token() {
        let mut config = test_config();
        config.token = None;
        let client = GitLabClient::new(config);
        assert!(!client.is_enabled());
    }

    #[test]
    fn test_is_enabled_disabled() {
        let mut config = test_config();
        config.enabled = false;
        let client = GitLabClient::new(config);
        assert!(!client.is_enabled());
    }

    fn no_token_config() -> GitLabConfig {
        GitLabConfig {
            token: None,
            ..GitLabConfig::test_default()
        }
    }

    #[tokio::test]
    async fn test_get_pr_status_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.get_pr_status("group/project", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_pr_info_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.get_pr_info("group/project", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_pr_diff_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.get_pr_diff("group/project", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_reviews_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.get_reviews("group/project", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_review_comments_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.get_review_comments("group/project", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_list_repos_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.list_repos("mygroup").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_pr_status_500() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_status("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Internal Server Error") || err_msg.contains("API error"),
            "error should contain API error info: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_pr_status_404_mr_not_found() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/999",
            404,
            "Not found",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_status("group/repo", 999).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("MR not found"),
            "error should say MR not found: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_pr_info_500() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_info("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Internal Server Error") || err_msg.contains("API error"),
            "error should contain API error info: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_list_repos_404() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/nonexistent/projects?per_page=100&page=1&include_subgroups=true",
            404,
            "Group not found",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.list_repos("nonexistent").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not found") || err_msg.contains("Group not found"),
            "error should mention not found: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_reviews_with_notes_and_approvals() {
        let mock = MockHttpClient::new();

        // Notes endpoint (page 1 -- fewer than PER_PAGE so pagination stops)
        let notes_json = r#"[
            {
                "id": 101,
                "body": "Looks good overall",
                "author": { "id": 10, "username": "reviewer_a" },
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T01:00:00Z",
                "type": null,
                "system": false,
                "position": null
            },
            {
                "id": 102,
                "body": "System note: merged",
                "author": { "id": 10, "username": "reviewer_a" },
                "created_at": "2025-01-02T00:00:00Z",
                "updated_at": "2025-01-02T00:00:00Z",
                "type": null,
                "system": true,
                "position": null
            },
            {
                "id": 103,
                "body": "Diff-level remark",
                "author": { "id": 20, "username": "reviewer_b" },
                "created_at": "2025-01-03T00:00:00Z",
                "updated_at": "2025-01-03T01:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {
                    "new_path": "src/main.rs",
                    "old_path": "src/main.rs",
                    "new_line": 42,
                    "old_line": null
                }
            }
        ]"#;
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            notes_json,
        );

        // Approvals endpoint
        let approvals_json = r#"{
            "approved_by": [
                {
                    "user": { "id": 30, "username": "approver_c" },
                    "created_at": "2025-01-04T00:00:00Z"
                }
            ]
        }"#;
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            approvals_json,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 1).await.unwrap();

        // Should include: note 101 (general comment), skip note 102 (system),
        // skip note 103 (diff note), plus 1 approval = 2 total
        assert_eq!(
            reviews.len(),
            2,
            "expected 2 reviews, got {}",
            reviews.len()
        );

        // First review: the general note
        assert_eq!(reviews[0].id, 101);
        assert_eq!(reviews[0].state, "COMMENTED");
        assert_eq!(reviews[0].body.as_deref(), Some("Looks good overall"));
        assert_eq!(reviews[0].user.login, "reviewer_a");
        assert_eq!(reviews[0].user.id, 10);
        assert_eq!(
            reviews[0].submitted_at.as_deref(),
            Some("2025-01-01T00:00:00Z")
        );

        // Second review: the approval (negative user ID to avoid collision with note IDs)
        assert_eq!(reviews[1].id, -30);
        assert_eq!(reviews[1].state, "APPROVED");
        assert!(reviews[1].body.is_none());
        assert_eq!(reviews[1].user.login, "approver_c");
        assert_eq!(reviews[1].user.id, 30);
        assert_eq!(
            reviews[1].submitted_at.as_deref(),
            Some("2025-01-04T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn test_get_review_comments_with_diff_notes() {
        let mock = MockHttpClient::new();

        let notes_json = r#"[
            {
                "id": 201,
                "body": "This is a general comment, not a diff note",
                "author": { "id": 10, "username": "reviewer_a" },
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T01:00:00Z",
                "type": null,
                "system": false,
                "position": null
            },
            {
                "id": 202,
                "body": "Fix this line",
                "author": { "id": 20, "username": "reviewer_b" },
                "created_at": "2025-02-01T00:00:00Z",
                "updated_at": "2025-02-01T01:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {
                    "new_path": "src/lib.rs",
                    "old_path": "src/lib.rs",
                    "new_line": 15,
                    "old_line": 12
                }
            },
            {
                "id": 203,
                "body": "Added in new file",
                "author": { "id": 30, "username": "reviewer_c" },
                "created_at": "2025-03-01T00:00:00Z",
                "updated_at": "2025-03-01T01:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {
                    "new_path": "src/new_module.rs",
                    "old_path": null,
                    "new_line": 5,
                    "old_line": null
                }
            },
            {
                "id": 204,
                "body": "System auto-merge",
                "author": { "id": 1, "username": "system" },
                "created_at": "2025-03-02T00:00:00Z",
                "updated_at": "2025-03-02T00:00:00Z",
                "type": null,
                "system": true,
                "position": null
            }
        ]"#;
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/5/notes?per_page=100&page=1&sort=asc",
            200,
            notes_json,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let comments = client.get_review_comments("group/repo", 5).await.unwrap();

        // Only diff notes (non-system, with position) should be returned:
        // note 202 and 203; note 201 has no position, note 204 is system.
        assert_eq!(
            comments.len(),
            2,
            "expected 2 diff-note comments, got {}",
            comments.len()
        );

        // First diff comment
        assert_eq!(comments[0].id, 202);
        assert_eq!(comments[0].body, "Fix this line");
        assert_eq!(comments[0].path, "src/lib.rs");
        assert_eq!(comments[0].line, Some(15)); // new_line preferred
        assert_eq!(comments[0].user.login, "reviewer_b");
        assert_eq!(comments[0].user.id, 20);
        assert_eq!(comments[0].created_at, "2025-02-01T00:00:00Z");
        assert_eq!(comments[0].updated_at, "2025-02-01T01:00:00Z");

        // Second diff comment (new file, no old_path)
        assert_eq!(comments[1].id, 203);
        assert_eq!(comments[1].body, "Added in new file");
        assert_eq!(comments[1].path, "src/new_module.rs");
        assert_eq!(comments[1].line, Some(5));
        assert_eq!(comments[1].user.login, "reviewer_c");
    }

    #[tokio::test]
    async fn test_get_pr_diff_success() {
        let mock = MockHttpClient::new();

        let changes_json = r#"{
            "changes": [
                {
                    "old_path": "src/main.rs",
                    "new_path": "src/main.rs",
                    "diff": "@@ -1,3 +1,4 @@\n fn main() {\n+    println!(\"hello\");\n }\n"
                },
                {
                    "old_path": "README.md",
                    "new_path": "README.md",
                    "diff": "@@ -1 +1,2 @@\n # Project\n+Some docs\n"
                }
            ]
        }"#;
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/7/changes",
            200,
            changes_json,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let diff = client.get_pr_diff("group/repo", 7).await.unwrap();

        // Verify unified diff header for first change
        assert!(
            diff.contains("--- a/src/main.rs"),
            "diff should contain old path header"
        );
        assert!(
            diff.contains("+++ b/src/main.rs"),
            "diff should contain new path header"
        );
        assert!(
            diff.contains("println!(\"hello\")"),
            "diff should contain the actual change"
        );

        // Verify unified diff header for second change
        assert!(
            diff.contains("--- a/README.md"),
            "diff should contain README old path header"
        );
        assert!(
            diff.contains("+++ b/README.md"),
            "diff should contain README new path header"
        );
        assert!(
            diff.contains("Some docs"),
            "diff should contain README change"
        );
    }

    #[test]
    fn test_encode_project_path_no_slashes() {
        assert_eq!(
            GitLabClient::<MockHttpClient>::encode_project_path("simple-project"),
            "simple-project"
        );
    }

    #[test]
    fn test_encode_project_path_deeply_nested() {
        assert_eq!(
            GitLabClient::<MockHttpClient>::encode_project_path("a/b/c/d/e"),
            "a%2Fb%2Fc%2Fd%2Fe"
        );
    }

    #[test]
    fn test_encode_project_path_empty() {
        assert_eq!(GitLabClient::<MockHttpClient>::encode_project_path(""), "");
    }

    #[test]
    fn test_encode_project_path_trailing_slash() {
        assert_eq!(
            GitLabClient::<MockHttpClient>::encode_project_path("group/"),
            "group%2F"
        );
    }

    #[test]
    fn test_encode_project_path_leading_slash() {
        assert_eq!(
            GitLabClient::<MockHttpClient>::encode_project_path("/repo"),
            "%2Frepo"
        );
    }

    #[test]
    fn test_encode_project_path_consecutive_slashes() {
        assert_eq!(
            GitLabClient::<MockHttpClient>::encode_project_path("a//b"),
            "a%2F%2Fb"
        );
    }

    #[test]
    fn test_encode_project_path_with_hyphens_and_dots() {
        assert_eq!(
            GitLabClient::<MockHttpClient>::encode_project_path("my-group/my.project"),
            "my-group%2Fmy.project"
        );
    }

    #[tokio::test]
    async fn test_get_pr_status_unknown_state_maps_to_open() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1, "state": "some_unknown_state"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let status = client.get_pr_status("group/repo", 1).await.unwrap();
        assert_eq!(status, PrStatus::Open);
    }

    #[tokio::test]
    async fn test_get_pr_status_invalid_json() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            "this is not json",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_status("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("JSON") || err_msg.contains("parse"),
            "error should mention JSON parse: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_pr_status_nested_project_path() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/org%2Fteam%2Frepo/merge_requests/42",
            200,
            r#"{"iid": 42, "state": "opened"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let status = client.get_pr_status("org/team/repo", 42).await.unwrap();
        assert_eq!(status, PrStatus::Open);
    }

    #[tokio::test]
    async fn test_get_pr_status_401_unauthorized() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            401,
            r#"{"message":"401 Unauthorized"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_status("group/repo", 1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_pr_info_optional_fields_missing() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/2",
            200,
            r#"{"iid": 2, "state": "opened"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let info = client.get_pr_info("group/repo", 2).await.unwrap();
        assert!(info.head_branch.is_none());
        assert!(info.base_branch.is_none());
        assert!(info.title.is_none());
        assert!(info.author.is_none());
    }

    #[tokio::test]
    async fn test_get_pr_info_partial_fields() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/3",
            200,
            r#"{"iid": 3, "state": "merged", "source_branch": "fix-bug", "title": "Fix critical bug"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let info = client.get_pr_info("group/repo", 3).await.unwrap();
        assert_eq!(info.head_branch, Some("fix-bug".to_string()));
        assert!(info.base_branch.is_none());
        assert_eq!(info.title, Some("Fix critical bug".to_string()));
        assert!(info.author.is_none());
    }

    #[tokio::test]
    async fn test_get_pr_info_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            503,
            "Service Unavailable",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_info("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Service Unavailable") || err_msg.contains("API error"),
            "error should contain API error info: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_pr_info_invalid_json() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            "{broken json",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_info("group/repo", 1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_pr_diff_empty_changes() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/10/changes",
            200,
            r#"{"changes": []}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let diff = client.get_pr_diff("group/repo", 10).await.unwrap();
        assert!(diff.is_empty(), "empty changes should produce empty diff");
    }

    #[tokio::test]
    async fn test_get_pr_diff_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/10/changes",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_diff("group/repo", 10).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Internal Server Error"),
            "error should mention the API failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_pr_diff_invalid_json() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/10/changes",
            200,
            "not json at all",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_diff("group/repo", 10).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_pr_diff_single_change() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/11/changes",
            200,
            r#"{"changes": [{"old_path": "a.rs", "new_path": "b.rs", "diff": "@@ -1 +1 @@\n-old\n+new\n"}]}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let diff = client.get_pr_diff("group/repo", 11).await.unwrap();
        assert!(diff.contains("--- a/a.rs"));
        assert!(diff.contains("+++ b/b.rs"));
        assert!(diff.contains("-old"));
        assert!(diff.contains("+new"));
    }

    #[tokio::test]
    async fn test_get_pr_diff_rename() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/12/changes",
            200,
            r#"{"changes": [{"old_path": "old_name.rs", "new_path": "new_name.rs", "diff": ""}]}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let diff = client.get_pr_diff("group/repo", 12).await.unwrap();
        assert!(diff.contains("--- a/old_name.rs"));
        assert!(diff.contains("+++ b/new_name.rs"));
    }

    #[tokio::test]
    async fn test_get_reviews_empty_notes_and_no_approvals() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            "[]",
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": []}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 1).await.unwrap();
        assert!(reviews.is_empty());
    }

    #[tokio::test]
    async fn test_get_reviews_only_system_notes_filtered_out() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 1,
                "body": "merged",
                "author": {"id": 1, "username": "bot"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": null,
                "system": true,
                "position": null
            }]"#,
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": []}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 1).await.unwrap();
        assert!(reviews.is_empty(), "system notes should be filtered out");
    }

    #[tokio::test]
    async fn test_get_reviews_only_diff_notes_excluded() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 1,
                "body": "inline comment",
                "author": {"id": 1, "username": "reviewer"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {"new_path": "main.rs", "old_path": "main.rs", "new_line": 1, "old_line": null}
            }]"#,
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": []}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 1).await.unwrap();
        assert!(
            reviews.is_empty(),
            "diff notes should not appear in reviews"
        );
    }

    #[tokio::test]
    async fn test_get_reviews_approvals_api_failure_gracefully_handled() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 50,
                "body": "Nice work",
                "author": {"id": 5, "username": "reviewer"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": null,
                "system": false,
                "position": null
            }]"#,
        );
        // Approvals endpoint returns 500 - should be handled gracefully
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 1).await.unwrap();
        // Should still return the note-based review even though approvals failed
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].id, 50);
        assert_eq!(reviews[0].state, "COMMENTED");
    }

    #[tokio::test]
    async fn test_get_reviews_multiple_approvals() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            "[]",
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": [
                {"user": {"id": 10, "username": "alice"}, "created_at": "2025-01-01T00:00:00Z"},
                {"user": {"id": 20, "username": "bob"}, "created_at": "2025-01-02T00:00:00Z"}
            ]}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 1).await.unwrap();
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].state, "APPROVED");
        assert_eq!(reviews[0].user.login, "alice");
        assert_eq!(reviews[1].state, "APPROVED");
        assert_eq!(reviews[1].user.login, "bob");
    }

    #[tokio::test]
    async fn test_get_reviews_approval_without_created_at() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            "[]",
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": [
                {"user": {"id": 10, "username": "alice"}, "created_at": null}
            ]}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 1).await.unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].state, "APPROVED");
        assert!(reviews[0].submitted_at.is_none());
    }

    #[tokio::test]
    async fn test_get_review_comments_empty() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let comments = client.get_review_comments("group/repo", 1).await.unwrap();
        assert!(comments.is_empty());
    }

    #[tokio::test]
    async fn test_get_review_comments_only_general_notes_excluded() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 1,
                "body": "general note",
                "author": {"id": 1, "username": "reviewer"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": null,
                "system": false,
                "position": null
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let comments = client.get_review_comments("group/repo", 1).await.unwrap();
        assert!(
            comments.is_empty(),
            "general notes should not appear in review comments"
        );
    }

    #[tokio::test]
    async fn test_get_review_comments_position_fallback_to_old_path() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 301,
                "body": "deleted line comment",
                "author": {"id": 5, "username": "reviewer"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {
                    "new_path": null,
                    "old_path": "deleted_file.rs",
                    "new_line": null,
                    "old_line": 10
                }
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let comments = client.get_review_comments("group/repo", 1).await.unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "deleted_file.rs");
        assert_eq!(comments[0].line, Some(10));
    }

    #[tokio::test]
    async fn test_get_review_comments_system_diff_note_excluded() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 1,
                "body": "system diff note",
                "author": {"id": 1, "username": "system"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": "DiffNote",
                "system": true,
                "position": {"new_path": "file.rs", "old_path": "file.rs", "new_line": 1, "old_line": null}
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let comments = client.get_review_comments("group/repo", 1).await.unwrap();
        assert!(
            comments.is_empty(),
            "system notes should be excluded even if they have a position"
        );
    }

    #[tokio::test]
    async fn test_get_new_reviews_no_since_returns_all() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 1,
                "body": "comment",
                "author": {"id": 1, "username": "dev"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": null,
                "system": false,
                "position": null
            }]"#,
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": []}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_new_reviews("group/repo", 1, None).await.unwrap();
        assert_eq!(reviews.len(), 1);
    }

    #[tokio::test]
    async fn test_get_new_reviews_with_since_filters() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[
                {
                    "id": 1,
                    "body": "old comment",
                    "author": {"id": 1, "username": "dev"},
                    "created_at": "2025-01-01T00:00:00Z",
                    "updated_at": "2025-01-01T00:00:00Z",
                    "type": null,
                    "system": false,
                    "position": null
                },
                {
                    "id": 2,
                    "body": "new comment",
                    "author": {"id": 2, "username": "dev2"},
                    "created_at": "2025-06-01T00:00:00Z",
                    "updated_at": "2025-06-01T00:00:00Z",
                    "type": null,
                    "system": false,
                    "position": null
                }
            ]"#,
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": []}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client
            .get_new_reviews("group/repo", 1, Some("2025-03-01T00:00:00Z"))
            .await
            .unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].id, 2);
        assert_eq!(reviews[0].body.as_deref(), Some("new comment"));
    }

    #[tokio::test]
    async fn test_get_new_reviews_since_equal_timestamp_included() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 1,
                "body": "exact match",
                "author": {"id": 1, "username": "dev"},
                "created_at": "2025-06-01T00:00:00Z",
                "updated_at": "2025-06-01T00:00:00Z",
                "type": null,
                "system": false,
                "position": null
            }]"#,
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": []}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client
            .get_new_reviews("group/repo", 1, Some("2025-06-01T00:00:00Z"))
            .await
            .unwrap();
        assert_eq!(
            reviews.len(),
            1,
            "review at exact 'since' timestamp should be included"
        );
    }

    #[tokio::test]
    async fn test_get_new_review_comments_no_since_returns_all() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 1,
                "body": "diff comment",
                "author": {"id": 1, "username": "dev"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {"new_path": "main.rs", "old_path": "main.rs", "new_line": 5, "old_line": null}
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let comments = client
            .get_new_review_comments("group/repo", 1, None)
            .await
            .unwrap();
        assert_eq!(comments.len(), 1);
    }

    #[tokio::test]
    async fn test_get_new_review_comments_with_since_filters_by_updated_at() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[
                {
                    "id": 1,
                    "body": "old diff comment",
                    "author": {"id": 1, "username": "dev"},
                    "created_at": "2025-01-01T00:00:00Z",
                    "updated_at": "2025-01-01T00:00:00Z",
                    "type": "DiffNote",
                    "system": false,
                    "position": {"new_path": "a.rs", "old_path": "a.rs", "new_line": 1, "old_line": null}
                },
                {
                    "id": 2,
                    "body": "new diff comment",
                    "author": {"id": 2, "username": "dev2"},
                    "created_at": "2025-06-01T00:00:00Z",
                    "updated_at": "2025-06-15T00:00:00Z",
                    "type": "DiffNote",
                    "system": false,
                    "position": {"new_path": "b.rs", "old_path": "b.rs", "new_line": 10, "old_line": null}
                }
            ]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let comments = client
            .get_new_review_comments("group/repo", 1, Some("2025-03-01T00:00:00Z"))
            .await
            .unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, 2);
    }

    #[tokio::test]
    async fn test_list_repos_empty_group() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/emptygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("emptygroup").await.unwrap();
        assert!(repos.is_empty());
    }

    #[tokio::test]
    async fn test_list_repos_filters_archived() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            r#"[
                {
                    "id": 1,
                    "path_with_namespace": "mygroup/active",
                    "name": "active",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/mygroup/active.git",
                    "ssh_url_to_repo": "git@gitlab.com:mygroup/active.git",
                    "web_url": "https://gitlab.com/mygroup/active",
                    "visibility": "private",
                    "archived": false
                },
                {
                    "id": 2,
                    "path_with_namespace": "mygroup/archived-repo",
                    "name": "archived-repo",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/mygroup/archived-repo.git",
                    "ssh_url_to_repo": "git@gitlab.com:mygroup/archived-repo.git",
                    "web_url": "https://gitlab.com/mygroup/archived-repo",
                    "visibility": "private",
                    "archived": true
                }
            ]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("mygroup").await.unwrap();
        assert_eq!(repos.len(), 1, "archived repos should be filtered out");
        assert_eq!(repos[0].name, "active");
        assert!(!repos[0].archived);
    }

    #[tokio::test]
    async fn test_list_repos_visibility_public_not_private() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            r#"[{
                "id": 1,
                "path_with_namespace": "mygroup/public-repo",
                "name": "public-repo",
                "default_branch": "main",
                "http_url_to_repo": "https://gitlab.com/mygroup/public-repo.git",
                "ssh_url_to_repo": "git@gitlab.com:mygroup/public-repo.git",
                "web_url": "https://gitlab.com/mygroup/public-repo",
                "visibility": "public",
                "archived": false
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("mygroup").await.unwrap();
        assert_eq!(repos.len(), 1);
        assert!(
            !repos[0].private,
            "public visibility should map to private=false"
        );
    }

    #[tokio::test]
    async fn test_list_repos_visibility_internal_is_private() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            r#"[{
                "id": 1,
                "path_with_namespace": "mygroup/internal-repo",
                "name": "internal-repo",
                "default_branch": "develop",
                "http_url_to_repo": "https://gitlab.com/mygroup/internal-repo.git",
                "ssh_url_to_repo": "git@gitlab.com:mygroup/internal-repo.git",
                "web_url": "https://gitlab.com/mygroup/internal-repo",
                "visibility": "internal",
                "archived": false
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("mygroup").await.unwrap();
        assert_eq!(repos.len(), 1);
        assert!(
            repos[0].private,
            "internal visibility should map to private=true"
        );
    }

    #[tokio::test]
    async fn test_list_repos_no_default_branch_defaults_to_main() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            r#"[{
                "id": 1,
                "path_with_namespace": "mygroup/no-default",
                "name": "no-default",
                "default_branch": null,
                "http_url_to_repo": "https://gitlab.com/mygroup/no-default.git",
                "ssh_url_to_repo": "git@gitlab.com:mygroup/no-default.git",
                "web_url": "https://gitlab.com/mygroup/no-default",
                "visibility": "private",
                "archived": false
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("mygroup").await.unwrap();
        assert_eq!(repos[0].default_branch, "main");
    }

    #[tokio::test]
    async fn test_list_repos_visibility_none_is_private() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            r#"[{
                "id": 1,
                "path_with_namespace": "mygroup/no-vis",
                "name": "no-vis",
                "default_branch": "main",
                "http_url_to_repo": "https://gitlab.com/mygroup/no-vis.git",
                "ssh_url_to_repo": "git@gitlab.com:mygroup/no-vis.git",
                "web_url": "https://gitlab.com/mygroup/no-vis",
                "visibility": null,
                "archived": false
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("mygroup").await.unwrap();
        assert!(
            repos[0].private,
            "null visibility should map to private=true"
        );
    }

    #[tokio::test]
    async fn test_list_repos_field_mapping() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            r#"[{
                "id": 42,
                "path_with_namespace": "mygroup/myrepo",
                "name": "myrepo",
                "default_branch": "develop",
                "http_url_to_repo": "https://gitlab.com/mygroup/myrepo.git",
                "ssh_url_to_repo": "git@gitlab.com:mygroup/myrepo.git",
                "web_url": "https://gitlab.com/mygroup/myrepo",
                "visibility": "private",
                "archived": false
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("mygroup").await.unwrap();
        assert_eq!(repos.len(), 1);
        let repo = &repos[0];
        assert_eq!(repo.id, 42);
        assert_eq!(repo.full_name, "mygroup/myrepo");
        assert_eq!(repo.name, "myrepo");
        assert_eq!(repo.default_branch, "develop");
        assert_eq!(repo.clone_url, "https://gitlab.com/mygroup/myrepo.git");
        assert_eq!(repo.ssh_url, "git@gitlab.com:mygroup/myrepo.git");
        assert_eq!(repo.html_url, "https://gitlab.com/mygroup/myrepo");
        assert!(repo.private);
        assert!(!repo.archived);
    }

    #[tokio::test]
    async fn test_list_repos_500_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.list_repos("mygroup").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_repos_nested_group_path_encoded() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/org%2Fsubgroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("org/subgroup").await.unwrap();
        assert!(repos.is_empty());
    }

    #[tokio::test]
    async fn test_get_project_issues_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues?per_page=100",
            200,
            r#"[{
                "iid": 1,
                "title": "Bug report",
                "description": "Something is broken",
                "state": "opened",
                "web_url": "https://gitlab.com/group/repo/-/issues/1",
                "labels": ["bug", "priority::high"],
                "author": {"id": 10, "username": "reporter"},
                "assignees": [{"id": 20, "username": "dev"}]
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let issues = client
            .get_project_issues("group/repo", &[], None)
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].iid, 1);
        assert_eq!(issues[0].title, "Bug report");
        assert_eq!(
            issues[0].description.as_deref(),
            Some("Something is broken")
        );
        assert_eq!(issues[0].state, "opened");
        assert_eq!(
            issues[0].web_url,
            "https://gitlab.com/group/repo/-/issues/1"
        );
        assert_eq!(issues[0].labels, vec!["bug", "priority::high"]);
        assert_eq!(issues[0].author.as_ref().unwrap().username, "reporter");
        assert_eq!(issues[0].assignees[0].username, "dev");
    }

    #[tokio::test]
    async fn test_get_project_issues_with_labels_and_state() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues?per_page=100&labels=bug,critical&state=opened",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let labels = vec!["bug".to_string(), "critical".to_string()];
        let issues = client
            .get_project_issues("group/repo", &labels, Some("opened"))
            .await
            .unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_get_project_issues_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues?per_page=100",
            403,
            "Forbidden",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_project_issues("group/repo", &[], None).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("403") || err_msg.contains("Forbidden"),
            "error should contain status or body: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_project_issues_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.get_project_issues("group/repo", &[], None).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_group_issues_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/issues?per_page=100&labels=auto-implement&state=opened",
            200,
            r#"[{
                "iid": 5,
                "title": "Feature request",
                "description": null,
                "state": "opened",
                "web_url": "https://gitlab.com/mygroup/repo/-/issues/5",
                "labels": ["auto-implement"],
                "author": null,
                "assignees": []
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let labels = vec!["auto-implement".to_string()];
        let issues = client
            .get_group_issues("mygroup", &labels, Some("opened"))
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].iid, 5);
        assert!(issues[0].description.is_none());
        assert!(issues[0].author.is_none());
        assert!(issues[0].assignees.is_empty());
    }

    #[tokio::test]
    async fn test_get_group_issues_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.get_group_issues("mygroup", &[], None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_group_issues_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/issues?per_page=100",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_group_issues("mygroup", &[], None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_issue_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues/42",
            200,
            r#"{
                "iid": 42,
                "title": "Single issue",
                "description": "Details here",
                "state": "opened",
                "web_url": "https://gitlab.com/group/repo/-/issues/42",
                "labels": [],
                "author": {"id": 1, "username": "admin"},
                "assignees": []
            }"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let issue = client.get_issue("group/repo", 42).await.unwrap();
        assert_eq!(issue.iid, 42);
        assert_eq!(issue.title, "Single issue");
    }

    #[tokio::test]
    async fn test_get_issue_not_found() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues/999",
            404,
            "Not found",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_issue("group/repo", 999).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_issue_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.get_issue("group/repo", 1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_auth_headers_private_token_format() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1, "state": "opened"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let _ = client.get_pr_status("group/repo", 1).await.unwrap();

        let headers = client
            .http
            .get_captured_headers(
                "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            )
            .unwrap();

        // Check PRIVATE-TOKEN header
        let token_header = headers
            .iter()
            .find(|(k, _)| k == "PRIVATE-TOKEN")
            .expect("should have PRIVATE-TOKEN header");
        assert_eq!(token_header.1, "test_token");

        // Check Accept header
        let accept_header = headers
            .iter()
            .find(|(k, _)| k == "Accept")
            .expect("should have Accept header");
        assert_eq!(accept_header.1, "application/json");

        // Check User-Agent header
        let ua_header = headers
            .iter()
            .find(|(k, _)| k == "User-Agent")
            .expect("should have User-Agent header");
        assert_eq!(ua_header.1, "claudear");
    }

    #[tokio::test]
    async fn test_auth_headers_custom_token() {
        let mut config = test_config();
        config.token = Some("my-custom-gl-token".into());

        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1, "state": "opened"}"#,
        );

        let client = GitLabClient::with_http_client(config, mock);
        let _ = client.get_pr_status("group/repo", 1).await.unwrap();

        let headers = client
            .http
            .get_captured_headers(
                "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            )
            .unwrap();

        let token_header = headers
            .iter()
            .find(|(k, _)| k == "PRIVATE-TOKEN")
            .expect("should have PRIVATE-TOKEN header");
        assert_eq!(token_header.1, "my-custom-gl-token");
    }

    #[tokio::test]
    async fn test_custom_base_url() {
        let mut config = test_config();
        config.base_url = "https://git.mycompany.com".to_string();

        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://git.mycompany.com/api/v4/projects/team%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1, "state": "merged"}"#,
        );

        let client = GitLabClient::with_http_client(config, mock);
        let status = client.get_pr_status("team/repo", 1).await.unwrap();
        assert_eq!(status, PrStatus::Merged);
    }

    #[test]
    fn test_review_trigger() {
        let config = test_config();
        let client = GitLabClient::new(config);
        assert_eq!(client.review_trigger(), "@claudear");
    }

    #[test]
    fn test_review_trigger_custom() {
        let mut config = test_config();
        config.review_trigger = "@mybot".to_string();
        let client = GitLabClient::new(config);
        assert_eq!(client.review_trigger(), "@mybot");
    }

    #[test]
    fn test_token_accessor() {
        let config = test_config();
        let client = GitLabClient::new(config);
        assert_eq!(client.token(), Some("test_token"));
    }

    #[test]
    fn test_token_accessor_none() {
        let config = no_token_config();
        let client = GitLabClient::new(config);
        assert_eq!(client.token(), None);
    }

    #[test]
    fn test_webhook_secret() {
        let config = test_config();
        let client = GitLabClient::new(config);
        assert_eq!(client.webhook_secret(), Some("test_secret"));
    }

    #[test]
    fn test_webhook_secret_none() {
        let mut config = test_config();
        config.webhook_secret = None;
        let client = GitLabClient::new(config);
        assert_eq!(client.webhook_secret(), None);
    }

    #[test]
    fn test_scm_provider_name() {
        let config = test_config();
        let client = GitLabClient::new(config);
        assert_eq!(ScmProvider::name(&client), "gitlab");
    }

    #[test]
    fn test_note_to_review_mapping() {
        let note = GitLabNote {
            id: 100,
            body: "Great work!".to_string(),
            author: GitLabUser {
                id: 5,
                username: "reviewer".to_string(),
            },
            created_at: "2025-01-15T10:00:00Z".to_string(),
            updated_at: "2025-01-15T11:00:00Z".to_string(),
            note_type: None,
            system: false,
            position: None,
        };

        let review = GitLabClient::<MockHttpClient>::note_to_review(&note);
        assert_eq!(review.id, 100);
        assert_eq!(review.state, "COMMENTED");
        assert_eq!(review.body.as_deref(), Some("Great work!"));
        assert_eq!(review.user.id, 5);
        assert_eq!(review.user.login, "reviewer");
        assert!(review.user.user_type.is_none());
        assert_eq!(review.submitted_at.as_deref(), Some("2025-01-15T10:00:00Z"));
        assert!(review.html_url.is_none());
    }

    #[test]
    fn test_note_to_comment_with_position() {
        let note = GitLabNote {
            id: 200,
            body: "Fix this line".to_string(),
            author: GitLabUser {
                id: 7,
                username: "code_reviewer".to_string(),
            },
            created_at: "2025-02-01T00:00:00Z".to_string(),
            updated_at: "2025-02-01T01:00:00Z".to_string(),
            note_type: Some("DiffNote".to_string()),
            system: false,
            position: Some(GitLabNotePosition {
                new_path: Some("src/lib.rs".to_string()),
                old_path: Some("src/lib.rs".to_string()),
                new_line: Some(42),
                old_line: Some(40),
            }),
        };

        let comment = GitLabClient::<MockHttpClient>::note_to_comment(&note);
        assert_eq!(comment.id, 200);
        assert_eq!(comment.body, "Fix this line");
        assert_eq!(comment.path, "src/lib.rs");
        assert_eq!(comment.line, Some(42)); // new_line preferred
        assert!(comment.start_line.is_none());
        assert_eq!(comment.user.id, 7);
        assert_eq!(comment.user.login, "code_reviewer");
        assert_eq!(comment.created_at, "2025-02-01T00:00:00Z");
        assert_eq!(comment.updated_at, "2025-02-01T01:00:00Z");
        assert!(comment.html_url.is_empty());
        assert!(comment.position.is_none());
        assert!(comment.original_position.is_none());
        assert!(comment.pull_request_review_id.is_none());
        assert!(comment.side.is_none());
    }

    #[test]
    fn test_note_to_comment_without_position() {
        let note = GitLabNote {
            id: 300,
            body: "general".to_string(),
            author: GitLabUser {
                id: 1,
                username: "user".to_string(),
            },
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            note_type: None,
            system: false,
            position: None,
        };

        let comment = GitLabClient::<MockHttpClient>::note_to_comment(&note);
        assert!(comment.path.is_empty());
        assert!(comment.line.is_none());
        assert!(comment.start_line.is_none());
    }

    #[test]
    fn test_note_to_comment_old_line_fallback() {
        let note = GitLabNote {
            id: 400,
            body: "on deleted line".to_string(),
            author: GitLabUser {
                id: 1,
                username: "user".to_string(),
            },
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            note_type: Some("DiffNote".to_string()),
            system: false,
            position: Some(GitLabNotePosition {
                new_path: None,
                old_path: Some("old.rs".to_string()),
                new_line: None,
                old_line: Some(99),
            }),
        };

        let comment = GitLabClient::<MockHttpClient>::note_to_comment(&note);
        assert_eq!(comment.path, "old.rs");
        assert_eq!(comment.line, Some(99));
    }

    #[test]
    fn test_note_to_comment_no_paths_at_all() {
        let note = GitLabNote {
            id: 500,
            body: "odd note".to_string(),
            author: GitLabUser {
                id: 1,
                username: "user".to_string(),
            },
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            note_type: Some("DiffNote".to_string()),
            system: false,
            position: Some(GitLabNotePosition {
                new_path: None,
                old_path: None,
                new_line: None,
                old_line: None,
            }),
        };

        let comment = GitLabClient::<MockHttpClient>::note_to_comment(&note);
        assert!(comment.path.is_empty());
        assert!(comment.line.is_none());
    }

    #[tokio::test]
    async fn test_get_reviews_notes_pagination() {
        let mock = MockHttpClient::new();

        // Build a full page of 100 notes for page 1
        let mut page1_notes = String::from("[");
        for i in 0..100 {
            if i > 0 {
                page1_notes.push(',');
            }
            page1_notes.push_str(&format!(
                r#"{{
                    "id": {},
                    "body": "note {}",
                    "author": {{"id": 1, "username": "dev"}},
                    "created_at": "2025-01-01T00:00:00Z",
                    "updated_at": "2025-01-01T00:00:00Z",
                    "type": null,
                    "system": false,
                    "position": null
                }}"#,
                i + 1,
                i + 1
            ));
        }
        page1_notes.push(']');

        // Page 2 has fewer than 100 notes (pagination stops)
        let page2_notes = r#"[{
            "id": 101,
            "body": "last note",
            "author": {"id": 1, "username": "dev"},
            "created_at": "2025-01-01T00:00:00Z",
            "updated_at": "2025-01-01T00:00:00Z",
            "type": null,
            "system": false,
            "position": null
        }]"#;

        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            &page1_notes,
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=2&sort=asc",
            200,
            page2_notes,
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": []}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 1).await.unwrap();
        assert_eq!(
            reviews.len(),
            101,
            "should get 100 from page 1 + 1 from page 2"
        );
    }

    #[tokio::test]
    async fn test_list_repos_pagination() {
        let mock = MockHttpClient::new();

        // Build a full page of 100 projects for page 1
        let mut page1 = String::from("[");
        for i in 0..100 {
            if i > 0 {
                page1.push(',');
            }
            page1.push_str(&format!(
                r#"{{
                    "id": {},
                    "path_with_namespace": "mygroup/repo-{}",
                    "name": "repo-{}",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/mygroup/repo-{}.git",
                    "ssh_url_to_repo": "git@gitlab.com:mygroup/repo-{}.git",
                    "web_url": "https://gitlab.com/mygroup/repo-{}",
                    "visibility": "private",
                    "archived": false
                }}"#,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                i + 1
            ));
        }
        page1.push(']');

        // Page 2 has just 1 project
        let page2 = r#"[{
            "id": 101,
            "path_with_namespace": "mygroup/repo-101",
            "name": "repo-101",
            "default_branch": "main",
            "http_url_to_repo": "https://gitlab.com/mygroup/repo-101.git",
            "ssh_url_to_repo": "git@gitlab.com:mygroup/repo-101.git",
            "web_url": "https://gitlab.com/mygroup/repo-101",
            "visibility": "private",
            "archived": false
        }]"#;

        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            &page1,
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=2&include_subgroups=true",
            200,
            page2,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("mygroup").await.unwrap();
        assert_eq!(repos.len(), 101);
    }

    #[tokio::test]
    async fn test_get_reviews_notes_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_reviews("group/repo", 1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_review_comments_notes_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_review_comments("group/repo", 1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_pr_status_missing_state_field() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_status("group/repo", 1).await;
        // state is a required field in GitLabMergeRequest, so this should fail
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_repos_invalid_json() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            "not valid json",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.list_repos("mygroup").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_reviews_invalid_json_in_notes() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            "definitely not json",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_reviews("group/repo", 1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_project_issues_invalid_json() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues?per_page=100",
            200,
            "{{invalid",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_project_issues("group/repo", &[], None).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_gitlab_user() {
        let json = r#"{"id": 42, "username": "jdoe"}"#;
        let user: GitLabUser = serde_json::from_str(json).unwrap();
        assert_eq!(user.id, 42);
        assert_eq!(user.username, "jdoe");
    }

    #[test]
    fn test_deserialize_gitlab_merge_request_full() {
        let json = r#"{
            "iid": 10,
            "state": "merged",
            "source_branch": "feature/x",
            "target_branch": "main",
            "title": "Add X",
            "author": {"id": 5, "username": "dev"}
        }"#;
        let mr: GitLabMergeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(mr.iid, 10);
        assert_eq!(mr.state, "merged");
        assert_eq!(mr.source_branch.as_deref(), Some("feature/x"));
        assert_eq!(mr.target_branch.as_deref(), Some("main"));
        assert_eq!(mr.title.as_deref(), Some("Add X"));
        assert!(mr.author.is_some());
        assert_eq!(mr.author.unwrap().username, "dev");
    }

    #[test]
    fn test_deserialize_gitlab_merge_request_minimal() {
        let json = r#"{"iid": 1, "state": "opened"}"#;
        let mr: GitLabMergeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(mr.iid, 1);
        assert_eq!(mr.state, "opened");
        assert!(mr.source_branch.is_none());
        assert!(mr.target_branch.is_none());
        assert!(mr.title.is_none());
        assert!(mr.author.is_none());
    }

    #[test]
    fn test_deserialize_gitlab_note_full() {
        let json = r#"{
            "id": 100,
            "body": "LGTM",
            "author": {"id": 1, "username": "reviewer"},
            "created_at": "2025-01-01T00:00:00Z",
            "updated_at": "2025-01-02T00:00:00Z",
            "type": "DiffNote",
            "system": false,
            "position": {
                "new_path": "src/lib.rs",
                "old_path": "src/old_lib.rs",
                "new_line": 10,
                "old_line": 8
            }
        }"#;
        let note: GitLabNote = serde_json::from_str(json).unwrap();
        assert_eq!(note.id, 100);
        assert_eq!(note.body, "LGTM");
        assert_eq!(note.author.id, 1);
        assert_eq!(note.note_type.as_deref(), Some("DiffNote"));
        assert!(!note.system);
        let pos = note.position.unwrap();
        assert_eq!(pos.new_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(pos.old_path.as_deref(), Some("src/old_lib.rs"));
        assert_eq!(pos.new_line, Some(10));
        assert_eq!(pos.old_line, Some(8));
    }

    #[test]
    fn test_deserialize_gitlab_note_system_no_position() {
        let json = r#"{
            "id": 200,
            "body": "changed the description",
            "author": {"id": 0, "username": "system"},
            "created_at": "2025-06-01T00:00:00Z",
            "updated_at": "2025-06-01T00:00:00Z",
            "type": null,
            "system": true,
            "position": null
        }"#;
        let note: GitLabNote = serde_json::from_str(json).unwrap();
        assert!(note.system);
        assert!(note.note_type.is_none());
        assert!(note.position.is_none());
    }

    #[test]
    fn test_deserialize_gitlab_note_position_partial() {
        let json = r#"{
            "new_path": "file.rs",
            "old_path": null,
            "new_line": null,
            "old_line": 5
        }"#;
        let pos: GitLabNotePosition = serde_json::from_str(json).unwrap();
        assert_eq!(pos.new_path.as_deref(), Some("file.rs"));
        assert!(pos.old_path.is_none());
        assert!(pos.new_line.is_none());
        assert_eq!(pos.old_line, Some(5));
    }

    #[test]
    fn test_deserialize_gitlab_approval() {
        let json = r#"{
            "user": {"id": 42, "username": "approver"},
            "created_at": "2025-03-15T12:00:00Z"
        }"#;
        let approval: GitLabApproval = serde_json::from_str(json).unwrap();
        assert_eq!(approval.user.id, 42);
        assert_eq!(approval.user.username, "approver");
        assert_eq!(approval.created_at.as_deref(), Some("2025-03-15T12:00:00Z"));
    }

    #[test]
    fn test_deserialize_gitlab_approval_no_created_at() {
        let json = r#"{
            "user": {"id": 1, "username": "user1"},
            "created_at": null
        }"#;
        let approval: GitLabApproval = serde_json::from_str(json).unwrap();
        assert!(approval.created_at.is_none());
    }

    #[test]
    fn test_deserialize_gitlab_approvals_response() {
        let json = r#"{
            "approved_by": [
                {"user": {"id": 1, "username": "a"}, "created_at": null},
                {"user": {"id": 2, "username": "b"}, "created_at": "2025-01-01T00:00:00Z"}
            ]
        }"#;
        let resp: GitLabApprovalsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.approved_by.len(), 2);
        assert_eq!(resp.approved_by[0].user.username, "a");
        assert_eq!(resp.approved_by[1].user.username, "b");
    }

    #[test]
    fn test_deserialize_gitlab_approvals_response_empty() {
        let json = r#"{"approved_by": []}"#;
        let resp: GitLabApprovalsResponse = serde_json::from_str(json).unwrap();
        assert!(resp.approved_by.is_empty());
    }

    #[test]
    fn test_deserialize_gitlab_project_full() {
        let json = r#"{
            "id": 99,
            "path_with_namespace": "org/team/repo",
            "name": "repo",
            "default_branch": "develop",
            "http_url_to_repo": "https://gitlab.com/org/team/repo.git",
            "ssh_url_to_repo": "git@gitlab.com:org/team/repo.git",
            "web_url": "https://gitlab.com/org/team/repo",
            "visibility": "internal",
            "archived": true
        }"#;
        let project: GitLabProject = serde_json::from_str(json).unwrap();
        assert_eq!(project.id, 99);
        assert_eq!(project.path_with_namespace, "org/team/repo");
        assert_eq!(project.name, "repo");
        assert_eq!(project.default_branch.as_deref(), Some("develop"));
        assert_eq!(project.visibility.as_deref(), Some("internal"));
        assert!(project.archived);
    }

    #[test]
    fn test_deserialize_gitlab_project_null_optional_fields() {
        let json = r#"{
            "id": 1,
            "path_with_namespace": "g/r",
            "name": "r",
            "default_branch": null,
            "http_url_to_repo": "https://x.git",
            "ssh_url_to_repo": "git@x:g/r.git",
            "web_url": "https://x",
            "visibility": null,
            "archived": false
        }"#;
        let project: GitLabProject = serde_json::from_str(json).unwrap();
        assert!(project.default_branch.is_none());
        assert!(project.visibility.is_none());
        assert!(!project.archived);
    }

    #[test]
    fn test_deserialize_gitlab_diff_response() {
        let json = r#"{
            "changes": [
                {"old_path": "a.rs", "new_path": "b.rs", "diff": "@@ -1 +1 @@"},
                {"old_path": "c.rs", "new_path": "c.rs", "diff": ""}
            ]
        }"#;
        let resp: GitLabDiffResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.changes.len(), 2);
        assert_eq!(resp.changes[0].old_path, "a.rs");
        assert_eq!(resp.changes[0].new_path, "b.rs");
        assert_eq!(resp.changes[1].diff, "");
    }

    #[test]
    fn test_deserialize_gitlab_diff_response_empty() {
        let json = r#"{"changes": []}"#;
        let resp: GitLabDiffResponse = serde_json::from_str(json).unwrap();
        assert!(resp.changes.is_empty());
    }

    #[test]
    fn test_deserialize_gitlab_issue_full() {
        let json = r#"{
            "iid": 7,
            "title": "Test issue",
            "description": "A description",
            "state": "closed",
            "web_url": "https://gitlab.com/g/r/-/issues/7",
            "labels": ["bug", "urgent"],
            "author": {"id": 1, "username": "admin"},
            "assignees": [
                {"id": 2, "username": "dev1"},
                {"id": 3, "username": "dev2"}
            ]
        }"#;
        let issue: GitLabIssue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.iid, 7);
        assert_eq!(issue.title, "Test issue");
        assert_eq!(issue.description.as_deref(), Some("A description"));
        assert_eq!(issue.state, "closed");
        assert_eq!(issue.labels, vec!["bug", "urgent"]);
        assert_eq!(issue.author.as_ref().unwrap().username, "admin");
        assert_eq!(issue.assignees.len(), 2);
    }

    #[test]
    fn test_deserialize_gitlab_issue_minimal() {
        let json = r#"{
            "iid": 1,
            "title": "Minimal",
            "description": null,
            "state": "opened",
            "web_url": "https://x",
            "labels": [],
            "author": null,
            "assignees": []
        }"#;
        let issue: GitLabIssue = serde_json::from_str(json).unwrap();
        assert!(issue.description.is_none());
        assert!(issue.author.is_none());
        assert!(issue.assignees.is_empty());
        assert!(issue.labels.is_empty());
    }

    #[tokio::test]
    async fn test_get_project_issues_labels_with_special_chars() {
        let mock = MockHttpClient::new();
        // Labels like "priority::high" have colons which get URL-encoded
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues?per_page=100&labels=priority%3A%3Ahigh,auto%20fix&state=opened",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let labels = vec!["priority::high".to_string(), "auto fix".to_string()];
        let issues = client
            .get_project_issues("group/repo", &labels, Some("opened"))
            .await
            .unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_get_group_issues_labels_with_special_chars() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/my%2Fgroup/issues?per_page=100&labels=auto%3A%3Aimplement&state=opened",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let labels = vec!["auto::implement".to_string()];
        let issues = client
            .get_group_issues("my/group", &labels, Some("opened"))
            .await
            .unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_get_project_issues_custom_base_url() {
        let mut config = test_config();
        config.base_url = "https://git.corp.io".to_string();

        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://git.corp.io/api/v4/projects/team%2Frepo/issues?per_page=100",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(config, mock);
        let issues = client
            .get_project_issues("team/repo", &[], None)
            .await
            .unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_get_group_issues_custom_base_url() {
        let mut config = test_config();
        config.base_url = "https://git.corp.io".to_string();

        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://git.corp.io/api/v4/groups/myteam/issues?per_page=100",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(config, mock);
        let issues = client.get_group_issues("myteam", &[], None).await.unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_get_issue_custom_base_url() {
        let mut config = test_config();
        config.base_url = "https://git.corp.io".to_string();

        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://git.corp.io/api/v4/projects/team%2Frepo/issues/1",
            200,
            r#"{
                "iid": 1, "title": "T", "description": null,
                "state": "opened", "web_url": "https://x",
                "labels": [], "author": null, "assignees": []
            }"#,
        );

        let client = GitLabClient::with_http_client(config, mock);
        let issue = client.get_issue("team/repo", 1).await.unwrap();
        assert_eq!(issue.iid, 1);
    }

    #[test]
    fn test_scm_provider_is_enabled_delegates() {
        let config = test_config();
        let client = GitLabClient::new(config);
        // ScmProvider::is_enabled should delegate to self.is_enabled()
        assert_eq!(ScmProvider::is_enabled(&client), client.is_enabled());
    }

    #[test]
    fn test_scm_provider_review_trigger_delegates() {
        let mut config = test_config();
        config.review_trigger = "@custom-bot".to_string();
        let client = GitLabClient::new(config);
        assert_eq!(ScmProvider::review_trigger(&client), "@custom-bot");
    }

    #[test]
    fn test_build_headers_contains_expected_entries() {
        let config = test_config();
        let client =
            GitLabClient::<MockHttpClient>::with_http_client(config, MockHttpClient::new());
        let headers = client.build_headers("my_token");
        assert_eq!(headers.len(), 4);

        let token_val = headers.iter().find(|(k, _)| *k == "PRIVATE-TOKEN").unwrap();
        assert_eq!(token_val.1, "my_token");

        let accept_val = headers.iter().find(|(k, _)| *k == "Accept").unwrap();
        assert_eq!(accept_val.1, "application/json");

        let ua_val = headers.iter().find(|(k, _)| *k == "User-Agent").unwrap();
        assert_eq!(ua_val.1, "claudear");
    }

    #[test]
    fn test_api_base_returns_config_base_url() {
        let mut config = test_config();
        config.base_url = "https://custom.gitlab.io".to_string();
        let client =
            GitLabClient::<MockHttpClient>::with_http_client(config, MockHttpClient::new());
        assert_eq!(client.api_base(), "https://custom.gitlab.io");
    }

    #[tokio::test]
    async fn test_get_group_issues_no_labels_no_state() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/issues?per_page=100",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let issues = client.get_group_issues("mygroup", &[], None).await.unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_get_issue_api_error_500() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues/1",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_issue("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("500") || err_msg.contains("Internal Server Error"),
            "error should mention the API failure: {err_msg}"
        );
    }

    #[test]
    fn test_with_http_client_constructor() {
        let config = test_config();
        let mock = MockHttpClient::new();
        let client = GitLabClient::with_http_client(config, mock);
        // Verify the client is correctly constructed
        assert!(client.is_enabled());
        assert_eq!(client.token(), Some("test_token"));
        assert_eq!(client.webhook_secret(), Some("test_secret"));
        assert_eq!(client.review_trigger(), "@claudear");
        assert_eq!(client.api_base(), "https://gitlab.com");
    }

    #[test]
    fn test_encode_project_path_with_spaces() {
        assert_eq!(
            GitLabClient::<MockHttpClient>::encode_project_path("my group/my repo"),
            "my%20group%2Fmy%20repo"
        );
    }

    #[test]
    fn test_encode_project_path_with_unicode() {
        let encoded = GitLabClient::<MockHttpClient>::encode_project_path("org/repo-\u{00e9}");
        assert!(encoded.contains("%C3%A9") || encoded.contains("repo-"));
    }

    #[tokio::test]
    async fn test_get_pr_diff_multiple_changes_order() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/20/changes",
            200,
            r#"{"changes": [
                {"old_path": "first.rs", "new_path": "first.rs", "diff": "diff1"},
                {"old_path": "second.rs", "new_path": "second.rs", "diff": "diff2"},
                {"old_path": "third.rs", "new_path": "third.rs", "diff": "diff3"}
            ]}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let diff = client.get_pr_diff("group/repo", 20).await.unwrap();

        // Verify changes appear in order
        let first_pos = diff.find("first.rs").unwrap();
        let second_pos = diff.find("second.rs").unwrap();
        let third_pos = diff.find("third.rs").unwrap();
        assert!(first_pos < second_pos);
        assert!(second_pos < third_pos);
        assert!(diff.contains("diff1"));
        assert!(diff.contains("diff2"));
        assert!(diff.contains("diff3"));
    }

    #[tokio::test]
    async fn test_get_new_review_comments_since_equal_timestamp_included() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 1,
                "body": "exact",
                "author": {"id": 1, "username": "dev"},
                "created_at": "2025-06-01T00:00:00Z",
                "updated_at": "2025-06-01T00:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {"new_path": "f.rs", "old_path": "f.rs", "new_line": 1, "old_line": null}
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let comments = client
            .get_new_review_comments("group/repo", 1, Some("2025-06-01T00:00:00Z"))
            .await
            .unwrap();
        assert_eq!(
            comments.len(),
            1,
            "comment at exact 'since' timestamp should be included"
        );
    }

    #[tokio::test]
    async fn test_get_group_issues_only_state_filter() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/issues?per_page=100&state=closed",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let issues = client
            .get_group_issues("mygroup", &[], Some("closed"))
            .await
            .unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_get_project_issues_only_labels_no_state() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues?per_page=100&labels=bug",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let labels = vec!["bug".to_string()];
        let issues = client
            .get_project_issues("group/repo", &labels, None)
            .await
            .unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_get_group_issues_invalid_json() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/issues?per_page=100",
            200,
            "not json",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_group_issues("mygroup", &[], None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_issue_invalid_json() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues/1",
            200,
            "not json",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_issue("group/repo", 1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_reviews_approval_id_is_negated_user_id() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            "[]",
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": [
                {"user": {"id": 77, "username": "approver77"}, "created_at": "2025-01-01T00:00:00Z"}
            ]}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 1).await.unwrap();
        assert_eq!(reviews.len(), 1);
        // Approval ID should be -(user.id) to avoid collision with note IDs
        assert_eq!(reviews[0].id, -77);
        assert_eq!(reviews[0].user.id, 77);
    }

    #[tokio::test]
    async fn test_get_reviews_notes_pagination_error_on_page2() {
        let mock = MockHttpClient::new();

        // Build a full page of 100 notes for page 1
        let mut page1_notes = String::from("[");
        for i in 0..100 {
            if i > 0 {
                page1_notes.push(',');
            }
            page1_notes.push_str(&format!(
                r#"{{
                    "id": {},
                    "body": "note {}",
                    "author": {{"id": 1, "username": "dev"}},
                    "created_at": "2025-01-01T00:00:00Z",
                    "updated_at": "2025-01-01T00:00:00Z",
                    "type": null,
                    "system": false,
                    "position": null
                }}"#,
                i + 1,
                i + 1
            ));
        }
        page1_notes.push(']');

        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            &page1_notes,
        );
        // Page 2 returns a server error
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=2&sort=asc",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_reviews("group/repo", 1).await;
        assert!(
            result.is_err(),
            "should propagate error from page 2 of notes"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Internal Server Error") || err_msg.contains("500"),
            "error should mention the API failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_reviews_notes_pagination_exact_page_boundary() {
        let mock = MockHttpClient::new();

        // Build a full page of 100 notes for page 1
        let mut page1_notes = String::from("[");
        for i in 0..100 {
            if i > 0 {
                page1_notes.push(',');
            }
            page1_notes.push_str(&format!(
                r#"{{
                    "id": {},
                    "body": "note {}",
                    "author": {{"id": 1, "username": "dev"}},
                    "created_at": "2025-01-01T00:00:00Z",
                    "updated_at": "2025-01-01T00:00:00Z",
                    "type": null,
                    "system": false,
                    "position": null
                }}"#,
                i + 1,
                i + 1
            ));
        }
        page1_notes.push(']');

        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            &page1_notes,
        );
        // Page 2 returns empty array (exact boundary)
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=2&sort=asc",
            200,
            "[]",
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": []}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 1).await.unwrap();
        assert_eq!(
            reviews.len(),
            100,
            "should get exactly 100 notes, page 2 empty stops pagination"
        );
    }

    #[tokio::test]
    async fn test_list_repos_pagination_error_on_page2() {
        let mock = MockHttpClient::new();

        // Build a full page of 100 projects for page 1
        let mut page1 = String::from("[");
        for i in 0..100 {
            if i > 0 {
                page1.push(',');
            }
            page1.push_str(&format!(
                r#"{{
                    "id": {},
                    "path_with_namespace": "mygroup/repo-{}",
                    "name": "repo-{}",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/mygroup/repo-{}.git",
                    "ssh_url_to_repo": "git@gitlab.com:mygroup/repo-{}.git",
                    "web_url": "https://gitlab.com/mygroup/repo-{}",
                    "visibility": "private",
                    "archived": false
                }}"#,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                i + 1
            ));
        }
        page1.push(']');

        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            &page1,
        );
        // Page 2 returns error
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=2&include_subgroups=true",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.list_repos("mygroup").await;
        assert!(
            result.is_err(),
            "should propagate error from page 2 of repos"
        );
    }

    #[tokio::test]
    async fn test_list_repos_all_archived() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            r#"[
                {
                    "id": 1,
                    "path_with_namespace": "mygroup/old1",
                    "name": "old1",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/mygroup/old1.git",
                    "ssh_url_to_repo": "git@gitlab.com:mygroup/old1.git",
                    "web_url": "https://gitlab.com/mygroup/old1",
                    "visibility": "private",
                    "archived": true
                },
                {
                    "id": 2,
                    "path_with_namespace": "mygroup/old2",
                    "name": "old2",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/mygroup/old2.git",
                    "ssh_url_to_repo": "git@gitlab.com:mygroup/old2.git",
                    "web_url": "https://gitlab.com/mygroup/old2",
                    "visibility": "private",
                    "archived": true
                }
            ]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("mygroup").await.unwrap();
        assert!(
            repos.is_empty(),
            "all archived projects should be filtered out"
        );
    }

    #[test]
    fn test_note_to_comment_new_file_no_old_path() {
        let note = GitLabNote {
            id: 600,
            body: "new file comment".to_string(),
            author: GitLabUser {
                id: 1,
                username: "user".to_string(),
            },
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            note_type: Some("DiffNote".to_string()),
            system: false,
            position: Some(GitLabNotePosition {
                new_path: Some("brand_new.rs".to_string()),
                old_path: None,
                new_line: Some(1),
                old_line: None,
            }),
        };

        let comment = GitLabClient::<MockHttpClient>::note_to_comment(&note);
        assert_eq!(comment.path, "brand_new.rs");
        assert_eq!(comment.line, Some(1));
    }

    #[test]
    fn test_note_to_comment_new_path_but_old_line() {
        let note = GitLabNote {
            id: 601,
            body: "comment on old line".to_string(),
            author: GitLabUser {
                id: 2,
                username: "reviewer".to_string(),
            },
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            note_type: Some("DiffNote".to_string()),
            system: false,
            position: Some(GitLabNotePosition {
                new_path: Some("file.rs".to_string()),
                old_path: Some("file.rs".to_string()),
                new_line: None,
                old_line: Some(25),
            }),
        };

        let comment = GitLabClient::<MockHttpClient>::note_to_comment(&note);
        assert_eq!(comment.path, "file.rs");
        // new_line is None so it should fall back to old_line
        assert_eq!(comment.line, Some(25));
    }

    #[tokio::test]
    async fn test_get_reviews_complex_mix() {
        let mock = MockHttpClient::new();

        let notes_json = r#"[
            {
                "id": 1,
                "body": "General note 1",
                "author": {"id": 10, "username": "alice"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": null,
                "system": false,
                "position": null
            },
            {
                "id": 2,
                "body": "System: assigned to bob",
                "author": {"id": 0, "username": "system"},
                "created_at": "2025-01-01T01:00:00Z",
                "updated_at": "2025-01-01T01:00:00Z",
                "type": null,
                "system": true,
                "position": null
            },
            {
                "id": 3,
                "body": "Diff comment on line 5",
                "author": {"id": 20, "username": "bob"},
                "created_at": "2025-01-02T00:00:00Z",
                "updated_at": "2025-01-02T00:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {"new_path": "src/lib.rs", "old_path": "src/lib.rs", "new_line": 5, "old_line": null}
            },
            {
                "id": 4,
                "body": "General note 2",
                "author": {"id": 30, "username": "charlie"},
                "created_at": "2025-01-03T00:00:00Z",
                "updated_at": "2025-01-03T00:00:00Z",
                "type": null,
                "system": false,
                "position": null
            },
            {
                "id": 5,
                "body": "System: merged MR",
                "author": {"id": 0, "username": "system"},
                "created_at": "2025-01-04T00:00:00Z",
                "updated_at": "2025-01-04T00:00:00Z",
                "type": null,
                "system": true,
                "position": null
            }
        ]"#;
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/2/notes?per_page=100&page=1&sort=asc",
            200,
            notes_json,
        );

        let approvals_json = r#"{"approved_by": [
            {"user": {"id": 10, "username": "alice"}, "created_at": "2025-01-05T00:00:00Z"},
            {"user": {"id": 30, "username": "charlie"}, "created_at": "2025-01-06T00:00:00Z"}
        ]}"#;
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/2/approvals",
            200,
            approvals_json,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 2).await.unwrap();

        // Expected: note 1 (general), note 4 (general), approval alice, approval charlie = 4
        // Excluded: note 2 (system), note 3 (diff), note 5 (system)
        assert_eq!(
            reviews.len(),
            4,
            "expected 2 general notes + 2 approvals = 4, got {}",
            reviews.len()
        );

        // General notes
        assert_eq!(reviews[0].id, 1);
        assert_eq!(reviews[0].state, "COMMENTED");
        assert_eq!(reviews[0].user.login, "alice");

        assert_eq!(reviews[1].id, 4);
        assert_eq!(reviews[1].state, "COMMENTED");
        assert_eq!(reviews[1].user.login, "charlie");

        // Approvals
        assert_eq!(reviews[2].id, -10);
        assert_eq!(reviews[2].state, "APPROVED");
        assert_eq!(reviews[2].user.login, "alice");

        assert_eq!(reviews[3].id, -30);
        assert_eq!(reviews[3].state, "APPROVED");
        assert_eq!(reviews[3].user.login, "charlie");
    }

    #[tokio::test]
    async fn test_get_review_comments_multiple_diff_notes() {
        let mock = MockHttpClient::new();
        let notes_json = r#"[
            {
                "id": 1,
                "body": "First diff comment",
                "author": {"id": 10, "username": "alice"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {"new_path": "a.rs", "old_path": "a.rs", "new_line": 1, "old_line": null}
            },
            {
                "id": 2,
                "body": "Second diff comment",
                "author": {"id": 20, "username": "bob"},
                "created_at": "2025-01-02T00:00:00Z",
                "updated_at": "2025-01-02T00:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {"new_path": "b.rs", "old_path": "b.rs", "new_line": 10, "old_line": 9}
            },
            {
                "id": 3,
                "body": "Third diff comment",
                "author": {"id": 30, "username": "charlie"},
                "created_at": "2025-01-03T00:00:00Z",
                "updated_at": "2025-01-03T00:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {"new_path": "c.rs", "old_path": null, "new_line": 5, "old_line": null}
            }
        ]"#;
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/3/notes?per_page=100&page=1&sort=asc",
            200,
            notes_json,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let comments = client.get_review_comments("group/repo", 3).await.unwrap();
        assert_eq!(comments.len(), 3);
        assert_eq!(comments[0].path, "a.rs");
        assert_eq!(comments[1].path, "b.rs");
        assert_eq!(comments[2].path, "c.rs");
        assert_eq!(comments[0].line, Some(1));
        assert_eq!(comments[1].line, Some(10));
        assert_eq!(comments[2].line, Some(5));
    }

    #[tokio::test]
    async fn test_get_new_reviews_with_since_excludes_approval_without_timestamp() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            "[]",
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": [
                {"user": {"id": 10, "username": "alice"}, "created_at": null},
                {"user": {"id": 20, "username": "bob"}, "created_at": "2025-06-01T00:00:00Z"}
            ]}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client
            .get_new_reviews("group/repo", 1, Some("2025-01-01T00:00:00Z"))
            .await
            .unwrap();
        // Alice's approval has no created_at, so submitted_at is None; should be excluded by default filter
        // Bob's approval is after since, so should be included
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].user.login, "bob");
    }

    #[tokio::test]
    async fn test_get_group_issues_only_labels_no_state() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/issues?per_page=100&labels=feature",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let labels = vec!["feature".to_string()];
        let issues = client
            .get_group_issues("mygroup", &labels, None)
            .await
            .unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_get_pr_diff_custom_base_url() {
        let mut config = test_config();
        config.base_url = "https://git.corp.io".to_string();

        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://git.corp.io/api/v4/projects/team%2Frepo/merge_requests/5/changes",
            200,
            r#"{"changes": [{"old_path": "x.rs", "new_path": "x.rs", "diff": "+new line"}]}"#,
        );

        let client = GitLabClient::with_http_client(config, mock);
        let diff = client.get_pr_diff("team/repo", 5).await.unwrap();
        assert!(diff.contains("--- a/x.rs"));
        assert!(diff.contains("+++ b/x.rs"));
        assert!(diff.contains("+new line"));
    }

    #[tokio::test]
    async fn test_get_reviews_custom_base_url() {
        let mut config = test_config();
        config.base_url = "https://git.corp.io".to_string();

        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://git.corp.io/api/v4/projects/team%2Frepo/merge_requests/5/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 1,
                "body": "LGTM",
                "author": {"id": 1, "username": "dev"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": null,
                "system": false,
                "position": null
            }]"#,
        );
        mock.mock_response(
            "https://git.corp.io/api/v4/projects/team%2Frepo/merge_requests/5/approvals",
            200,
            r#"{"approved_by": []}"#,
        );

        let client = GitLabClient::with_http_client(config, mock);
        let reviews = client.get_reviews("team/repo", 5).await.unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].body.as_deref(), Some("LGTM"));
    }

    #[tokio::test]
    async fn test_get_review_comments_custom_base_url() {
        let mut config = test_config();
        config.base_url = "https://git.corp.io".to_string();

        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://git.corp.io/api/v4/projects/team%2Frepo/merge_requests/5/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 1,
                "body": "inline note",
                "author": {"id": 1, "username": "dev"},
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z",
                "type": "DiffNote",
                "system": false,
                "position": {"new_path": "f.rs", "old_path": "f.rs", "new_line": 10, "old_line": null}
            }]"#,
        );

        let client = GitLabClient::with_http_client(config, mock);
        let comments = client.get_review_comments("team/repo", 5).await.unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "f.rs");
    }

    #[tokio::test]
    async fn test_list_repos_pagination_with_archived_filter() {
        let mock = MockHttpClient::new();

        // Build a full page of 100 projects, half archived
        let mut page1 = String::from("[");
        for i in 0..100 {
            if i > 0 {
                page1.push(',');
            }
            let archived = i % 2 == 0; // even indices are archived
            page1.push_str(&format!(
                r#"{{
                    "id": {},
                    "path_with_namespace": "mygroup/repo-{}",
                    "name": "repo-{}",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/mygroup/repo-{}.git",
                    "ssh_url_to_repo": "git@gitlab.com:mygroup/repo-{}.git",
                    "web_url": "https://gitlab.com/mygroup/repo-{}",
                    "visibility": "private",
                    "archived": {}
                }}"#,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                archived
            ));
        }
        page1.push(']');

        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            &page1,
        );
        // Page 2: 2 non-archived projects
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=2&include_subgroups=true",
            200,
            r#"[
                {
                    "id": 201,
                    "path_with_namespace": "mygroup/repo-201",
                    "name": "repo-201",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/mygroup/repo-201.git",
                    "ssh_url_to_repo": "git@gitlab.com:mygroup/repo-201.git",
                    "web_url": "https://gitlab.com/mygroup/repo-201",
                    "visibility": "private",
                    "archived": false
                },
                {
                    "id": 202,
                    "path_with_namespace": "mygroup/repo-202",
                    "name": "repo-202",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/mygroup/repo-202.git",
                    "ssh_url_to_repo": "git@gitlab.com:mygroup/repo-202.git",
                    "web_url": "https://gitlab.com/mygroup/repo-202",
                    "visibility": "private",
                    "archived": false
                }
            ]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("mygroup").await.unwrap();
        // Page 1: 50 non-archived (odd indices 1,3,5,...99) + page 2: 2 = 52
        assert_eq!(
            repos.len(),
            52,
            "should have 50 non-archived from page 1 + 2 from page 2"
        );
    }

    #[tokio::test]
    async fn test_get_mr_approvals_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        // get_mr_approvals is private but we can test it indirectly -- since
        // get_reviews calls get_mr_notes first (which also fails on no token),
        // we verify the error propagates from the notes call.
        let result = client.get_reviews("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("token"));
    }

    #[tokio::test]
    async fn test_get_reviews_approvals_404_gracefully_handled() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            "[]",
        );
        // Approvals endpoint returns 404 (e.g. feature not enabled)
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            404,
            "Not found",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client.get_reviews("group/repo", 1).await.unwrap();
        // Should gracefully handle the failure with empty approvals
        assert!(
            reviews.is_empty(),
            "should return empty reviews when both notes empty and approvals fail"
        );
    }

    #[tokio::test]
    async fn test_get_project_issues_multiple() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues?per_page=100",
            200,
            r#"[
                {
                    "iid": 1,
                    "title": "Issue 1",
                    "description": "Desc 1",
                    "state": "opened",
                    "web_url": "https://gitlab.com/group/repo/-/issues/1",
                    "labels": ["bug"],
                    "author": {"id": 1, "username": "alice"},
                    "assignees": []
                },
                {
                    "iid": 2,
                    "title": "Issue 2",
                    "description": null,
                    "state": "closed",
                    "web_url": "https://gitlab.com/group/repo/-/issues/2",
                    "labels": [],
                    "author": null,
                    "assignees": [{"id": 2, "username": "bob"}]
                },
                {
                    "iid": 3,
                    "title": "Issue 3",
                    "description": "Desc 3",
                    "state": "opened",
                    "web_url": "https://gitlab.com/group/repo/-/issues/3",
                    "labels": ["bug", "critical"],
                    "author": {"id": 3, "username": "charlie"},
                    "assignees": [{"id": 1, "username": "alice"}, {"id": 2, "username": "bob"}]
                }
            ]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let issues = client
            .get_project_issues("group/repo", &[], None)
            .await
            .unwrap();
        assert_eq!(issues.len(), 3);
        assert_eq!(issues[0].iid, 1);
        assert_eq!(issues[1].iid, 2);
        assert_eq!(issues[2].iid, 3);
        assert!(issues[1].description.is_none());
        assert!(issues[1].author.is_none());
        assert_eq!(issues[2].assignees.len(), 2);
        assert_eq!(issues[2].labels, vec!["bug", "critical"]);
    }

    #[tokio::test]
    async fn test_get_group_issues_multiple() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/issues?per_page=100",
            200,
            r#"[
                {
                    "iid": 10,
                    "title": "Group issue A",
                    "description": "Desc A",
                    "state": "opened",
                    "web_url": "https://gitlab.com/mygroup/repo1/-/issues/10",
                    "labels": ["enhancement"],
                    "author": {"id": 1, "username": "user1"},
                    "assignees": []
                },
                {
                    "iid": 20,
                    "title": "Group issue B",
                    "description": "Desc B",
                    "state": "closed",
                    "web_url": "https://gitlab.com/mygroup/repo2/-/issues/20",
                    "labels": [],
                    "author": {"id": 2, "username": "user2"},
                    "assignees": [{"id": 3, "username": "user3"}]
                }
            ]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let issues = client.get_group_issues("mygroup", &[], None).await.unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].title, "Group issue A");
        assert_eq!(issues[1].title, "Group issue B");
        assert_eq!(issues[0].state, "opened");
        assert_eq!(issues[1].state, "closed");
    }

    #[tokio::test]
    async fn test_get_issue_full_field_mapping() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues/10",
            200,
            r#"{
                "iid": 10,
                "title": "Detailed issue",
                "description": "Full description with details",
                "state": "closed",
                "web_url": "https://gitlab.com/group/repo/-/issues/10",
                "labels": ["bug", "P1", "backend"],
                "author": {"id": 42, "username": "reporter"},
                "assignees": [
                    {"id": 100, "username": "dev1"},
                    {"id": 200, "username": "dev2"}
                ]
            }"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let issue = client.get_issue("group/repo", 10).await.unwrap();
        assert_eq!(issue.iid, 10);
        assert_eq!(issue.title, "Detailed issue");
        assert_eq!(
            issue.description.as_deref(),
            Some("Full description with details")
        );
        assert_eq!(issue.state, "closed");
        assert_eq!(issue.web_url, "https://gitlab.com/group/repo/-/issues/10");
        assert_eq!(issue.labels, vec!["bug", "P1", "backend"]);
        assert_eq!(issue.author.as_ref().unwrap().id, 42);
        assert_eq!(issue.author.as_ref().unwrap().username, "reporter");
        assert_eq!(issue.assignees.len(), 2);
        assert_eq!(issue.assignees[0].id, 100);
        assert_eq!(issue.assignees[0].username, "dev1");
        assert_eq!(issue.assignees[1].id, 200);
        assert_eq!(issue.assignees[1].username, "dev2");
    }

    #[test]
    fn test_note_to_review_verifies_all_default_fields() {
        let note = GitLabNote {
            id: 999,
            body: "".to_string(),
            author: GitLabUser {
                id: 0,
                username: "empty_user".to_string(),
            },
            created_at: "".to_string(),
            updated_at: "some_date".to_string(),
            note_type: Some("SomeType".to_string()),
            system: true,
            position: None,
        };

        let review = GitLabClient::<MockHttpClient>::note_to_review(&note);
        // note_to_review always maps state to "COMMENTED"
        assert_eq!(review.state, "COMMENTED");
        // body is set from note.body even if empty
        assert_eq!(review.body.as_deref(), Some(""));
        // submitted_at uses created_at, not updated_at
        assert_eq!(review.submitted_at.as_deref(), Some(""));
        // html_url is always None
        assert!(review.html_url.is_none());
        // user_type is always None
        assert!(review.user.user_type.is_none());
    }

    #[tokio::test]
    async fn test_list_repos_pagination_exact_boundary() {
        let mock = MockHttpClient::new();

        // Build exactly 100 projects for page 1
        let mut page1 = String::from("[");
        for i in 0..100 {
            if i > 0 {
                page1.push(',');
            }
            page1.push_str(&format!(
                r#"{{
                    "id": {},
                    "path_with_namespace": "mygroup/repo-{}",
                    "name": "repo-{}",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/mygroup/repo-{}.git",
                    "ssh_url_to_repo": "git@gitlab.com:mygroup/repo-{}.git",
                    "web_url": "https://gitlab.com/mygroup/repo-{}",
                    "visibility": "private",
                    "archived": false
                }}"#,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                i + 1
            ));
        }
        page1.push(']');

        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            &page1,
        );
        // Page 2 returns empty
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=2&include_subgroups=true",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("mygroup").await.unwrap();
        assert_eq!(
            repos.len(),
            100,
            "should get exactly 100 repos; page 2 empty stops pagination"
        );
    }

    #[tokio::test]
    async fn test_scm_provider_get_pr_status_via_trait() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1, "state": "opened"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        // Call through the trait reference
        let provider: &dyn ScmProvider = &client;
        let status = provider.get_pr_status("group/repo", 1).await.unwrap();
        assert_eq!(status, PrStatus::Open);
    }

    #[tokio::test]
    async fn test_scm_provider_get_pr_info_via_trait() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1, "state": "opened", "source_branch": "feat", "target_branch": "main", "title": "Feat", "author": {"id": 1, "username": "dev"}}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let provider: &dyn ScmProvider = &client;
        let info = provider.get_pr_info("group/repo", 1).await.unwrap();
        assert_eq!(info.head_branch.as_deref(), Some("feat"));
        assert_eq!(info.title.as_deref(), Some("Feat"));
    }

    #[test]
    fn test_is_enabled_requires_both_enabled_and_token() {
        // enabled=false, token=Some => false
        let mut config = test_config();
        config.enabled = false;
        config.token = Some("tk".into());
        let client = GitLabClient::new(config);
        assert!(!client.is_enabled());

        // enabled=true, token=None => false
        let mut config2 = test_config();
        config2.enabled = true;
        config2.token = None;
        let client2 = GitLabClient::new(config2);
        assert!(!client2.is_enabled());

        // enabled=false, token=None => false
        let mut config3 = test_config();
        config3.enabled = false;
        config3.token = None;
        let client3 = GitLabClient::new(config3);
        assert!(!client3.is_enabled());

        // enabled=true, token=Some => true
        let config4 = test_config();
        let client4 = GitLabClient::new(config4);
        assert!(client4.is_enabled());
    }

    #[tokio::test]
    async fn test_get_new_review_comments_uses_updated_at_for_filter() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[
                {
                    "id": 1,
                    "body": "old created but recently updated",
                    "author": {"id": 1, "username": "dev"},
                    "created_at": "2024-01-01T00:00:00Z",
                    "updated_at": "2025-09-01T00:00:00Z",
                    "type": "DiffNote",
                    "system": false,
                    "position": {"new_path": "f.rs", "old_path": "f.rs", "new_line": 1, "old_line": null}
                },
                {
                    "id": 2,
                    "body": "recent created but old updated",
                    "author": {"id": 2, "username": "dev2"},
                    "created_at": "2025-08-01T00:00:00Z",
                    "updated_at": "2025-01-01T00:00:00Z",
                    "type": "DiffNote",
                    "system": false,
                    "position": {"new_path": "g.rs", "old_path": "g.rs", "new_line": 5, "old_line": null}
                }
            ]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let comments = client
            .get_new_review_comments("group/repo", 1, Some("2025-06-01T00:00:00Z"))
            .await
            .unwrap();
        // Only comment 1 should pass: updated_at "2025-09-01" >= since "2025-06-01"
        // Comment 2 has updated_at "2025-01-01" < since "2025-06-01"
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, 1);
    }

    #[tokio::test]
    async fn test_get_pr_status_403_forbidden() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            403,
            r#"{"message":"403 Forbidden"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_status("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Forbidden") || err_msg.contains("API error"),
            "error should indicate forbidden: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_pr_info_404() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/999",
            404,
            "Not found",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_info("group/repo", 999).await;
        // get_pr_info does not have special 404 handling like get_pr_status
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Not found") || err_msg.contains("API error"),
            "error should indicate not found: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_project_issues_only_state_no_labels() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues?per_page=100&state=closed",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let issues = client
            .get_project_issues("group/repo", &[], Some("closed"))
            .await
            .unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn test_get_pr_diff_many_changes() {
        let mock = MockHttpClient::new();

        let mut changes = String::from(r#"{"changes": ["#);
        for i in 0..50 {
            if i > 0 {
                changes.push(',');
            }
            changes.push_str(&format!(
                r#"{{"old_path": "file{}.rs", "new_path": "file{}.rs", "diff": "@@ -1 +1 @@\n-old{}\n+new{}\n"}}"#,
                i, i, i, i
            ));
        }
        changes.push_str("]}");

        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/50/changes",
            200,
            &changes,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let diff = client.get_pr_diff("group/repo", 50).await.unwrap();

        // Verify first and last changes are present
        assert!(diff.contains("--- a/file0.rs"));
        assert!(diff.contains("+++ b/file0.rs"));
        assert!(diff.contains("--- a/file49.rs"));
        assert!(diff.contains("+++ b/file49.rs"));
        assert!(diff.contains("+new0"));
        assert!(diff.contains("+new49"));
    }

    #[test]
    fn test_deserialize_gitlab_note_position_all_null() {
        let json = r#"{
            "new_path": null,
            "old_path": null,
            "new_line": null,
            "old_line": null
        }"#;
        let pos: GitLabNotePosition = serde_json::from_str(json).unwrap();
        assert!(pos.new_path.is_none());
        assert!(pos.old_path.is_none());
        assert!(pos.new_line.is_none());
        assert!(pos.old_line.is_none());
    }

    #[test]
    fn test_deserialize_gitlab_note_position_all_set() {
        let json = r#"{
            "new_path": "new.rs",
            "old_path": "old.rs",
            "new_line": 100,
            "old_line": 50
        }"#;
        let pos: GitLabNotePosition = serde_json::from_str(json).unwrap();
        assert_eq!(pos.new_path.as_deref(), Some("new.rs"));
        assert_eq!(pos.old_path.as_deref(), Some("old.rs"));
        assert_eq!(pos.new_line, Some(100));
        assert_eq!(pos.old_line, Some(50));
    }

    #[tokio::test]
    async fn test_list_repos_invalid_json_on_page2() {
        let mock = MockHttpClient::new();

        // Build a full page of 100 projects for page 1
        let mut page1 = String::from("[");
        for i in 0..100 {
            if i > 0 {
                page1.push(',');
            }
            page1.push_str(&format!(
                r#"{{
                    "id": {},
                    "path_with_namespace": "mygroup/repo-{}",
                    "name": "repo-{}",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/mygroup/repo-{}.git",
                    "ssh_url_to_repo": "git@gitlab.com:mygroup/repo-{}.git",
                    "web_url": "https://gitlab.com/mygroup/repo-{}",
                    "visibility": "private",
                    "archived": false
                }}"#,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                i + 1
            ));
        }
        page1.push(']');

        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=1&include_subgroups=true",
            200,
            &page1,
        );
        // Page 2 returns invalid JSON
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/mygroup/projects?per_page=100&page=2&include_subgroups=true",
            200,
            "not json",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.list_repos("mygroup").await;
        assert!(
            result.is_err(),
            "should fail when page 2 returns invalid JSON"
        );
    }

    #[tokio::test]
    async fn test_get_reviews_notes_invalid_json_on_page2() {
        let mock = MockHttpClient::new();

        // Build a full page of 100 notes for page 1
        let mut page1_notes = String::from("[");
        for i in 0..100 {
            if i > 0 {
                page1_notes.push(',');
            }
            page1_notes.push_str(&format!(
                r#"{{
                    "id": {},
                    "body": "note {}",
                    "author": {{"id": 1, "username": "dev"}},
                    "created_at": "2025-01-01T00:00:00Z",
                    "updated_at": "2025-01-01T00:00:00Z",
                    "type": null,
                    "system": false,
                    "position": null
                }}"#,
                i + 1,
                i + 1
            ));
        }
        page1_notes.push(']');

        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            &page1_notes,
        );
        // Page 2 returns invalid JSON
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=2&sort=asc",
            200,
            "not valid json",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_reviews("group/repo", 1).await;
        assert!(
            result.is_err(),
            "should fail when page 2 of notes returns invalid JSON"
        );
    }

    #[tokio::test]
    async fn test_get_new_reviews_only_approval_after_since() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes?per_page=100&page=1&sort=asc",
            200,
            r#"[{
                "id": 1,
                "body": "old comment",
                "author": {"id": 1, "username": "dev"},
                "created_at": "2024-01-01T00:00:00Z",
                "updated_at": "2024-01-01T00:00:00Z",
                "type": null,
                "system": false,
                "position": null
            }]"#,
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approvals",
            200,
            r#"{"approved_by": [
                {"user": {"id": 10, "username": "alice"}, "created_at": "2025-09-01T00:00:00Z"}
            ]}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let reviews = client
            .get_new_reviews("group/repo", 1, Some("2025-06-01T00:00:00Z"))
            .await
            .unwrap();
        // The general note has submitted_at (created_at) "2024-01-01" < since "2025-06-01"
        // The approval has submitted_at (created_at) "2025-09-01" >= since "2025-06-01"
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].state, "APPROVED");
        assert_eq!(reviews[0].user.login, "alice");
    }

    #[tokio::test]
    async fn test_get_review_comments_pagination() {
        let mock = MockHttpClient::new();

        // Build a full page of 100 diff notes for page 1
        let mut page1_notes = String::from("[");
        for i in 0..100 {
            if i > 0 {
                page1_notes.push(',');
            }
            page1_notes.push_str(&format!(
                r#"{{
                    "id": {},
                    "body": "diff note {}",
                    "author": {{"id": 1, "username": "dev"}},
                    "created_at": "2025-01-01T00:00:00Z",
                    "updated_at": "2025-01-01T00:00:00Z",
                    "type": "DiffNote",
                    "system": false,
                    "position": {{"new_path": "file{}.rs", "old_path": "file{}.rs", "new_line": {}, "old_line": null}}
                }}"#,
                i + 1,
                i + 1,
                i + 1,
                i + 1,
                i + 1
            ));
        }
        page1_notes.push(']');

        // Page 2 has 1 diff note
        let page2_notes = r#"[{
            "id": 101,
            "body": "last diff note",
            "author": {"id": 1, "username": "dev"},
            "created_at": "2025-01-01T00:00:00Z",
            "updated_at": "2025-01-01T00:00:00Z",
            "type": "DiffNote",
            "system": false,
            "position": {"new_path": "last.rs", "old_path": "last.rs", "new_line": 1, "old_line": null}
        }]"#;

        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/7/notes?per_page=100&page=1&sort=asc",
            200,
            &page1_notes,
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/7/notes?per_page=100&page=2&sort=asc",
            200,
            page2_notes,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let comments = client.get_review_comments("group/repo", 7).await.unwrap();
        assert_eq!(
            comments.len(),
            101,
            "should get 100 from page 1 + 1 from page 2"
        );
        assert_eq!(comments[100].body, "last diff note");
    }

    #[test]
    fn test_encode_project_path_simple() {
        // Simple path with no slashes -- should remain unchanged
        assert_eq!(
            GitLabClient::<MockHttpClient>::encode_project_path("my-project"),
            "my-project"
        );
    }

    #[test]
    fn test_encode_project_path_nested() {
        // Nested path with multiple slashes
        assert_eq!(
            GitLabClient::<MockHttpClient>::encode_project_path("org/subgroup/project"),
            "org%2Fsubgroup%2Fproject"
        );
    }

    #[test]
    fn test_encode_project_path_special_chars() {
        // Path with special characters: @, +, ~
        let encoded = GitLabClient::<MockHttpClient>::encode_project_path("org/my+project@v2~beta");
        assert!(encoded.contains("org%2F"));
        // + and @ and ~ should be percent-encoded
        assert!(
            encoded.contains("%40") || encoded.contains("%2B") || encoded.contains("%7E"),
            "special chars should be encoded: {encoded}"
        );
    }

    #[test]
    fn test_note_to_review_basic() {
        let note = GitLabNote {
            id: 42,
            body: "Looks good to me".to_string(),
            author: GitLabUser {
                id: 7,
                username: "tester".to_string(),
            },
            created_at: "2025-06-15T12:00:00Z".to_string(),
            updated_at: "2025-06-15T13:00:00Z".to_string(),
            note_type: None,
            system: false,
            position: None,
        };

        let review = GitLabClient::<MockHttpClient>::note_to_review(&note);
        assert_eq!(review.id, 42);
        assert_eq!(review.state, "COMMENTED");
        assert_eq!(review.body.as_deref(), Some("Looks good to me"));
        assert_eq!(review.user.id, 7);
        assert_eq!(review.user.login, "tester");
        assert!(review.user.user_type.is_none());
        assert_eq!(review.submitted_at.as_deref(), Some("2025-06-15T12:00:00Z"));
        assert!(review.html_url.is_none());
    }

    #[test]
    fn test_note_to_comment_with_position_fields() {
        let note = GitLabNote {
            id: 55,
            body: "Please fix this".to_string(),
            author: GitLabUser {
                id: 3,
                username: "reviewer_x".to_string(),
            },
            created_at: "2025-04-01T00:00:00Z".to_string(),
            updated_at: "2025-04-01T01:00:00Z".to_string(),
            note_type: Some("DiffNote".to_string()),
            system: false,
            position: Some(GitLabNotePosition {
                new_path: Some("src/app.rs".to_string()),
                old_path: Some("src/app.rs".to_string()),
                new_line: Some(77),
                old_line: Some(75),
            }),
        };

        let comment = GitLabClient::<MockHttpClient>::note_to_comment(&note);
        assert_eq!(comment.id, 55);
        assert_eq!(comment.body, "Please fix this");
        assert_eq!(comment.path, "src/app.rs");
        assert_eq!(comment.line, Some(77)); // new_line preferred over old_line
        assert!(comment.start_line.is_none());
        assert_eq!(comment.user.id, 3);
        assert_eq!(comment.user.login, "reviewer_x");
        assert_eq!(comment.created_at, "2025-04-01T00:00:00Z");
        assert_eq!(comment.updated_at, "2025-04-01T01:00:00Z");
        assert!(comment.html_url.is_empty());
        assert!(comment.position.is_none());
        assert!(comment.original_position.is_none());
        assert!(comment.pull_request_review_id.is_none());
        assert!(comment.side.is_none());
    }

    #[test]
    fn test_note_to_comment_no_position_maps_empty() {
        let note = GitLabNote {
            id: 66,
            body: "general note".to_string(),
            author: GitLabUser {
                id: 9,
                username: "commenter".to_string(),
            },
            created_at: "2025-05-01T00:00:00Z".to_string(),
            updated_at: "2025-05-01T00:00:00Z".to_string(),
            note_type: None,
            system: false,
            position: None,
        };

        let comment = GitLabClient::<MockHttpClient>::note_to_comment(&note);
        assert_eq!(comment.id, 66);
        assert!(
            comment.path.is_empty(),
            "path should be empty when no position"
        );
        assert!(
            comment.line.is_none(),
            "line should be None when no position"
        );
        assert!(comment.start_line.is_none());
    }

    #[test]
    fn test_build_headers_returns_correct_headers() {
        let config = test_config();
        let client =
            GitLabClient::<MockHttpClient>::with_http_client(config, MockHttpClient::new());
        let headers = client.build_headers("gl-pat-12345");
        assert_eq!(headers.len(), 4);

        let private_token = headers
            .iter()
            .find(|(k, _)| *k == "PRIVATE-TOKEN")
            .expect("should contain PRIVATE-TOKEN");
        assert_eq!(private_token.1, "gl-pat-12345");

        let accept = headers
            .iter()
            .find(|(k, _)| *k == "Accept")
            .expect("should contain Accept");
        assert_eq!(accept.1, "application/json");

        let ua = headers
            .iter()
            .find(|(k, _)| *k == "User-Agent")
            .expect("should contain User-Agent");
        assert_eq!(ua.1, "claudear");
    }

    #[tokio::test]
    async fn test_get_project_issues_with_labels() {
        let mock = MockHttpClient::new();
        // Labels only, no state filter
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues?per_page=100&labels=feature-request,enhancement",
            200,
            r#"[{
                "iid": 10,
                "title": "Labeled issue",
                "description": null,
                "state": "opened",
                "web_url": "https://gitlab.com/group/repo/-/issues/10",
                "labels": ["feature-request", "enhancement"],
                "author": null,
                "assignees": []
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let labels = vec!["feature-request".to_string(), "enhancement".to_string()];
        let issues = client
            .get_project_issues("group/repo", &labels, None)
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].iid, 10);
        assert_eq!(issues[0].labels, vec!["feature-request", "enhancement"]);
    }

    #[tokio::test]
    async fn test_get_project_issues_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/issues?per_page=100",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_project_issues("group/repo", &[], None).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("500") || err_msg.contains("Internal Server Error"),
            "expected error message to contain status info: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_pr_diff_assembles_unified_diff() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/org%2Frepo/merge_requests/99/changes",
            200,
            r#"{"changes": [
                {
                    "old_path": "lib.rs",
                    "new_path": "lib.rs",
                    "diff": "@@ -10,3 +10,4 @@\n fn hello() {\n+    println!(\"world\");\n }\n"
                }
            ]}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let diff = client.get_pr_diff("org/repo", 99).await.unwrap();
        assert!(diff.contains("--- a/lib.rs"));
        assert!(diff.contains("+++ b/lib.rs"));
        assert!(diff.contains("println!"));
    }

    #[tokio::test]
    async fn test_get_pr_info_full_fields() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/org%2Frepo/merge_requests/7",
            200,
            r#"{
                "iid": 7,
                "state": "opened",
                "source_branch": "feat/new-api",
                "target_branch": "develop",
                "title": "Implement new API endpoints",
                "author": {"id": 42, "username": "engineer"}
            }"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let info = client.get_pr_info("org/repo", 7).await.unwrap();
        assert_eq!(info.head_branch, Some("feat/new-api".to_string()));
        assert_eq!(info.base_branch, Some("develop".to_string()));
        assert_eq!(info.title, Some("Implement new API endpoints".to_string()));
        assert_eq!(info.author, Some("engineer".to_string()));
    }

    #[tokio::test]
    async fn test_list_repos_success_with_field_mapping() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/test-org/projects?per_page=100&page=1&include_subgroups=true",
            200,
            r#"[
                {
                    "id": 10,
                    "path_with_namespace": "test-org/service-a",
                    "name": "service-a",
                    "default_branch": "main",
                    "http_url_to_repo": "https://gitlab.com/test-org/service-a.git",
                    "ssh_url_to_repo": "git@gitlab.com:test-org/service-a.git",
                    "web_url": "https://gitlab.com/test-org/service-a",
                    "visibility": "private",
                    "archived": false
                },
                {
                    "id": 11,
                    "path_with_namespace": "test-org/service-b",
                    "name": "service-b",
                    "default_branch": "develop",
                    "http_url_to_repo": "https://gitlab.com/test-org/service-b.git",
                    "ssh_url_to_repo": "git@gitlab.com:test-org/service-b.git",
                    "web_url": "https://gitlab.com/test-org/service-b",
                    "visibility": "public",
                    "archived": false
                }
            ]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let repos = client.list_repos("test-org").await.unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].id, 10);
        assert_eq!(repos[0].full_name, "test-org/service-a");
        assert_eq!(repos[0].name, "service-a");
        assert!(repos[0].private);
        assert_eq!(repos[1].id, 11);
        assert!(
            !repos[1].private,
            "public visibility should be private=false"
        );
        assert_eq!(repos[1].default_branch, "develop");
    }

    #[tokio::test]
    async fn test_list_repos_not_found() {
        let mock = MockHttpClient::new();
        // Group returns 404
        mock.mock_response(
            "https://gitlab.com/api/v4/groups/does-not-exist/projects?per_page=100&page=1&include_subgroups=true",
            404,
            "Group not found",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.list_repos("does-not-exist").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("not found"),
            "error should mention not found: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_merge_pr_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/merge",
            200,
            r#"{"iid": 1, "state": "merged"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.merge_pr("group/repo", 1).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_merge_pr_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/merge",
            405,
            "Method Not Allowed",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.merge_pr("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to merge"),
            "error should mention merge failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_merge_pr_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.merge_pr("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_merge_pr_conflict() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/5/merge",
            422,
            r#"{"message": "Branch cannot be merged"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.merge_pr("group/repo", 5).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to merge"),
            "error should mention merge failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_close_pr_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            r#"{"iid": 1, "state": "closed"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.close_pr("group/repo", 1).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_close_pr_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.close_pr("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to close"),
            "error should mention close failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_close_pr_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.close_pr("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_delete_branch_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/repository/branches/feature%2Fmy-branch",
            204,
            "",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client
            .delete_branch("group/repo", "feature/my-branch")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_delete_branch_not_found_is_ok() {
        let mock = MockHttpClient::new();
        // Branch already deleted - 404 should be treated as success
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/repository/branches/gone",
            404,
            "Branch not found",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.delete_branch("group/repo", "gone").await;
        assert!(
            result.is_ok(),
            "404 on delete_branch should be treated as success"
        );
    }

    #[tokio::test]
    async fn test_delete_branch_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/repository/branches/protected",
            403,
            "Branch is protected",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.delete_branch("group/repo", "protected").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to delete branch"),
            "error should mention delete failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_delete_branch_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.delete_branch("group/repo", "branch").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_post_review_comment_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes",
            201,
            r#"{"id": 999, "body": "Looks good"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client
            .post_review("group/repo", 1, PostReviewAction::Comment, "Looks good")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_post_review_comment_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes",
            403,
            "Forbidden",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client
            .post_review("group/repo", 1, PostReviewAction::Comment, "text")
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to post comment"),
            "error should mention comment failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_post_review_approve_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approve",
            201,
            r#"{"id": 1}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client
            .post_review("group/repo", 1, PostReviewAction::Approve, "LGTM")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_post_review_approve_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/approve",
            401,
            "Unauthorized",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client
            .post_review("group/repo", 1, PostReviewAction::Approve, "LGTM")
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to approve"),
            "error should mention approval failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_post_review_request_changes_success() {
        let mock = MockHttpClient::new();
        // Unapprove endpoint (may fail, errors ignored)
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/unapprove",
            200,
            "{}",
        );
        // Note endpoint for changes-requested comment
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes",
            201,
            r#"{"id": 1, "body": "Please fix"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client
            .post_review(
                "group/repo",
                1,
                PostReviewAction::RequestChanges,
                "Please fix",
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_post_review_request_changes_unapprove_fails_still_ok() {
        let mock = MockHttpClient::new();
        // Unapprove returns error (not previously approved) -- this should be ignored
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/unapprove",
            403,
            "Cannot unapprove",
        );
        // Note endpoint succeeds
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes",
            201,
            r#"{"id": 2}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client
            .post_review(
                "group/repo",
                1,
                PostReviewAction::RequestChanges,
                "Needs work",
            )
            .await;
        assert!(
            result.is_ok(),
            "unapprove failure should not cause overall failure"
        );
    }

    #[tokio::test]
    async fn test_post_review_request_changes_note_fails() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/unapprove",
            200,
            "{}",
        );
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/notes",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client
            .post_review(
                "group/repo",
                1,
                PostReviewAction::RequestChanges,
                "Fix this",
            )
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to post changes-requested note"),
            "error should mention changes-requested failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_post_review_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client
            .post_review("group/repo", 1, PostReviewAction::Comment, "hi")
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_list_open_prs_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests?state=opened&per_page=100",
            200,
            r#"[
                {"iid": 1, "title": "Feature A", "source_branch": "feature-a", "web_url": "https://gitlab.com/group/repo/-/merge_requests/1"},
                {"iid": 2, "title": "Fix B", "source_branch": "fix-b", "web_url": "https://gitlab.com/group/repo/-/merge_requests/2"}
            ]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let prs = client.list_open_prs("group/repo").await.unwrap();
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 1);
        assert_eq!(prs[0].title, "Feature A");
        assert_eq!(prs[0].branch, "feature-a");
        assert!(prs[0].url.contains("merge_requests/1"));
        assert_eq!(prs[1].number, 2);
        assert_eq!(prs[1].title, "Fix B");
    }

    #[tokio::test]
    async fn test_list_open_prs_empty() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests?state=opened&per_page=100",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let prs = client.list_open_prs("group/repo").await.unwrap();
        assert!(prs.is_empty());
    }

    #[tokio::test]
    async fn test_list_open_prs_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests?state=opened&per_page=100",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.list_open_prs("group/repo").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to list open MRs"),
            "error should mention list failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_list_open_prs_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.list_open_prs("group/repo").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_pr_branch_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/3",
            200,
            r#"{"iid": 3, "state": "opened", "source_branch": "my-feature", "target_branch": "main"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let branch = client.get_pr_branch("group/repo", 3).await.unwrap();
        assert_eq!(branch, "my-feature");
    }

    #[tokio::test]
    async fn test_get_pr_branch_no_source_branch() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/4",
            200,
            r#"{"iid": 4, "state": "opened"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_pr_branch("group/repo", 4).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("No source branch"),
            "error should mention missing branch: {err_msg}"
        );
    }

    #[test]
    fn test_pr_url_pattern() {
        let client = GitLabClient::new(test_config());
        assert_eq!(ScmProvider::pr_url_pattern(&client), "%-/merge_requests/%");
    }

    #[test]
    fn test_parse_pr_number_valid() {
        let client = GitLabClient::new(test_config());
        assert_eq!(
            ScmProvider::parse_pr_number(
                &client,
                "https://gitlab.com/group/repo/-/merge_requests/42"
            ),
            Some(42)
        );
    }

    #[test]
    fn test_parse_pr_number_nested_path() {
        let client = GitLabClient::new(test_config());
        assert_eq!(
            ScmProvider::parse_pr_number(
                &client,
                "https://gitlab.com/org/team/repo/-/merge_requests/123"
            ),
            Some(123)
        );
    }

    #[test]
    fn test_parse_pr_number_invalid_url() {
        let client = GitLabClient::new(test_config());
        assert_eq!(
            ScmProvider::parse_pr_number(&client, "https://gitlab.com/group/repo/-/issues/5"),
            None
        );
    }

    #[test]
    fn test_parse_pr_number_no_number() {
        let client = GitLabClient::new(test_config());
        assert_eq!(
            ScmProvider::parse_pr_number(
                &client,
                "https://gitlab.com/group/repo/-/merge_requests/"
            ),
            None
        );
    }

    #[tokio::test]
    async fn test_get_latest_release_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/releases?per_page=1&order_by=released_at&sort=desc",
            200,
            r#"[{
                "tag_name": "v1.0.0",
                "name": "Release 1.0.0",
                "_links": {"self": "https://gitlab.com/group/repo/-/releases/v1.0.0"},
                "released_at": "2025-01-01T00:00:00Z"
            }]"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let release = client.get_latest_release("group/repo").await.unwrap();
        assert!(release.is_some());
        let r = release.unwrap();
        assert_eq!(r.tag, "v1.0.0");
        assert_eq!(r.name.as_deref(), Some("Release 1.0.0"));
        assert_eq!(r.url, "https://gitlab.com/group/repo/-/releases/v1.0.0");
        assert_eq!(r.published_at.as_deref(), Some("2025-01-01T00:00:00Z"));
    }

    #[tokio::test]
    async fn test_get_latest_release_no_releases() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/releases?per_page=1&order_by=released_at&sort=desc",
            200,
            "[]",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let release = client.get_latest_release("group/repo").await.unwrap();
        assert!(release.is_none());
    }

    #[tokio::test]
    async fn test_get_latest_release_404() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/releases?per_page=1&order_by=released_at&sort=desc",
            404,
            "Not Found",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let release = client.get_latest_release("group/repo").await.unwrap();
        assert!(release.is_none());
    }

    #[tokio::test]
    async fn test_get_latest_release_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/releases?per_page=1&order_by=released_at&sort=desc",
            500,
            "Internal Server Error",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.get_latest_release("group/repo").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to get releases"),
            "error should mention releases failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_latest_release_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.get_latest_release("group/repo").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_create_release_success() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/releases",
            201,
            r#"{
                "tag_name": "v2.0.0",
                "name": "Version 2",
                "_links": {"self": "https://gitlab.com/group/repo/-/releases/v2.0.0"},
                "released_at": "2025-06-01T00:00:00Z"
            }"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let release = client
            .create_release("group/repo", "v2.0.0", "Version 2", "Release notes")
            .await
            .unwrap();
        assert_eq!(release.tag, "v2.0.0");
        assert_eq!(release.name.as_deref(), Some("Version 2"));
        assert_eq!(
            release.url,
            "https://gitlab.com/group/repo/-/releases/v2.0.0"
        );
        assert_eq!(
            release.published_at.as_deref(),
            Some("2025-06-01T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn test_create_release_api_error() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/releases",
            422,
            r#"{"message": "Tag already exists"}"#,
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client
            .create_release("group/repo", "v1.0.0", "Version 1", "notes")
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to create release"),
            "error should mention create failure: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_create_release_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client
            .create_release("group/repo", "v1.0.0", "Version 1", "notes")
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[test]
    fn test_scm_provider_name_is_gitlab() {
        let config = test_config();
        let client = GitLabClient::new(config);
        let provider: &dyn ScmProvider = &client;
        assert_eq!(provider.name(), "gitlab");
    }

    #[tokio::test]
    async fn test_get_mr_approvals_no_token_direct() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.get_mr_approvals("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_mr_notes_no_token() {
        let client = GitLabClient::with_http_client(no_token_config(), MockHttpClient::new());
        let result = client.get_mr_notes("group/repo", 1).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("token"),
            "error should mention token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_scm_provider_merge_pr() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1/merge",
            200,
            "{}",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let provider: &dyn ScmProvider = &client;
        let result = provider.merge_pr("group/repo", 1).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_scm_provider_close_pr() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/merge_requests/1",
            200,
            "{}",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let provider: &dyn ScmProvider = &client;
        let result = provider.close_pr("group/repo", 1).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_scm_provider_delete_branch() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/group%2Frepo/repository/branches/my-branch",
            204,
            "",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let provider: &dyn ScmProvider = &client;
        let result = provider.delete_branch("group/repo", "my-branch").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_merge_pr_nested_project_path() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/org%2Fteam%2Frepo/merge_requests/7/merge",
            200,
            "{}",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.merge_pr("org/team/repo", 7).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_close_pr_nested_project_path() {
        let mock = MockHttpClient::new();
        mock.mock_response(
            "https://gitlab.com/api/v4/projects/org%2Fteam%2Frepo/merge_requests/7",
            200,
            "{}",
        );

        let client = GitLabClient::with_http_client(test_config(), mock);
        let result = client.close_pr("org/team/repo", 7).await;
        assert!(result.is_ok());
    }
}
