//! Generic instrumented wrappers for all resource traits.
//!
//! Each wrapper automatically logs trait method calls:
//! - `tracing::info!` on success with structured fields
//! - `tracing::warn!` on error, then propagates the `Err`

use crate::notifier::Delivery;
use crate::notifier::DigestDelivery;
use crate::notifier::Notifier;
use crate::reports::RepetitiveDigest;
use crate::reports::Report;
use crate::runner::{AgentRunner, ProviderCapabilities};
use crate::scm::{
    CodeReview, PostReviewAction, PrInfo, PrStatus, PrSummary, RemoteRepo, ReviewComment,
    ScmProvider, ScmRelease,
};
use crate::source::{ConversationStatus, IssueSource, ReporterReply};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use claudear_analysis::distribution::AssignmentNotice;
use claudear_analysis::distribution::Digest;
use claudear_core::error::Result;
use claudear_core::types::{
    AgentResult, AskDelivery, AskReply, AskRequest, Issue, MatchResult, ReplyKind, VerifyResult,
};
use std::path::Path;
use std::sync::Arc;

macro_rules! delegate {
    (fn $method:ident(&self $(, $arg:ident: $ty:ty)*) -> $ret:ty) => {
        fn $method(&self $(, $arg: $ty)*) -> $ret {
            self.inner.$method($($arg),*)
        }
    };
}

pub struct InstrumentedSource {
    inner: Arc<dyn IssueSource>,
}

impl InstrumentedSource {
    pub fn wrap(inner: Arc<dyn IssueSource>) -> Arc<dyn IssueSource> {
        Arc::new(Self { inner })
    }
}

#[async_trait]
impl IssueSource for InstrumentedSource {
    delegate!(fn name(&self) -> &str);
    delegate!(fn display_name(&self) -> &str);
    delegate!(fn matches_criteria(&self, issue: &Issue) -> MatchResult);
    delegate!(fn is_terminal_status(&self, status: &str) -> bool);

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        self.inner.build_issue_context(issue).await
    }
    async fn get_issue(&self, issue_id: &str) -> Result<Issue> {
        self.inner.get_issue(issue_id).await
    }
    async fn get_issue_status(&self, issue_id: &str) -> Result<String> {
        self.inner.get_issue_status(issue_id).await
    }
    async fn find_or_create_label(&self, name: &str) -> Result<String> {
        self.inner.find_or_create_label(name).await
    }
    async fn list_open_issues(&self, title_filter: &str) -> Result<Vec<Issue>> {
        self.inner.list_open_issues(title_filter).await
    }
    async fn list_conversations(&self, status: Option<&str>) -> Result<Vec<Issue>> {
        self.inner.list_conversations(status).await
    }

    async fn reply_to_reporter(
        &self,
        issue_id: &str,
        body: &str,
        status: Option<ConversationStatus>,
    ) -> ReporterReply {
        let outcome = self.inner.reply_to_reporter(issue_id, body, status).await;
        match &outcome {
            ReporterReply::Sent => {
                tracing::info!(component = self.inner.name(), issue_id = %issue_id, "Replied to reporter");
            }
            ReporterReply::Rejected(reason) => {
                tracing::warn!(component = self.inner.name(), issue_id = %issue_id, reason = %reason, "Reporter reply rejected");
            }
            ReporterReply::Unconfirmed(reason) => {
                tracing::warn!(component = self.inner.name(), issue_id = %issue_id, reason = %reason, "Reporter reply unconfirmed");
            }
        }
        outcome
    }

    async fn mark_spam(&self, issue_id: &str) -> Result<()> {
        match self.inner.mark_spam(issue_id).await {
            Ok(()) => {
                tracing::info!(component = self.inner.name(), issue_id = %issue_id, "Marked conversation as spam");
                Ok(())
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue_id = %issue_id, error = %e, "Failed to mark conversation as spam");
                Err(e)
            }
        }
    }

    async fn move_conversation(&self, issue_id: &str, mailbox_id: &str) -> Result<()> {
        match self.inner.move_conversation(issue_id, mailbox_id).await {
            Ok(()) => {
                tracing::info!(component = self.inner.name(), issue_id = %issue_id, mailbox_id = %mailbox_id, "Moved conversation");
                Ok(())
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue_id = %issue_id, mailbox_id = %mailbox_id, error = %e, "Failed to move conversation");
                Err(e)
            }
        }
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        match self.inner.fetch_issues().await {
            Ok(issues) => {
                tracing::info!(
                    component = self.inner.name(),
                    count = issues.len(),
                    "Fetched issues"
                );
                Ok(issues)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), error = %e, "Failed to fetch issues");
                Err(e)
            }
        }
    }

    async fn resolve_issue(&self, issue_id: &str) -> Result<()> {
        match self.inner.resolve_issue(issue_id).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), issue_id = %issue_id, "Resolved issue");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue_id = %issue_id, error = %e, "Failed to resolve issue");
                Err(e)
            }
        }
    }

    async fn add_comment(&self, issue_id: &str, comment: &str) -> Result<()> {
        match self.inner.add_comment(issue_id, comment).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), issue_id = %issue_id, "Added comment");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue_id = %issue_id, error = %e, "Failed to add comment");
                Err(e)
            }
        }
    }

    async fn create_issue(
        &self,
        title: &str,
        description: &str,
        labels: &[String],
    ) -> Result<Issue> {
        match self.inner.create_issue(title, description, labels).await {
            Ok(issue) => {
                tracing::info!(component = self.inner.name(), issue_id = %issue.short_id, "Created issue");
                Ok(issue)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), error = %e, "Failed to create issue");
                Err(e)
            }
        }
    }
}

