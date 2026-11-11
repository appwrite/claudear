use super::*;
use crate::security::SecurityInboxDependencies;
use async_trait::async_trait;
use claudear_config::config::{
    HelpScoutConfig, HelpScoutMailboxConfig, MailboxProfile, SecurityInboxConfig,
    CUSTOMER_THREAD_COUNT_METADATA_KEY,
};
use claudear_core::security::{
    security_fix_short_id, NewSecurityEvent, NewSecurityReport, NewSecurityVulnerability,
    SecurityEventKind, SecurityReportChanges, SecurityReportState, VulnerabilityState,
};
use claudear_core::types::{AgentResult, IndexedRepo, ReplyKind, RepoIndex};
use claudear_integrations::notifier::ConsoleNotifier;
use claudear_integrations::runner::{ProviderCapabilities, SecuritySandbox};
use claudear_integrations::scm::{CodeReview, PrInfo, RemoteRepo, ReviewComment};
use claudear_storage::{ActivityStore, AttemptTracker, SqliteTracker};
use std::path::Path;
use std::sync::atomic::AtomicUsize;

const SECURITY_MAILBOX: &str = "249891";
const SUPPORT_MAILBOX: &str = "249877";
const REPORT_BODY: &str =
    "The logout endpoint leaves the session token valid, so a stolen token keeps working.";
const UNTAGGED_METADATA_KEY: &str = "untagged";
const REVIEW_FEEDBACK: &str = "Always check the session token before trusting it";
const SDK_REPOSITORY: &str = "appwrite/sdk";

/// An agent whose security queries all fail, so every routed report is
/// released, and whose fix runs all fail.
#[derive(Default)]
struct Agent {
    queries: AtomicUsize,
    executions: AtomicUsize,
}

impl Agent {
    fn queries(&self) -> usize {
        self.queries.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AgentRunner for Agent {
    fn name(&self) -> &str {
        "security-test"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    fn build_prompt_for_issue(&self, _: &Issue, context: &str, _: &Path) -> String {
        context.to_string()
    }

    async fn execute_with_attempt(
        &self,
        _: &str,
        _: Option<&Issue>,
        _: Option<i64>,
        _: &Path,
    ) -> Result<AgentResult> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Err(claudear_core::error::Error::runner("the fix agent failed"))
    }

    async fn generate_reply(
        &self,
        _: &Issue,
        _: &str,
        _: Option<&str>,
        _: ReplyKind,
        _: &Path,
    ) -> Result<String> {
        Ok("The fix shipped.".to_string())
    }

    fn supports_security_query(&self) -> bool {
        true
    }

    async fn security_query(
        &self,
        _: &str,
        _: &str,
        _: &Path,
        _: SecuritySandbox,
        _: Duration,
    ) -> Result<serde_json::Value> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        Err(claudear_core::error::Error::runner(
            "the assessment agent is unavailable",
        ))
    }
}

/// A repository classifier that counts every classification it is asked for
/// and classifies nothing.
#[derive(Default)]
struct Classifier {
    classifications: AtomicUsize,
}

impl Classifier {
    fn classifications(&self) -> usize {
        self.classifications.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AgentRunner for Classifier {
    fn name(&self) -> &str {
        "classifier"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    fn build_prompt_for_issue(&self, _: &Issue, context: &str, _: &Path) -> String {
        context.to_string()
    }

    async fn execute_with_attempt(
        &self,
        _: &str,
        _: Option<&Issue>,
        _: Option<i64>,
        _: &Path,
    ) -> Result<AgentResult> {
        Err(claudear_core::error::Error::runner(
            "the classifier only classifies",
        ))
    }

    async fn structured_query(&self, _: &str, _: &str, _: &Path) -> Result<serde_json::Value> {
        self.classifications.fetch_add(1, Ordering::SeqCst);
        Err(claudear_core::error::Error::runner(
            "the classifier is unavailable",
        ))
    }
}

/// A source that serves `issues` and records what is done to them. An issue
/// whose [`UNTAGGED_METADATA_KEY`] is set does not match its trigger tags.
struct Source {
    name: String,
    issues: Mutex<Vec<Issue>>,
    lookups: AtomicUsize,
    resolved: Mutex<Vec<String>>,
    comments: Mutex<Vec<String>>,
}

impl Source {
    fn named(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            issues: Mutex::new(Vec::new()),
            lookups: AtomicUsize::new(0),
            resolved: Mutex::new(Vec::new()),
            comments: Mutex::new(Vec::new()),
        })
    }

    fn serve(&self, issues: Vec<Issue>) {
        *self.issues.lock().unwrap() = issues;
    }

    fn lookups(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }

    fn resolved(&self) -> Vec<String> {
        self.resolved.lock().unwrap().clone()
    }

    fn comments(&self) -> Vec<String> {
        self.comments.lock().unwrap().clone()
    }
}

#[async_trait]
impl IssueSource for Source {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        &self.name
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        Ok(self.issues.lock().unwrap().clone())
    }

    fn matches_criteria(&self, issue: &Issue) -> MatchResult {
        if issue.get_metadata::<bool>(UNTAGGED_METADATA_KEY) == Some(true) {
            MatchResult::not_matched("missing trigger tag")
        } else {
            MatchResult::matched("tagged", MatchPriority::Normal)
        }
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        Ok(issue.description.clone().unwrap_or_default())
    }

