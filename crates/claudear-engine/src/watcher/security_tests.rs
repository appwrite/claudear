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
use claudear_core::types::{AgentResult, ReplyKind};
use claudear_integrations::notifier::ConsoleNotifier;
use claudear_integrations::runner::{ProviderCapabilities, SecuritySandbox};
use claudear_integrations::scm::{CodeReview, PrInfo, RemoteRepo, ReviewComment};
use claudear_storage::{AttemptTracker, SqliteTracker};
use std::path::Path;
use std::sync::atomic::AtomicUsize;

const SECURITY_MAILBOX: &str = "249891";
const SUPPORT_MAILBOX: &str = "249877";
const REPORT_BODY: &str =
    "The logout endpoint leaves the session token valid, so a stolen token keeps working.";
const UNTAGGED_METADATA_KEY: &str = "untagged";

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

    /// A vulnerability waiting for its first fix run, and its first report.
    fn awaiting_fix(&self) -> (i64, i64) {
        let report = self
            .tracker
            .upsert_security_report(&NewSecurityReport {
                source: HELPSCOUT_SOURCE.to_string(),
                conversation_id: "seeded".to_string(),
                short_id: "HS-seeded".to_string(),
                mailbox_id: SECURITY_MAILBOX.to_string(),
                url: String::new(),
                subject: "Session token survives logout".to_string(),
                reporter_email: None,
                reporter_customer_id: None,
                reported_at: Utc::now(),
                customer_thread_count: 1,
            })
            .unwrap();
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