pub struct InstrumentedNotifier {
    inner: Arc<dyn Notifier>,
}

impl InstrumentedNotifier {
    pub fn wrap(inner: Arc<dyn Notifier>) -> Arc<dyn Notifier> {
        Arc::new(Self { inner })
    }
}

#[async_trait]
impl Notifier for InstrumentedNotifier {
    delegate!(fn name(&self) -> &str);
    delegate!(fn is_enabled(&self) -> bool);
    delegate!(fn is_local(&self) -> bool);
    delegate!(fn supports_replies(&self) -> bool);
    async fn poll_question_replies(
        &self,
        request: &AskRequest,
        since: DateTime<Utc>,
    ) -> Result<Vec<AskReply>> {
        self.inner.poll_question_replies(request, since).await
    }

    async fn notify_start(&self, issue: &Issue) -> Result<()> {
        match self.inner.notify_start(issue).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), issue = %issue.short_id, "Notified start");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue = %issue.short_id, error = %e, "Failed to notify start");
                Err(e)
            }
        }
    }

    async fn notify_success(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        match self.inner.notify_success(issue, pr_url).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), issue = %issue.short_id, pr_url = %pr_url, "Notified success");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue = %issue.short_id, error = %e, "Failed to notify success");
                Err(e)
            }
        }
    }

    async fn notify_completed(&self, issue: &Issue) -> Result<()> {
        match self.inner.notify_completed(issue).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), issue = %issue.short_id, "Notified completed");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue = %issue.short_id, error = %e, "Failed to notify completed");
                Err(e)
            }
        }
    }

    async fn notify_failed(&self, issue: &Issue, error: &str) -> Result<()> {
        match self.inner.notify_failed(issue, error).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), issue = %issue.short_id, "Notified failed");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue = %issue.short_id, error = %e, "Failed to notify failed");
                Err(e)
            }
        }
    }

    async fn notify_status(&self, message: &str) -> Result<()> {
        match self.inner.notify_status(message).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), "Notified status");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), error = %e, "Failed to notify status");
                Err(e)
            }
        }
    }

    async fn notify_answer(&self, issue: &Issue, answer: &str) -> Result<Vec<String>> {
        // Forward to the inner notifier so reply-capable channels (e.g. Discord)
        // actually run and return their sent message ids. Without this the trait
        // default would run here and the ids would be lost.
        match self.inner.notify_answer(issue, answer).await {
            Ok(ids) => {
                tracing::info!(component = self.inner.name(), issue = %issue.short_id, sent = ids.len(), "Notified answer");
                Ok(ids)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue = %issue.short_id, error = %e, "Failed to notify answer");
                Err(e)
            }
        }
    }

    async fn notify_urgent_issues(&self, issues: &[Issue]) -> Result<()> {
        match self.inner.notify_urgent_issues(issues).await {
            Ok(v) => {
                tracing::info!(
                    component = self.inner.name(),
                    count = issues.len(),
                    "Notified urgent issues"
                );
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), error = %e, "Failed to notify urgent issues");
                Err(e)
            }
        }
    }

    async fn notify_merged(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        match self.inner.notify_merged(issue, pr_url).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), issue = %issue.short_id, "Notified merged");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue = %issue.short_id, error = %e, "Failed to notify merged");
                Err(e)
            }
        }
    }

    async fn notify_closed(&self, issue: &Issue, pr_url: &str) -> Result<()> {
        match self.inner.notify_closed(issue, pr_url).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), issue = %issue.short_id, "Notified closed");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue = %issue.short_id, error = %e, "Failed to notify closed");
                Err(e)
            }
        }
    }

    async fn notify_report(&self, report: &Report) -> Result<()> {
        match self.inner.notify_report(report).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), "Notified report");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), error = %e, "Failed to notify report");
                Err(e)
            }
        }
    }

    async fn notify_repetitive_digest(&self, digest: &RepetitiveDigest) -> Result<()> {
        match self.inner.notify_repetitive_digest(digest).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), "Notified repetitive digest");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), error = %e, "Failed to notify repetitive digest");
                Err(e)
            }
        }
    }

    async fn notify_pr_assignment(&self, notice: &AssignmentNotice) -> Result<Delivery> {
        match self.inner.notify_pr_assignment(notice).await {
            Ok(v) => {
                tracing::info!(
                    component = self.inner.name(),
                    pr_url = %notice.pull_request.url,
                    "Notified PR assignment"
                );
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), pr_url = %notice.pull_request.url, error = %e, "Failed to notify PR assignment");
                Err(e)
            }
        }
    }

    async fn notify_pr_digest(&self, digest: &Digest) -> Result<DigestDelivery> {
        match self.inner.notify_pr_digest(digest).await {
            Ok(v) => {
                tracing::info!(
                    component = self.inner.name(),
                    count = digest.count(),
                    "Notified PR digest"
                );
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), error = %e, "Failed to notify PR digest");
                Err(e)
            }
        }
    }

    async fn ask_question(
        &self,
        issue: &Issue,
        request: &AskRequest,
    ) -> Result<Option<AskDelivery>> {
        match self.inner.ask_question(issue, request).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), issue = %issue.short_id, "Asked question");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), issue = %issue.short_id, error = %e, "Failed to ask question");
                Err(e)
            }
        }
    }
}