    async fn get_issue(&self, issue_id: &str) -> Result<Issue> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        self.issues
            .lock()
            .unwrap()
            .iter()
            .find(|issue| issue.id == issue_id)
            .cloned()
            .ok_or_else(|| claudear_core::error::Error::source(&self.name, "Issue not found"))
    }

    async fn resolve_issue(&self, issue_id: &str) -> Result<()> {
        self.resolved.lock().unwrap().push(issue_id.to_string());
        Ok(())
    }

    async fn add_comment(&self, _: &str, comment: &str) -> Result<()> {
        self.comments.lock().unwrap().push(comment.to_string());
        Ok(())
    }
}

/// An SCM on which every pull request has merged.
struct MergedScm;

#[async_trait]
impl ScmProvider for MergedScm {
    fn name(&self) -> &str {
        "merged-scm"
    }

    fn is_enabled(&self) -> bool {
        true
    }

    fn review_trigger(&self) -> &str {
        "@claudear"
    }

    async fn get_pr_status(&self, _: &str, _: i64) -> Result<PrStatus> {
        Ok(PrStatus::Merged)
    }

    async fn get_pr_info(&self, _: &str, _: i64) -> Result<PrInfo> {
        Ok(PrInfo {
            head_branch: None,
            base_branch: None,
            title: None,
            author: None,
        })
    }

    async fn get_pr_diff(&self, _: &str, _: i64) -> Result<String> {
        Ok(String::new())
    }

    async fn get_reviews(&self, _: &str, _: i64) -> Result<Vec<CodeReview>> {
        Ok(Vec::new())
    }

    async fn get_review_comments(&self, _: &str, _: i64) -> Result<Vec<ReviewComment>> {
        Ok(Vec::new())
    }

    async fn list_repos(&self, _: &str) -> Result<Vec<RemoteRepo>> {
        Ok(Vec::new())
    }
}

fn config() -> Config {
    let mut config = Config {
        processing_delay_ms: 0,
        ..Config::default()
    };
    config.issues.helpscout = Some(HelpScoutConfig {
        enabled: true,
        mailbox_ids: vec![SUPPORT_MAILBOX.to_string()],
        mailboxes: vec![HelpScoutMailboxConfig {
            id: SECURITY_MAILBOX.to_string(),
            profile: MailboxProfile::Security,
            reply_as: None,
            trigger_tags: Some(Vec::new()),
            trigger_status: None,
        }],
        security: SecurityInboxConfig {
            enabled: true,
            require_fix_approval: false,
            ..SecurityInboxConfig::default()
        },
        ..HelpScoutConfig::default()
    });
    config
}

fn report(conversation_id: &str) -> Issue {
    let mut issue = Issue::new(
        conversation_id,
        format!("HS-{conversation_id}"),
        "Session token survives logout",
        format!("https://secure.helpscout.net/conversation/{conversation_id}"),
        HELPSCOUT_SOURCE,
    );
    issue.description = Some(REPORT_BODY.to_string());
    issue.created_at = Some(Utc::now());
    issue.set_metadata("mailbox_id", SECURITY_MAILBOX);
    issue.set_metadata("customer_email", "researcher@example.com");
    issue.set_metadata("customer_id", "42");
    issue.set_metadata(CUSTOMER_THREAD_COUNT_METADATA_KEY, 1u32);
    issue
}

/// The conversation of `report(conversation_id)` after it moved to the
/// support mailbox.
fn moved(conversation_id: &str) -> Issue {
    let mut issue = report(conversation_id);
    issue.set_metadata("mailbox_id", SUPPORT_MAILBOX);
    issue
}

