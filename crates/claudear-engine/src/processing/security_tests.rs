use super::*;
use claudear_config::config::{HelpScoutConfig, HelpScoutMailboxConfig, MailboxProfile};
use claudear_core::types::{AgentResult, BlockingQuestion, FixAttemptStatus, MatchResult};
use claudear_integrations::runner::ProviderCapabilities;
use claudear_storage::{AttemptTracker, SqliteTracker};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SECURITY_MAILBOX: &str = "249891";
const SUPPORT_MAILBOX: &str = "249877";
const BRIEF: &str = "# Security fix SEC-7\nHarden the session revocation on logout.";
const DIAGNOSIS_MARKER: &str = "DIAGNOSIS-FREE-TEXT";
const TOKEN: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
const HOST: &str = "billing.svc.cluster.local";

#[derive(Debug, Clone)]
struct ExecutedRun {
    prompt: String,
    issue: Option<Issue>,
}

/// An agent that records every fix run and answers each with `result`.
struct RecordingAgent {
    result: AgentResult,
    contexts: Mutex<Vec<String>>,
    runs: Mutex<Vec<ExecutedRun>>,
}

impl RecordingAgent {
    fn answering(result: AgentResult) -> Arc<Self> {
        Arc::new(Self {
            result,
            contexts: Mutex::new(Vec::new()),
            runs: Mutex::new(Vec::new()),
        })
    }

    fn failing(error: &str) -> Arc<Self> {
        Self::answering(agent_result(false, Some(error.to_string()), None))
    }

    fn runs(&self) -> Vec<ExecutedRun> {
        self.runs.lock().unwrap().clone()
    }
}

fn agent_result(
    success: bool,
    error: Option<String>,
    blocking_question: Option<BlockingQuestion>,
) -> AgentResult {
    AgentResult {
        success,
        output: String::new(),
        pr_url: None,
        changelog: None,
        error,
        blocking_question,
        used_qa_ids: vec![],
        confidence: 0,
        confidence_reasoning: None,
        wrong_repo: None,
    }
}

#[async_trait]
impl AgentRunner for RecordingAgent {
    fn name(&self) -> &str {
        "recording"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    fn build_prompt_for_issue(&self, issue: &Issue, context: &str, _: &Path) -> String {
        self.contexts.lock().unwrap().push(context.to_string());
        format!("{} {}\n{context}", issue.short_id, issue.title)
    }

    async fn execute_with_attempt(
        &self,
        prompt: &str,
        issue: Option<&Issue>,
        _: Option<i64>,
        _: &Path,
    ) -> Result<AgentResult> {
        self.runs.lock().unwrap().push(ExecutedRun {
            prompt: prompt.to_string(),
            issue: issue.cloned(),
        });
        Ok(self.result.clone())
    }
}

/// A notifier that records the error of every failure it is told about.
#[derive(Default)]
struct FailureNotifier {
    failures: Mutex<Vec<String>>,
}

#[async_trait]
impl Notifier for FailureNotifier {
    fn name(&self) -> &str {
        "failures"
    }

    fn is_enabled(&self) -> bool {
        true
    }

    async fn notify_start(&self, _: &Issue) -> Result<()> {
        Ok(())
    }

    async fn notify_success(&self, _: &Issue, _: &str) -> Result<()> {
        Ok(())
    }

    async fn notify_completed(&self, _: &Issue) -> Result<()> {
        Ok(())
    }

    async fn notify_failed(&self, _: &Issue, error: &str) -> Result<()> {
        self.failures.lock().unwrap().push(error.to_string());
        Ok(())
    }

    async fn notify_status(&self, _: &str) -> Result<()> {
        Ok(())
    }