pub struct InstrumentedScm {
    inner: Arc<dyn ScmProvider>,
}

impl InstrumentedScm {
    pub fn wrap(inner: Arc<dyn ScmProvider>) -> Arc<dyn ScmProvider> {
        Arc::new(Self { inner })
    }
}

#[async_trait]
impl ScmProvider for InstrumentedScm {
    delegate!(fn name(&self) -> &str);
    delegate!(fn is_enabled(&self) -> bool);
    delegate!(fn review_trigger(&self) -> &str);
    fn allowed_bots(&self) -> &[String] {
        self.inner.allowed_bots()
    }
    async fn self_login(&self) -> Option<String> {
        self.inner.self_login().await
    }
    delegate!(fn pr_url_pattern(&self) -> &str);
    delegate!(fn parse_pr_number(&self, url: &str) -> Option<i64>);

    async fn get_pr_status(&self, project: &str, number: i64) -> Result<PrStatus> {
        self.inner.get_pr_status(project, number).await
    }
    async fn get_pr_info(&self, project: &str, number: i64) -> Result<PrInfo> {
        self.inner.get_pr_info(project, number).await
    }
    async fn get_pr_diff(&self, project: &str, number: i64) -> Result<String> {
        self.inner.get_pr_diff(project, number).await
    }
    async fn get_reviews(&self, project: &str, number: i64) -> Result<Vec<CodeReview>> {
        self.inner.get_reviews(project, number).await
    }
    async fn get_review_comments(&self, project: &str, number: i64) -> Result<Vec<ReviewComment>> {
        self.inner.get_review_comments(project, number).await
    }
    async fn get_new_reviews(
        &self,
        project: &str,
        number: i64,
        since: Option<&str>,
    ) -> Result<Vec<CodeReview>> {
        self.inner.get_new_reviews(project, number, since).await
    }
    async fn get_new_review_comments(
        &self,
        project: &str,
        number: i64,
        since: Option<&str>,
    ) -> Result<Vec<ReviewComment>> {
        self.inner
            .get_new_review_comments(project, number, since)
            .await
    }
    async fn get_pr_conversation_comments(
        &self,
        project: &str,
        number: i64,
    ) -> Result<Vec<ReviewComment>> {
        self.inner
            .get_pr_conversation_comments(project, number)
            .await
    }
    async fn get_new_conversation_comments(
        &self,
        project: &str,
        number: i64,
        since: Option<&str>,
    ) -> Result<Vec<ReviewComment>> {
        self.inner
            .get_new_conversation_comments(project, number, since)
            .await
    }
    async fn list_repos(&self, org_or_group: &str) -> Result<Vec<RemoteRepo>> {
        self.inner.list_repos(org_or_group).await
    }
    async fn list_open_prs(&self, project: &str) -> Result<Vec<PrSummary>> {
        self.inner.list_open_prs(project).await
    }
    async fn get_pr_branch(&self, project: &str, number: i64) -> Result<String> {
        self.inner.get_pr_branch(project, number).await
    }
    async fn get_latest_release(&self, project: &str) -> Result<Option<ScmRelease>> {
        self.inner.get_latest_release(project).await
    }