struct Fixture {
    tracker: Arc<SqliteTracker>,
    agent: Arc<Agent>,
    helpscout: Arc<Source>,
    inbox: Arc<SecurityInbox>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_config(config())
    }

    fn with_config(config: Config) -> Self {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let agent = Arc::new(Agent::default());
        let helpscout = Source::named(HELPSCOUT_SOURCE);
        let inbox = Arc::new(SecurityInbox::new(SecurityInboxDependencies {
            config: config
                .security_inbox()
                .cloned()
                .expect("the security inbox is enabled"),
            tracker: tracker.clone(),
            agent: agent.clone(),
            embedding_client: None,
            notifier: Arc::new(ConsoleNotifier::new()),
            source: helpscout.clone(),
            deployment: None,
            inferrer: None,
        }));
        Self {
            tracker,
            agent,
            helpscout,
            inbox,
        }
    }

    fn options(&self, config: Config, sources: Vec<Arc<dyn IssueSource>>) -> WatcherOptions {
        WatcherOptions {
            config,
            sources,
            notifier: Arc::new(ConsoleNotifier::new()),
            tracker: self.tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: None,
            github_client: None,
            scm_provider: None,
            user_registry: UserRegistry::new(HashMap::new()),
            agent: self.agent.clone(),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }
    }

    /// A watcher over the HelpScout source that hands security work to the
    /// inbox.
    fn watcher(&self) -> Arc<Watcher> {
        self.watcher_over(vec![self.helpscout.clone()])
    }

    fn watcher_over(&self, sources: Vec<Arc<dyn IssueSource>>) -> Arc<Watcher> {
        let watcher = Arc::new(Watcher::new(self.options(config(), sources)));
        watcher.set_security_inbox(self.inbox.clone());
        watcher
    }

    async fn poll(&self, watcher: &Arc<Watcher>) {
        let source: Arc<dyn IssueSource> = self.helpscout.clone();
        watcher.poll_source(&source).await.unwrap();
        watcher.drain_spawned_tasks().await;
    }

    fn stored_report(
        &self,
        conversation_id: &str,
    ) -> Option<claudear_core::security::SecurityReport> {
        self.tracker
            .get_security_report_by_conversation(HELPSCOUT_SOURCE, conversation_id)
            .unwrap()
    }

    /// A watcher whose repository inference indexes one repository and asks
    /// `classifier` to classify every issue.
    fn classifying_watcher(&self, classifier: Arc<Classifier>) -> Arc<Watcher> {
        let mut config = config();
        config.llm.use_agent = true;
        let mut index = RepoIndex::new();
        index.add_repo(IndexedRepo::new(
            "appwrite/appwrite",
            std::env::temp_dir().join("appwrite"),
        ));
        let mut options = self.options(config, vec![self.helpscout.clone()]);
        options.inferrer = Some(RepoInferrer::new(index));
        options.repo_classification_agent = Some(classifier);
        let watcher = Arc::new(Watcher::new(options));
        watcher.set_security_inbox(self.inbox.clone());
        watcher
    }

    /// Record the report of `conversation_id` as the security inbox does.
    fn hold_report(&self, conversation_id: &str) -> claudear_core::security::SecurityReport {
        self.tracker
            .upsert_security_report(&NewSecurityReport {
                source: HELPSCOUT_SOURCE.to_string(),
                conversation_id: conversation_id.to_string(),
                short_id: format!("HS-{conversation_id}"),
                mailbox_id: SECURITY_MAILBOX.to_string(),
                url: String::new(),
                subject: "Session token survives logout".to_string(),
                reporter_email: None,
                reporter_customer_id: None,
                reported_at: Utc::now(),
                customer_thread_count: 1,
            })
            .unwrap()
    }

    /// A vulnerability waiting for its first fix run, and its first report.
    fn awaiting_fix(&self) -> (i64, i64) {
        self.awaiting_fix_for("seeded")
    }

    /// A vulnerability first reported in `conversation_id` waiting for its
    /// first fix run, and that report.
    fn awaiting_fix_for(&self, conversation_id: &str) -> (i64, i64) {
        let report = self.hold_report(conversation_id);
        let vulnerability = self
            .tracker
            .create_security_vulnerability(&NewSecurityVulnerability {
                title: "Session tokens survive logout".to_string(),
                summary: "Logout does not revoke the session token.".to_string(),
                embedding: Vec::new(),
                repository: None,
                severity: None,
                state: VulnerabilityState::AwaitingFix,
                first_report_id: report.id,
                first_reported_at: report.reported_at,
                merge_commit: None,
                merged_at: None,
            })
            .unwrap();
        for (expected, next, changes) in [
            (
                SecurityReportState::Received,
                SecurityReportState::Assessing,
                SecurityReportChanges::default(),
            ),
            (
                SecurityReportState::Assessing,
                SecurityReportState::Tracking,
                SecurityReportChanges {
                    vulnerability_id: Some(vulnerability.id),
                    ..SecurityReportChanges::default()
                },
            ),
        ] {
            assert!(self
                .tracker
                .transition_security_report(
                    report.id,
                    expected,
                    next,
                    &changes,
                    &NewSecurityEvent::new(SecurityEventKind::Assessed),
                )
                .unwrap());
        }
        (vulnerability.id, report.id)
    }

    fn fix_failures(&self, report_id: i64) -> usize {
        self.tracker
            .list_security_timeline(report_id)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == SecurityEventKind::FixFailed)
            .count()
    }

    fn inference_records(&self) -> usize {
        self.tracker.get_inference_history(100).unwrap().len()
    }

    /// A merged attempt of `source` for `pull_request`.
    fn merged(&self, source: &str, issue_id: &str, pull_request: &str) {
        self.tracker
            .record_attempt(source, issue_id, issue_id)
            .unwrap();
        self.tracker
            .mark_success(source, issue_id, pull_request)
            .unwrap();
        self.tracker.mark_merged(source, issue_id).unwrap();
    }

    fn fix_attempt(&self, vulnerability_id: i64) -> Option<FixAttempt> {
        self.tracker
            .get_attempt(
                SECURITY_FIX_SOURCE,
                &security_fix_short_id(vulnerability_id),
            )
            .unwrap()
    }
}

#[tokio::test]
async fn a_security_conversation_reaches_the_inbox_past_the_attempted_ids() {
    let fixture = Fixture::new();
    fixture
        .tracker
        .record_attempt(HELPSCOUT_SOURCE, "5001", "HS-5001")
        .unwrap();
    fixture
        .helpscout
        .serve(vec![report("5001"), report("5002")]);
    let watcher = fixture.watcher();

    fixture.poll(&watcher).await;

    for conversation_id in ["5001", "5002"] {
        assert!(
            fixture.stored_report(conversation_id).is_some(),
            "{conversation_id} must reach the security inbox"
        );
    }
    assert_eq!(fixture.agent.queries(), 2, "each report is screened once");
    assert!(
        fixture
            .tracker
            .get_attempt(HELPSCOUT_SOURCE, "5002")
            .unwrap()
            .is_none(),
        "a security conversation must never get a generic attempt"
    );
}