    async fn notify_urgent_issues(&self, _: &[Issue]) -> Result<()> {
        Ok(())
    }
}

struct StaticContext(&'static str);

#[async_trait]
impl ContextProvider for StaticContext {
    async fn build_issue_context(&self, _: &Issue) -> Result<String> {
        Ok(self.0.to_string())
    }
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(directory)
        .args([
            "-c",
            "user.name=claudear",
            "-c",
            "user.email=claudear@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(arguments)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A clone of a local origin with one commit on `main`.
struct LocalRepository {
    directory: tempfile::TempDir,
    clone: PathBuf,
    origin: PathBuf,
}

impl LocalRepository {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let origin = directory.path().join("origin.git");
        let work = directory.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(
            directory.path(),
            &["init", "--bare", "-b", "main", "origin.git"],
        );
        git(&work, &["init", "-b", "main"]);
        git(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        std::fs::write(work.join("README.md"), "repository\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "initial import"]);
        git(&work, &["push", "origin", "main"]);
        let clone = directory.path().join("clone");
        git(
            directory.path(),
            &["clone", origin.to_str().unwrap(), "clone"],
        );
        Self {
            directory,
            clone,
            origin,
        }
    }

    fn resolution(&self) -> RepoResolution {
        RepoResolution::Resolved {
            project_dir: self.clone.clone(),
            repo_name: "appwrite/appwrite".to_string(),
            repo_id: None,
            scm_url: self.origin.to_string_lossy().to_string(),
            default_branch: "main".to_string(),
            confidence: None,
        }
    }

    fn workspace(&self) -> PathBuf {
        let workspace = self.directory.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        workspace
    }
}

fn security_config() -> Config {
    let mut config = Config::default();
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
        ..HelpScoutConfig::default()
    });
    config
}

struct Fixture {
    processor: IssueProcessor,
    tracker: Arc<SqliteTracker>,
    notifier: Arc<FailureNotifier>,
}

impl Fixture {
    fn new(agent: Arc<dyn AgentRunner>) -> Self {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(FailureNotifier::default());
        let processor = IssueProcessor {
            config: security_config(),
            tracker: tracker.clone(),
            notifier: notifier.clone(),
            agent,
            qa_agent: None,
            inferrer: None,
            embedding_client: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            feedback_analyzer: Arc::new(tokio::sync::Mutex::new(FeedbackAnalyzer::new())),
            review_watcher: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            github_client: None,
            llm_analyzer: None,
            intent_classifier: None,
        };
        Self {
            processor,
            tracker,
            notifier,
        }
    }

    fn failures(&self) -> Vec<String> {
        self.notifier.failures.lock().unwrap().clone()
    }
}

fn helpscout_issue(mailbox_id: Option<&str>) -> Issue {
    let mut issue = Issue::new(
        "8812",
        "HS-8812",
        "Session token survives logout",
        "https://secure.helpscout.net/conversation/8812",
        "helpscout",
    );
    issue.description = Some("A stolen token keeps working after logout.".to_string());
    issue.set_metadata("customer_email", "researcher@example.com");
    if let Some(mailbox_id) = mailbox_id {
        issue.set_metadata("mailbox_id", mailbox_id);
    }
    issue
}

fn security_fix_issue() -> Issue {
    Issue::new(
        "SEC-7",
        "SEC-7",
        "Security fix SEC-7",
        "",
        SECURITY_FIX_SOURCE,
    )
}

fn diagnosis() -> VerifyResult {
    VerifyResult {
        reproduced: true,
        summary: DIAGNOSIS_MARKER.to_string(),
        impact: DIAGNOSIS_MARKER.to_string(),
        root_cause: DIAGNOSIS_MARKER.to_string(),
        suggested_fix: DIAGNOSIS_MARKER.to_string(),
        evidence: DIAGNOSIS_MARKER.to_string(),
        triage: TriageVerdict::Fix,
        owner_repo: String::new(),
    }
}

fn input(issue: Issue, resolution: RepoResolution) -> ProcessingInput {
    ProcessingInput {
        source_name: issue.source.clone(),
        issue,
        match_result: MatchResult::matched("test", claudear_core::types::MatchPriority::Urgent),
        resolution,
        attempt_id: None,
        review_feedback: None,
        existing_pr_branch: None,
        intent: Some(Intent::Security),
        diagnosis: Some(diagnosis()),
    }
}

fn unresolved() -> RepoResolution {
    RepoResolution::Skip {
        reason: "no repository".to_string(),
    }
}

fn assert_refused(outcome: ProcessingOutcome) {
    match outcome {
        ProcessingOutcome::CompletedNoPr { reason } => assert_eq!(reason, SECURITY_REFUSAL),
        ProcessingOutcome::Failed { error } => panic!("expected the refusal, got Failed: {error}"),
        ProcessingOutcome::Success { pr_url } => {
            panic!("expected the refusal, got Success: {pr_url}")
        }
        ProcessingOutcome::WrongRepo { .. } => panic!("expected the refusal, got WrongRepo"),
    }
}

fn failed(outcome: ProcessingOutcome) -> String {
    match outcome {
        ProcessingOutcome::Failed { error } => error,
        ProcessingOutcome::CompletedNoPr { reason } => {
            panic!("expected Failed, got CompletedNoPr: {reason}")
        }
        ProcessingOutcome::Success { pr_url } => panic!("expected Failed, got Success: {pr_url}"),
        ProcessingOutcome::WrongRepo { .. } => panic!("expected Failed, got WrongRepo"),
    }
}

fn security_issues() -> Vec<Issue> {
    vec![
        helpscout_issue(Some(SECURITY_MAILBOX)),
        helpscout_issue(None),
        security_fix_issue(),
    ]
}

#[tokio::test]
async fn run_refuses_every_security_issue_without_running_the_agent() {
    for issue in security_issues() {
        let agent = RecordingAgent::failing("unused");
        let fixture = Fixture::new(agent.clone());

        let outcome = fixture
            .processor
            .run(input(issue.clone(), unresolved()), &StaticContext(""))
            .await;

        assert_refused(outcome);
        assert!(
            agent.runs().is_empty(),
            "{} reached the agent",
            issue.short_id
        );
        assert!(
            fixture
                .tracker
                .get_attempt(&issue.source, &issue.id)
                .unwrap()
                .is_none(),
            "{} was recorded",
            issue.short_id
        );
    }
}

#[tokio::test]
async fn run_single_action_refuses_every_security_issue_for_every_action() {
    for action in [ActionKind::Verify, ActionKind::Reply, ActionKind::Resolve] {
        for issue in security_issues() {
            let agent = RecordingAgent::failing("unused");
            let fixture = Fixture::new(agent.clone());

            let outcome = fixture
                .processor
                .run_single_action(
                    action,
                    input(issue.clone(), unresolved()),
                    &StaticContext(""),
                )
                .await;

            assert_refused(outcome);
            assert!(
                agent.runs().is_empty(),
                "{action} on {} reached the agent",
                issue.short_id
            );
        }
    }
}

#[tokio::test]
async fn run_still_routes_a_support_conversation() {
    let fixture = Fixture::new(RecordingAgent::failing("unused"));
    let mut support = input(helpscout_issue(Some(SUPPORT_MAILBOX)), unresolved());
    support.intent = Some(Intent::Fix);

    let error = failed(fixture.processor.run(support, &StaticContext("")).await);

    assert!(
        error.starts_with("Repository resolution failed"),
        "a support conversation must reach the fix pipeline: {error}"
    );
}

#[tokio::test]
async fn run_security_fix_refuses_anything_but_a_security_fix() {
    let agent = RecordingAgent::failing("unused");
    let fixture = Fixture::new(agent.clone());

    let error = failed(
        fixture
            .processor
            .run_security_fix(
                input(helpscout_issue(Some(SECURITY_MAILBOX)), unresolved()),
                &StaticContext(BRIEF),
            )
            .await,
    );

    assert!(error.contains("only runs security issues"), "{error}");
    assert!(agent.runs().is_empty());
}

#[tokio::test]
async fn a_security_fix_reaches_the_agent_with_only_the_neutral_brief() {
    let repository = LocalRepository::new();
    let agent = RecordingAgent::failing("the fix did not build");
    let mut fixture = Fixture::new(agent.clone());
    fixture.processor.config.workspace = repository.workspace();
    let issue = security_fix_issue();
    fixture
        .tracker
        .record_attempt(SECURITY_FIX_SOURCE, &issue.id, &issue.short_id)
        .unwrap();

    fixture
        .processor
        .run_security_fix(
            input(issue, repository.resolution()),
            &crate::security::SecurityFixContext {
                context: BRIEF.to_string(),
            },
        )
        .await;

    let runs = agent.runs();
    assert_eq!(runs.len(), 1, "the fix agent must run once");
    let run = &runs[0];
    assert!(
        run.prompt.contains("Harden the session revocation"),
        "{}",
        run.prompt
    );
    for leaked in [
        "helpscout",
        "HS-",
        "secure.helpscout.net",
        "researcher@example.com",
        DIAGNOSIS_MARKER,
    ] {
        assert!(
            !run.prompt.contains(leaked),
            "the fix prompt must not carry {leaked}: {}",
            run.prompt
        );
    }
    let agent_issue = run.issue.as_ref().expect("the fix run names its issue");
    assert_eq!(agent_issue.source, SECURITY_FIX_SOURCE);
    assert_eq!(agent_issue.short_id, "SEC-7");
    assert_eq!(agent_issue.url, "");
    assert!(agent_issue.description.is_none());
}

#[tokio::test]
async fn a_failed_security_fix_stores_and_notifies_only_scrubbed_errors() {
    let repository = LocalRepository::new();
    let agent = RecordingAgent::failing(&format!("push to {HOST} failed with {TOKEN}"));
    let mut fixture = Fixture::new(agent.clone());
    fixture.processor.config.workspace = repository.workspace();
    let issue = security_fix_issue();
    fixture
        .tracker
        .record_attempt(SECURITY_FIX_SOURCE, &issue.id, &issue.short_id)
        .unwrap();

    let error = failed(
        fixture
            .processor
            .run_security_fix(
                input(issue.clone(), repository.resolution()),
                &StaticContext(BRIEF),
            )
            .await,
    );

    let stored = fixture
        .tracker
        .get_attempt(SECURITY_FIX_SOURCE, &issue.id)
        .unwrap()
        .expect("the attempt is recorded")
        .error_message
        .unwrap_or_default();
    let notified = fixture.failures();
    assert_eq!(notified.len(), 1, "the failure must be notified once");
    for text in [&error, &stored, &notified[0]] {
        assert!(!text.contains(TOKEN), "{text}");
        assert!(!text.contains(HOST), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
    }
    assert!(
        fixture.tracker.get_error_patterns(10).unwrap().is_empty(),
        "a security fix must record no error pattern"
    );
    assert!(
        fixture
            .tracker
            .get_feedback_outcomes(None, 10)
            .unwrap()
            .is_empty(),
        "a security fix must record no feedback outcome"
    );
}

#[tokio::test]
async fn a_security_fix_that_asks_for_human_input_cannot_be_fixed() {
    let repository = LocalRepository::new();
    let agent = RecordingAgent::answering(agent_result(
        false,
        None,
        Some(BlockingQuestion {
            question: "Which session store does production use?".to_string(),
            context: None,
            options: vec![],
            why: None,
        }),
    ));
    let mut fixture = Fixture::new(agent.clone());
    fixture.processor.config.workspace = repository.workspace();
    fixture.processor.config.ask.enabled = true;
    let issue = security_fix_issue();
    fixture
        .tracker
        .record_attempt(SECURITY_FIX_SOURCE, &issue.id, &issue.short_id)
        .unwrap();

    let error = failed(
        fixture
            .processor
            .run_security_fix(
                input(issue.clone(), repository.resolution()),
                &StaticContext(BRIEF),
            )
            .await,
    );

    assert_eq!(error, SECURITY_FIX_NEEDS_HUMAN);
    assert_eq!(
        agent.runs().len(),
        1,
        "no stored answer may rerun the agent"
    );
    let attempt = fixture
        .tracker
        .get_attempt(SECURITY_FIX_SOURCE, &issue.id)
        .unwrap()
        .expect("the attempt is recorded");
    assert_eq!(attempt.status, FixAttemptStatus::CannotFix);
    let asked = fixture
        .processor
        .tracker
        .get_recent_activities(100)
        .unwrap()
        .into_iter()
        .filter(|activity| {
            matches!(
                activity.activity_type.as_str(),
                "question_asked" | "question_reused"
            )
        })
        .count();
    assert_eq!(
        asked, 0,
        "a security fix never asks humans or reuses answers"
    );
}

#[tokio::test]
async fn a_security_fix_records_no_feedback_outcome() {
    let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
    let analyzer = tokio::sync::Mutex::new(FeedbackAnalyzer::new());
    let issue = security_fix_issue();
    tracker
        .record_attempt(SECURITY_FIX_SOURCE, &issue.id, &issue.short_id)
        .unwrap();

    record_feedback_outcome(
        &tracker,
        None,
        None,
        &analyzer,
        SECURITY_FIX_SOURCE,
        &issue,
        Outcome::Merged,
    )
    .await;

    assert!(tracker.get_feedback_outcomes(None, 10).unwrap().is_empty());
    assert!(analyzer.lock().await.tracker().all().is_empty());
}

#[test]
fn a_security_fix_records_no_error_pattern() {
    let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());

    record_error_pattern(&tracker, SECURITY_FIX_SOURCE, "SEC-7", "cargo build failed");
    record_error_pattern(&tracker, "sentry", "SENTRY-1", "cargo build failed");

    let patterns = tracker.get_error_patterns(10).unwrap();
    assert_eq!(patterns.len(), 1);
    assert_eq!(patterns[0].sources, Some(vec!["sentry".to_string()]));
}

#[tokio::test]
async fn notify_failed_with_escalation_scrubs_only_security_fix_errors() {
    let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
    let recording = Arc::new(FailureNotifier::default());
    let notifier: Arc<dyn Notifier> = recording.clone();
    let error = format!("rate limit exceeded calling {HOST} with {TOKEN}");

    notify_failed_with_escalation(&notifier, &tracker, &security_fix_issue(), &error)
        .await
        .unwrap();
    notify_failed_with_escalation(&notifier, &tracker, &helpscout_issue(None), &error)
        .await
        .unwrap();

    let failures = recording.failures.lock().unwrap().clone();
    assert!(
        !failures[0].contains(TOKEN) && !failures[0].contains(HOST),
        "{}",
        failures[0]
    );
    assert_eq!(failures[1], error);
    let escalated = tracker
        .get_recent_activities(10)
        .unwrap()
        .into_iter()
        .filter(|activity| activity.source.as_deref() == Some(SECURITY_FIX_SOURCE))
        .filter_map(|activity| activity.metadata)
        .map(|metadata| metadata["error"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert_eq!(escalated.len(), 1);
    assert!(!escalated[0].contains(TOKEN), "{}", escalated[0]);
}

#[test]
fn a_security_fix_uses_no_issue_embeddings() {
    assert!(is_security_fix(&security_fix_issue()));
    assert!(!is_security_fix(&helpscout_issue(Some(SUPPORT_MAILBOX))));
    let fixture = Fixture::new(RecordingAgent::failing("unused"));
    assert!(fixture
        .processor
        .embedding_service_for(&security_fix_issue())
        .is_none());
}

#[test]
fn a_pull_request_opened_for_a_security_fix_is_neutral() {
    let (title, body) = pull_request_text(&security_fix_issue(), SECURITY_FIX_SOURCE, "fix-branch");

    for text in [&title, &body] {
        assert!(!text.contains("SEC-"), "{text}");
        assert!(!text.contains("security"), "{text}");
    }
    let (title, body) = pull_request_text(&helpscout_issue(None), "helpscout", "fix-branch");
    assert_eq!(title, "Fix: Session token survives logout");
    assert!(body.starts_with("Resolves HS-8812 (helpscout)."), "{body}");
}

#[test]
fn a_security_fix_red_phase_prompt_does_not_name_its_source() {
    let prompt = build_failing_test_prompt(&security_fix_issue(), BRIEF);

    assert!(!prompt.contains("from security"), "{prompt}");
    assert!(prompt.contains("Harden the session revocation"), "{prompt}");
    let support = build_failing_test_prompt(&helpscout_issue(None), "context");
    assert!(support.contains("from helpscout"), "{support}");
}