    async fn merge_pr(&self, project: &str, number: i64) -> Result<()> {
        match self.inner.merge_pr(project, number).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), project = %project, number = number, "Merged PR");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), project = %project, number = number, error = %e, "Failed to merge PR");
                Err(e)
            }
        }
    }

    async fn close_pr(&self, project: &str, number: i64) -> Result<()> {
        match self.inner.close_pr(project, number).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), project = %project, number = number, "Closed PR");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), project = %project, number = number, error = %e, "Failed to close PR");
                Err(e)
            }
        }
    }

    async fn post_review(
        &self,
        project: &str,
        number: i64,
        action: PostReviewAction,
        body: &str,
    ) -> Result<()> {
        match self.inner.post_review(project, number, action, body).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), project = %project, number = number, "Posted review");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), project = %project, number = number, error = %e, "Failed to post review");
                Err(e)
            }
        }
    }

    async fn delete_branch(&self, project: &str, branch: &str) -> Result<()> {
        match self.inner.delete_branch(project, branch).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), project = %project, branch = %branch, "Deleted branch");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), project = %project, branch = %branch, error = %e, "Failed to delete branch");
                Err(e)
            }
        }
    }

    async fn create_release(
        &self,
        project: &str,
        tag: &str,
        name: &str,
        body: &str,
    ) -> Result<ScmRelease> {
        match self.inner.create_release(project, tag, name, body).await {
            Ok(v) => {
                tracing::info!(component = self.inner.name(), project = %project, tag = %tag, "Created release");
                Ok(v)
            }
            Err(e) => {
                tracing::warn!(component = self.inner.name(), project = %project, tag = %tag, error = %e, "Failed to create release");
                Err(e)
            }
        }
    }
}

pub struct InstrumentedRunner {
    inner: Arc<dyn AgentRunner>,
}

impl InstrumentedRunner {
    pub fn wrap(inner: Arc<dyn AgentRunner>) -> Arc<dyn AgentRunner> {
        Arc::new(Self { inner })
    }
}

#[async_trait]
impl AgentRunner for InstrumentedRunner {
    delegate!(fn name(&self) -> &str);
    delegate!(fn capabilities(&self) -> ProviderCapabilities);
    delegate!(fn build_prompt_for_issue(&self, issue: &Issue, context: &str, project_dir: &Path) -> String);