#[tokio::test]
async fn a_released_report_is_routed_again_on_the_next_poll() {
    let fixture = Fixture::new();
    fixture.helpscout.serve(vec![report("5001")]);
    let watcher = fixture.watcher();

    fixture.poll(&watcher).await;
    let released = fixture.stored_report("5001").unwrap();
    fixture.poll(&watcher).await;

    assert_eq!(released.state, SecurityReportState::Received);
    assert_eq!(released.assessment_attempts, 1);
    let rerouted = fixture.stored_report("5001").unwrap();
    assert_eq!(rerouted.state, SecurityReportState::Received);
    assert_eq!(rerouted.assessment_attempts, 2);
    assert_eq!(fixture.agent.queries(), 2);
}

#[tokio::test]
async fn a_report_being_assessed_is_skipped() {
    let fixture = Fixture::new();
    fixture.helpscout.serve(vec![report("5001")]);
    let watcher = fixture.watcher();
    fixture.poll(&watcher).await;
    let stored = fixture.stored_report("5001").unwrap();
    assert!(fixture
        .tracker
        .transition_security_report(
            stored.id,
            SecurityReportState::Received,
            SecurityReportState::Assessing,
            &SecurityReportChanges::default(),
            &NewSecurityEvent::new(SecurityEventKind::AssessmentStarted),
        )
        .unwrap());

    fixture.poll(&watcher).await;

    assert_eq!(
        fixture.agent.queries(),
        1,
        "an assessing report must not be routed again"
    );
    assert_eq!(
        fixture.stored_report("5001").unwrap().state,
        SecurityReportState::Assessing
    );
}

#[tokio::test]
async fn security_conversations_are_dropped_without_an_inbox() {
    let fixture = Fixture::new();
    fixture.helpscout.serve(vec![report("5001")]);
    let helpscout: Arc<dyn IssueSource> = fixture.helpscout.clone();
    let watcher = Arc::new(Watcher::new(
        fixture.options(config(), vec![helpscout.clone()]),
    ));

    watcher.poll_source(&helpscout).await.unwrap();
    watcher.drain_spawned_tasks().await;

    assert!(fixture.stored_report("5001").is_none());
    assert!(fixture
        .tracker
        .get_attempt(HELPSCOUT_SOURCE, "5001")
        .unwrap()
        .is_none());
    assert_eq!(fixture.agent.queries(), 0);
}

#[tokio::test]
async fn a_security_conversation_without_its_trigger_tag_is_not_routed() {
    let fixture = Fixture::new();
    let mut untagged = report("5001");
    untagged.set_metadata(UNTAGGED_METADATA_KEY, true);
    fixture.helpscout.serve(vec![untagged]);
    let watcher = fixture.watcher();

    fixture.poll(&watcher).await;

    assert!(fixture.stored_report("5001").is_none());
    assert_eq!(fixture.agent.queries(), 0);
}

#[tokio::test]
async fn a_fix_whose_start_was_dropped_starts_exactly_once_on_the_next_polls() {
    let fixture = Fixture::new();
    let (vulnerability_id, report_id) = fixture.awaiting_fix();
    let watcher = fixture.watcher();

    fixture.poll(&watcher).await;
    fixture.poll(&watcher).await;

    let attempt = fixture
        .fix_attempt(vulnerability_id)
        .expect("the waiting fix must start");
    assert_eq!(attempt.retry_count, 0);
    assert_eq!(attempt.status, FixAttemptStatus::Failed);
    assert_eq!(
        fixture.fix_failures(report_id),
        1,
        "the fix must run exactly once"
    );
}

#[tokio::test]
async fn racing_starts_of_one_fix_run_it_once() {
    let fixture = Fixture::new();
    let (vulnerability_id, report_id) = fixture.awaiting_fix();
    let watcher = fixture.watcher();
    let first = fixture
        .inbox
        .fix_request(vulnerability_id)
        .unwrap()
        .unwrap();
    let second = first.clone();

    let (one, two) = tokio::join!(
        watcher.run_security_fix_request(&fixture.inbox, first, SecurityFixRun::Start),
        watcher.run_security_fix_request(&fixture.inbox, second, SecurityFixRun::Start),
    );
    let third = watcher
        .run_security_fix_request(
            &fixture.inbox,
            fixture
                .inbox
                .fix_request(vulnerability_id)
                .unwrap()
                .unwrap(),
            SecurityFixRun::Start,
        )
        .await;

    let runs = [one, two, third];
    assert_eq!(
        runs.iter()
            .filter(|run| **run == IssueRun::Processed)
            .count(),
        1,
        "{runs:?}"
    );
    assert_eq!(fixture.fix_failures(report_id), 1);
}

#[tokio::test]
async fn poll_holds_a_waiting_fix_until_it_is_approved() {
    let mut config = config();
    if let Some(helpscout) = config.issues.helpscout.as_mut() {
        helpscout.security.require_fix_approval = true;
    }
    let fixture = Fixture::with_config(config);
    let (vulnerability_id, _) = fixture.awaiting_fix();
    let watcher = fixture.watcher();

    watcher.poll().await.unwrap();
    watcher.drain_spawned_tasks().await;
    assert!(fixture.fix_attempt(vulnerability_id).is_none());

    fixture
        .tracker
        .record_security_event(
            None,
            Some(vulnerability_id),
            &NewSecurityEvent::new(SecurityEventKind::FixApproved),
        )
        .unwrap();
    watcher.poll().await.unwrap();
    watcher.drain_spawned_tasks().await;

    assert!(fixture.fix_attempt(vulnerability_id).is_some());
}

#[tokio::test]
async fn a_manual_trigger_cannot_start_an_unapproved_fix() {
    let mut config = config();
    if let Some(helpscout) = config.issues.helpscout.as_mut() {
        helpscout.security.require_fix_approval = true;
    }
    let fixture = Fixture::with_config(config);
    let (vulnerability_id, _) = fixture.awaiting_fix();
    let watcher = fixture.watcher();
    let short_id = security_fix_short_id(vulnerability_id);

    let refused = watcher.trigger_issue(SECURITY_FIX_SOURCE, &short_id).await;

    let error = refused.expect_err("an unapproved fix must not start");
    assert!(
        error.to_string().contains("awaiting fix approval"),
        "{error}"
    );
    assert!(fixture.fix_attempt(vulnerability_id).is_none());

    fixture
        .tracker
        .record_security_event(
            None,
            Some(vulnerability_id),
            &NewSecurityEvent::new(SecurityEventKind::FixApproved),
        )
        .unwrap();
    let _ = watcher.trigger_issue(SECURITY_FIX_SOURCE, &short_id).await;

    assert!(fixture.fix_attempt(vulnerability_id).is_some());
}

#[tokio::test]
async fn poll_advances_the_inbox_and_starts_waiting_fixes() {
    let fixture = Fixture::new();
    let (vulnerability_id, _) = fixture.awaiting_fix();
    let watcher = fixture.watcher();

    watcher.poll().await.unwrap();
    watcher.drain_spawned_tasks().await;

    assert!(fixture.fix_attempt(vulnerability_id).is_some());
}

#[tokio::test]
async fn a_watcher_without_the_helpscout_source_never_advances_the_inbox() {
    let fixture = Fixture::new();
    let (vulnerability_id, _) = fixture.awaiting_fix();
    let watcher = fixture.watcher_over(vec![Source::named("linear")]);

    watcher.poll().await.unwrap();
    watcher.drain_spawned_tasks().await;

    assert!(fixture.fix_attempt(vulnerability_id).is_none());
}

#[tokio::test]
async fn a_security_fix_retry_rebuilds_its_request_without_loading_an_issue() {
    let fixture = Fixture::new();
    let (vulnerability_id, report_id) = fixture.awaiting_fix();
    let watcher = fixture.watcher();
    fixture.poll(&watcher).await;
    let attempt = fixture.fix_attempt(vulnerability_id).unwrap();

    let outcome = watcher.retry(&attempt, "Retry attempt 1").await;

    assert!(matches!(outcome, RetryOutcome::Ran), "{outcome:?}");
    assert_eq!(
        fixture.fix_attempt(vulnerability_id).unwrap().retry_count,
        1
    );
    assert_eq!(fixture.fix_failures(report_id), 2);
    assert_eq!(
        fixture.helpscout.lookups(),
        0,
        "a security retry never loads a source issue"
    );
}

#[tokio::test]
async fn a_security_fix_review_rerun_rebuilds_its_request_without_loading_an_issue() {
    let fixture = Fixture::new();
    let (vulnerability_id, report_id) = fixture.awaiting_fix();
    let watcher = fixture.watcher();
    fixture.poll(&watcher).await;

    watcher
        .trigger_issue_with_feedback(
            SECURITY_FIX_SOURCE,
            &security_fix_short_id(vulnerability_id),
            Some("Cover the expired session too".to_string()),
            Some("fix-branch".to_string()),
            Some("Review feedback received".to_string()),
        )
        .await
        .unwrap();

    assert_eq!(fixture.fix_failures(report_id), 2);
    assert_eq!(fixture.helpscout.lookups(), 0);
}

#[tokio::test]
async fn a_security_fix_without_retries_left_needs_a_human() {
    let mut config = config();
    config.retry.max_retries = 0;
    let fixture = Fixture::with_config(config.clone());
    let (vulnerability_id, _) = fixture.awaiting_fix();
    let watcher = Arc::new(Watcher::new(
        fixture.options(config, vec![fixture.helpscout.clone()]),
    ));
    watcher.set_security_inbox(fixture.inbox.clone());

    fixture.poll(&watcher).await;

    let vulnerability = fixture
        .tracker
        .get_security_vulnerability(vulnerability_id)
        .unwrap()
        .unwrap();
    assert_eq!(vulnerability.state, VulnerabilityState::NeedsHuman);
}