    async fn execute_with_attempt(
        &self,
        prompt: &str,
        issue: Option<&Issue>,
        attempt_id: Option<i64>,
        project_dir: &Path,
    ) -> Result<AgentResult> {
        let start = std::time::Instant::now();
        match self
            .inner
            .execute_with_attempt(prompt, issue, attempt_id, project_dir)
            .await
        {
            Ok(result) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                tracing::info!(
                    component = self.inner.name(),
                    duration_ms = duration_ms,
                    success = result.success,
                    "Agent execution completed"
                );
                Ok(result)
            }
            Err(e) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                tracing::warn!(
                    component = self.inner.name(),
                    duration_ms = duration_ms,
                    error = %e,
                    "Agent execution failed"
                );
                Err(e)
            }
        }
    }

    async fn answer_question(
        &self,
        issue: &Issue,
        context: &str,
        project_dir: &Path,
    ) -> Result<String> {
        self.inner
            .answer_question(issue, context, project_dir)
            .await
    }

    async fn verify_issue(
        &self,
        issue: &Issue,
        context: &str,
        project_dir: &Path,
    ) -> Result<VerifyResult> {
        self.inner.verify_issue(issue, context, project_dir).await
    }

    async fn generate_reply(
        &self,
        issue: &Issue,
        context: &str,
        guideline: Option<&str>,
        kind: ReplyKind,
        project_dir: &Path,
    ) -> Result<String> {
        self.inner
            .generate_reply(issue, context, guideline, kind, project_dir)
            .await
    }

    async fn structured_query(
        &self,
        prompt: &str,
        json_schema: &str,
        project_dir: &Path,
    ) -> Result<serde_json::Value> {
        self.inner
            .structured_query(prompt, json_schema, project_dir)
            .await
    }

    delegate!(fn supports_security_query(&self) -> bool);

    async fn security_query(
        &self,
        prompt: &str,
        json_schema: &str,
        project_dir: &Path,
        sandbox: crate::runner::SecuritySandbox,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value> {
        self.inner
            .security_query(prompt, json_schema, project_dir, sandbox, timeout)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_analysis::distribution::Developer;
    use claudear_analysis::distribution::DigestGroup;
    use claudear_analysis::distribution::PullRequestSummary;
    use claudear_core::types::AssignmentReason;
    use std::sync::Mutex;

    /// Records which trait method was called, so a wrapper falling back to a
    /// default (which routes through `notify_status`) shows up as "status".
    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<&'static str>>,
    }

    impl Recorder {
        fn record(&self, call: &'static str) -> Result<()> {
            self.calls.lock().unwrap().push(call);
            Ok(())
        }
    }

    #[async_trait]
    impl Notifier for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        fn is_enabled(&self) -> bool {
            true
        }
        async fn notify_start(&self, _issue: &Issue) -> Result<()> {
            self.record("start")
        }
        async fn notify_success(&self, _issue: &Issue, _pr_url: &str) -> Result<()> {
            self.record("success")
        }
        async fn notify_completed(&self, _issue: &Issue) -> Result<()> {
            self.record("completed")
        }
        async fn notify_failed(&self, _issue: &Issue, _error: &str) -> Result<()> {
            self.record("failed")
        }
        async fn notify_status(&self, _message: &str) -> Result<()> {
            self.record("status")
        }
        async fn notify_urgent_issues(&self, _issues: &[Issue]) -> Result<()> {
            self.record("urgent")
        }
        async fn notify_repetitive_digest(&self, _digest: &RepetitiveDigest) -> Result<()> {
            self.record("repetitive_digest")
        }
        async fn notify_pr_assignment(&self, _notice: &AssignmentNotice) -> Result<Delivery> {
            self.record("pr_assignment")?;
            Ok(Delivery::Delivered)
        }
        async fn notify_pr_digest(&self, digest: &Digest) -> Result<DigestDelivery> {
            self.record("pr_digest")?;
            Ok(DigestDelivery::accepted(digest, false))
        }
    }

    #[tokio::test]
    async fn instrumented_notifier_forwards_mention_capable_digests() {
        let recorder = Arc::new(Recorder::default());
        let notifier = InstrumentedNotifier::wrap(Arc::clone(&recorder) as Arc<dyn Notifier>);
        let summary = PullRequestSummary {
            url: "https://github.com/org/repo/pull/1".to_string(),
            repo: "org/repo".to_string(),
            number: 1,
            title: "Fix".to_string(),
            opened_at: None,
        };

        notifier
            .notify_repetitive_digest(&RepetitiveDigest {
                period: "week".to_string(),
                from: Utc::now(),
                to: Utc::now(),
                entries: Vec::new(),
            })
            .await
            .unwrap();
        notifier
            .notify_pr_assignment(&AssignmentNotice {
                developer: Developer::new("amy"),
                pull_request: summary.clone(),
                reason: AssignmentReason::Load,
                previous: None,
            })
            .await
            .unwrap();
        notifier
            .notify_pr_digest(&Digest {
                groups: vec![DigestGroup {
                    developer: Developer::new("amy"),
                    pull_requests: vec![summary],
                }],
                generated_at: Utc::now(),
            })
            .await
            .unwrap();

        assert_eq!(
            *recorder.calls.lock().unwrap(),
            vec!["repetitive_digest", "pr_assignment", "pr_digest"]
        );
    }
}