#[tokio::test]
async fn a_triggered_security_conversation_goes_to_the_inbox() {
    let fixture = Fixture::new();
    fixture.helpscout.serve(vec![report("5001")]);
    let watcher = fixture.watcher();

    watcher
        .trigger_issue(HELPSCOUT_SOURCE, "5001")
        .await
        .unwrap();

    assert!(fixture.stored_report("5001").is_some());
    assert_eq!(fixture.agent.queries(), 1);
    assert!(fixture
        .tracker
        .get_attempt(HELPSCOUT_SOURCE, "5001")
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_merged_security_fix_skips_the_reply_the_resolve_and_the_learning() {
    let mut config = config();
    config.notifiers.helpscout.enabled = true;
    config.github_mut().auto_resolve_on_merge = true;
    let fixture = Fixture::with_config(config.clone());
    let security = Source::named(SECURITY_FIX_SOURCE);
    let mut options = fixture.options(config, vec![fixture.helpscout.clone(), security.clone()]);
    options.scm_provider = Some(Arc::new(MergedScm));
    let watcher = Arc::new(Watcher::new(options));
    watcher.set_security_inbox(fixture.inbox.clone());
    watcher.set_running(true);
    let mut support = report("8812");
    support.set_metadata("mailbox_id", SUPPORT_MAILBOX);
    fixture.helpscout.serve(vec![support]);
    for (source, issue_id, pull_request) in [
        (
            HELPSCOUT_SOURCE,
            "8812",
            "https://github.com/appwrite/appwrite/pull/1",
        ),
        (
            SECURITY_FIX_SOURCE,
            "SEC-1",
            "https://github.com/appwrite/appwrite/pull/2",
        ),
    ] {
        fixture
            .tracker
            .record_attempt(source, issue_id, issue_id)
            .unwrap();
        fixture
            .tracker
            .mark_success(source, issue_id, pull_request)
            .unwrap();
    }

    watcher.check_pr_merges_and_cascade().await.unwrap();

    assert_eq!(fixture.helpscout.resolved(), vec!["8812".to_string()]);
    assert_eq!(
        fixture.helpscout.comments().len(),
        1,
        "the support fix gets its reply"
    );
    assert!(security.resolved().is_empty(), "{:?}", security.resolved());
    assert!(security.comments().is_empty(), "{:?}", security.comments());
    assert_eq!(
        fixture
            .tracker
            .get_attempt(SECURITY_FIX_SOURCE, "SEC-1")
            .unwrap()
            .unwrap()
            .status,
        FixAttemptStatus::Merged,
        "the merge of a security fix is still tracked"
    );
    assert!(fixture
        .tracker
        .get_feedback_outcomes(Some(SECURITY_FIX_SOURCE), 10)
        .unwrap()
        .is_empty());
    assert_eq!(
        fixture
            .tracker
            .get_feedback_outcomes(Some(HELPSCOUT_SOURCE), 10)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_closed_security_fix_records_no_feedback_outcome() {
    let fixture = Fixture::new();
    let watcher = fixture.watcher();
    fixture
        .tracker
        .record_attempt(SECURITY_FIX_SOURCE, "SEC-1", "SEC-1")
        .unwrap();
    let attempt = fixture
        .tracker
        .get_attempt(SECURITY_FIX_SOURCE, "SEC-1")
        .unwrap()
        .unwrap();

    watcher
        .record_feedback_outcome_from_attempt(&attempt, Outcome::Closed)
        .await;

    assert!(fixture
        .tracker
        .get_feedback_outcomes(None, 10)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn seeding_leaves_security_conversations_to_the_inbox() {
    let fixture = Fixture::new();
    let mut support = report("8812");
    support.set_metadata("mailbox_id", SUPPORT_MAILBOX);
    fixture.helpscout.serve(vec![report("5001"), support]);
    let watcher = fixture.watcher();

    let seeded = watcher.seed().await.unwrap();

    assert_eq!(seeded.total, 1);
    assert!(fixture
        .tracker
        .get_attempt(HELPSCOUT_SOURCE, "5001")
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_generic_retry_of_a_security_conversation_records_nothing_about_it() {
    let fixture = Fixture::new();
    fixture.helpscout.serve(vec![report("5001")]);
    fixture
        .tracker
        .record_attempt(HELPSCOUT_SOURCE, "5001", "HS-5001")
        .unwrap();
    fixture
        .tracker
        .mark_failed(HELPSCOUT_SOURCE, "5001", "an earlier failure")
        .unwrap();
    let attempt = fixture
        .tracker
        .get_attempt(HELPSCOUT_SOURCE, "5001")
        .unwrap()
        .unwrap();
    let watcher = fixture.watcher();

    watcher.retry(&attempt, "Retry attempt 1").await;

    let mentions = watcher
        .tracker
        .get_recent_activities(100)
        .unwrap()
        .into_iter()
        .filter(|activity| activity.issue_id.as_deref() == Some("5001"))
        .map(|activity| activity.activity_type)
        .filter(|activity_type| activity_type != "retry_executed")
        .collect::<Vec<_>>();
    assert!(
        mentions.is_empty(),
        "a security conversation must leave no generic record: {mentions:?}"
    );
    assert_eq!(fixture.agent.queries(), 0);
}

fn assert_refused(outcome: crate::processing::ProcessingOutcome) {
    match outcome {
        crate::processing::ProcessingOutcome::CompletedNoPr { reason } => {
            assert_eq!(reason, SECURITY_REFUSAL)
        }
        crate::processing::ProcessingOutcome::Failed { error } => {
            panic!("expected the refusal, got Failed: {error}")
        }
        crate::processing::ProcessingOutcome::Success { pr_url } => {
            panic!("expected the refusal, got Success: {pr_url}")
        }
        crate::processing::ProcessingOutcome::WrongRepo { .. } => {
            panic!("expected the refusal, got WrongRepo")
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_action_on_a_security_conversation_reaches_no_repository_classifier() {
    let fixture = Fixture::new();
    fixture.helpscout.serve(vec![report("5001")]);
    let classifier = Arc::new(Classifier::default());
    let watcher = fixture.classifying_watcher(classifier.clone());

    for action in [ActionKind::Verify, ActionKind::Reply, ActionKind::Resolve] {
        assert_refused(
            watcher
                .run_action(action, HELPSCOUT_SOURCE, "5001")
                .await
                .unwrap(),
        );
    }

    assert_eq!(classifier.classifications(), 0);
    assert_eq!(fixture.inference_records(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_action_on_a_support_conversation_still_infers_its_repository() {
    let fixture = Fixture::new();
    let mut support = report("8812");
    support.set_metadata("mailbox_id", SUPPORT_MAILBOX);
    fixture.helpscout.serve(vec![support]);
    let classifier = Arc::new(Classifier::default());
    let watcher = fixture.classifying_watcher(classifier.clone());

    watcher
        .run_action(ActionKind::Verify, HELPSCOUT_SOURCE, "8812")
        .await
        .unwrap();

    assert_eq!(classifier.classifications(), 1);
    assert_eq!(fixture.inference_records(), 1);
}

#[tokio::test]
async fn a_moved_conversation_the_inbox_holds_stays_out_of_the_poll_pipeline() {
    let fixture = Fixture::new();
    fixture.hold_report("5001");
    fixture.helpscout.serve(vec![moved("5001")]);
    let watcher = fixture.watcher();
    watcher.set_running(true);

    fixture.poll(&watcher).await;

    assert!(fixture
        .tracker
        .get_attempt(HELPSCOUT_SOURCE, "5001")
        .unwrap()
        .is_none());
    let mentions = fixture
        .tracker
        .get_recent_activities(100, None)
        .unwrap()
        .into_iter()
        .filter(|activity| {
            activity.issue_id.as_deref() == Some("5001")
                || activity
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.to_string().contains("HS-5001"))
        })
        .map(|activity| activity.activity_type)
        .collect::<Vec<_>>();
    assert!(mentions.is_empty(), "{mentions:?}");
    assert_eq!(fixture.agent.queries(), 0);
}

#[tokio::test]
async fn a_manual_action_on_a_moved_conversation_the_inbox_holds_is_refused() {
    let fixture = Fixture::new();
    fixture.hold_report("5001");
    fixture.helpscout.serve(vec![moved("5001")]);
    let watcher = fixture.watcher();

    for action in [ActionKind::Verify, ActionKind::Reply, ActionKind::Resolve] {
        assert_refused(
            watcher
                .run_action(action, HELPSCOUT_SOURCE, "5001")
                .await
                .unwrap(),
        );
    }
    assert!(fixture.helpscout.comments().is_empty());
}

#[tokio::test]
async fn a_triggered_moved_conversation_the_inbox_holds_is_refused() {
    let fixture = Fixture::new();
    fixture.hold_report("5001");
    fixture.helpscout.serve(vec![moved("5001")]);
    let watcher = fixture.watcher();

    let error = watcher
        .trigger_issue(HELPSCOUT_SOURCE, "5001")
        .await
        .unwrap_err();

    assert!(error.to_string().contains(SECURITY_REFUSAL), "{error}");
    assert!(fixture
        .tracker
        .get_attempt(HELPSCOUT_SOURCE, "5001")
        .unwrap()
        .is_none());
    assert_eq!(fixture.agent.queries(), 0);
}

#[tokio::test]
async fn a_retried_moved_conversation_the_inbox_holds_is_skipped() {
    let fixture = Fixture::new();
    fixture.hold_report("5001");
    fixture.helpscout.serve(vec![moved("5001")]);
    fixture
        .tracker
        .record_attempt(HELPSCOUT_SOURCE, "5001", "HS-5001")
        .unwrap();
    fixture
        .tracker
        .mark_failed(HELPSCOUT_SOURCE, "5001", "an earlier failure")
        .unwrap();
    let attempt = fixture
        .tracker
        .get_attempt(HELPSCOUT_SOURCE, "5001")
        .unwrap()
        .unwrap();
    let watcher = fixture.watcher();

    let outcome = watcher.retry(&attempt, "Retry attempt 1").await;

    match outcome {
        RetryOutcome::Failed(error) => {
            assert!(error.to_string().contains("was skipped"), "{error}")
        }
        outcome => panic!("expected a skipped retry, got {outcome:?}"),
    }
    let mentions = watcher
        .tracker
        .get_recent_activities(100)
        .unwrap()
        .into_iter()
        .filter(|activity| activity.issue_id.as_deref() == Some("5001"))
        .map(|activity| activity.activity_type)
        .filter(|activity_type| activity_type != "retry_executed")
        .collect::<Vec<_>>();
    assert!(mentions.is_empty(), "{mentions:?}");
}

#[tokio::test]
async fn seeding_skips_a_moved_conversation_the_inbox_holds() {
    let fixture = Fixture::new();
    fixture.hold_report("5001");
    fixture.helpscout.serve(vec![moved("5001")]);
    let watcher = fixture.watcher();

    let seeded = watcher.seed().await.unwrap();

    assert_eq!(seeded.total, 0);
}

#[tokio::test]
async fn review_feedback_on_a_security_fix_is_not_learned() {
    let mut config = config();
    config.learning.review_classification = true;
    let fixture = Fixture::with_config(config.clone());
    let watcher = Arc::new(Watcher::new(
        fixture.options(config, vec![fixture.helpscout.clone()]),
    ));
    watcher.set_security_inbox(fixture.inbox.clone());

    for (source, issue_id, learned) in [
        (SECURITY_FIX_SOURCE, "SEC-1", false),
        ("linear", "LIN-1", true),
    ] {
        let repository = format!("appwrite/{issue_id}");
        fixture
            .tracker
            .record_attempt(source, issue_id, issue_id)
            .unwrap();
        let mut attempt = fixture
            .tracker
            .get_attempt(source, issue_id)
            .unwrap()
            .unwrap();
        attempt.scm_repo = Some(repository.clone());

        let _ = watcher
            .process_review_action(&attempt, REVIEW_FEEDBACK)
            .await;

        let patterns = fixture
            .tracker
            .get_review_patterns(&repository, 10)
            .unwrap();
        assert_eq!(!patterns.is_empty(), learned, "{source}: {patterns:?}");
    }
}

#[tokio::test]
async fn waiting_security_fixes_start_only_in_free_processing_slots() {
    let fixture = Fixture::new();
    let (first, _) = fixture.awaiting_fix_for("first");
    let (second, _) = fixture.awaiting_fix_for("second");
    let watcher = fixture.watcher();
    let started = |fixture: &Fixture| {
        [first, second]
            .into_iter()
            .filter(|vulnerability_id| fixture.fix_attempt(*vulnerability_id).is_some())
            .count()
    };

    watcher.advance_security_inbox().await;
    watcher.drain_spawned_tasks().await;
    let after_one_advance = started(&fixture);
    watcher.advance_security_inbox().await;
    watcher.drain_spawned_tasks().await;

    assert_eq!(after_one_advance, 1, "max_concurrent is one");
    assert_eq!(
        started(&fixture),
        2,
        "the waiting fix starts on the next advance"
    );
}

#[tokio::test]
async fn a_release_cascade_starts_from_the_last_merge_before_a_security_fix() {
    let fixture = Fixture::new();
    let watcher = fixture.watcher();
    fixture.merged(
        "linear",
        "LIN-1",
        &format!("https://github.com/{SDK_REPOSITORY}/pull/1"),
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    fixture.merged(
        SECURITY_FIX_SOURCE,
        "SEC-1",
        &format!("https://github.com/{SDK_REPOSITORY}/pull/2"),
    );
    assert_eq!(
        fixture
            .tracker
            .get_most_recent_merged_attempt_for_repo(SDK_REPOSITORY)
            .unwrap()
            .unwrap()
            .source,
        SECURITY_FIX_SOURCE,
        "the security fix is the latest merge"
    );

    let attempt = watcher
        .release_cascade_attempt(SDK_REPOSITORY)
        .expect("the release cascades");

    assert_eq!(attempt.source, "linear");
    assert_eq!(attempt.issue_id, "LIN-1");
}

#[tokio::test]
async fn a_release_whose_only_merge_is_a_security_fix_starts_no_cascade() {
    let fixture = Fixture::new();
    let watcher = fixture.watcher();
    fixture.merged(
        SECURITY_FIX_SOURCE,
        "SEC-1",
        &format!("https://github.com/{SDK_REPOSITORY}/pull/2"),
    );

    assert!(watcher.release_cascade_attempt(SDK_REPOSITORY).is_none());
}

/// Bounds the block so a regression fails the test instead of hanging it.
const BLOCKING_LIMIT: Duration = Duration::from_secs(60);

/// A source whose fetch blocks its thread until released, as a run stuck in a
/// synchronous call does.
#[derive(Default)]
struct Blocking {
    released: AtomicBool,
}

impl Blocking {
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl IssueSource for Blocking {
    fn name(&self) -> &str {
        "sentry"
    }

    fn display_name(&self) -> &str {
        "Sentry"
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        let started = std::time::Instant::now();
        while !self.released.load(Ordering::SeqCst) && started.elapsed() < BLOCKING_LIMIT {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(Vec::new())
    }

    fn matches_criteria(&self, _: &Issue) -> MatchResult {
        MatchResult::not_matched("never processed")
    }

    async fn build_issue_context(&self, _: &Issue) -> Result<String> {
        Ok(String::new())
    }

    async fn get_issue(&self, _: &str) -> Result<Issue> {
        Err(claudear_core::error::Error::source(
            "sentry",
            "Issue not found",
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_source_blocking_its_thread_never_stalls_security_intake() {
    let fixture = Fixture::new();
    fixture.helpscout.serve(vec![report("5001")]);
    let blocking = Arc::new(Blocking::default());
    let mut config = config();
    config.poll_interval_ms = 1000;
    let watcher = Arc::new(Watcher::new(
        fixture.options(config, vec![blocking.clone(), fixture.helpscout.clone()]),
    ));
    watcher.set_security_inbox(fixture.inbox.clone());

    let loops = {
        let watcher = Arc::clone(&watcher);
        tokio::spawn(async move { watcher.start(Some(1000)).await })
    };
    let started = std::time::Instant::now();
    let mut reached = None;
    while reached.is_none() && started.elapsed() < BLOCKING_LIMIT {
        if fixture.stored_report("5001").is_some() {
            reached = Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    watcher.stop();
    blocking.release();
    loops.await.unwrap().unwrap();

    assert!(
        reached.is_some_and(|elapsed| elapsed < BLOCKING_LIMIT / 2),
        "a source blocking its polling thread kept a security report from the inbox: reached after {reached:?}"
    );
}

#[tokio::test]
async fn a_separate_intake_leaves_security_conversations_out_of_the_source_poll() {
    let fixture = Fixture::new();
    fixture.helpscout.serve(vec![report("5001")]);
    let watcher = fixture.watcher();
    let source: Arc<dyn IssueSource> = fixture.helpscout.clone();

    watcher
        .poll_source_with(&source, SecurityIntake::Separate)
        .await
        .unwrap();
    watcher.drain_spawned_tasks().await;

    assert!(fixture.stored_report("5001").is_none());
    assert_eq!(fixture.agent.queries(), 0);
    assert!(fixture
        .tracker
        .get_attempt(HELPSCOUT_SOURCE, "5001")
        .unwrap()
        .is_none());

    watcher.poll_security_source(&source).await.unwrap();
    watcher.drain_spawned_tasks().await;
    assert!(fixture.stored_report("5001").is_some());
}
