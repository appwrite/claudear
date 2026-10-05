//! Main watcher that coordinates sources, Claude, and notifications.

use crate::heartbeat::{Heartbeat, Liveness};
use crate::intent::Intent;
use crate::llm_classifier::LlmRepoClassifier;
use crate::repo_index::build_repo_index_with_fallback;
use crate::retry::RetryManager;
use crate::shutdown::DRAIN_TIMEOUT;
use chrono::{DateTime, Utc};
use claudear_analysis::deploy_qa::DEPLOY_QA_SOURCE;
use claudear_analysis::feedback::{FeedbackAnalyzer, IssueEmbeddingService, Outcome};
use claudear_analysis::inference::{
    resolve_repo_for_cascade, resolve_repo_for_issue, Confidence, RepoInferrer, RepoResolution,
};
use claudear_analysis::qa::build_correlation_id;
use claudear_analysis::repo::{worktree_path, GitOps, RepoRelationships};
use claudear_config::config::Config;
use claudear_config::users::UserRegistry;
use claudear_core::error::Result;
use claudear_core::types::{
    ActionKind, ActivityLogEntry, AskRequest, BlockingQuestion, DeployQaTip, DeployQaTipStatus,
    FixAttempt, FixAttemptStats, FixAttemptStatus, Issue, IssueEmbedding, IssueType, MatchPriority,
    MatchResult, PrRecord, ProcessingMetric, RegressionWatch, ReplyKind, TimelineEventStatus,
};
use claudear_integrations::github::GitHubClient;
use claudear_integrations::notifier::{send_to_all_and_wait_first_reply, Notifier};
use claudear_integrations::reports::{ReportFrequency, ReportGenerator, ReportSchedule};
use claudear_integrations::runner::{self, AgentRunner};
use claudear_integrations::scm::{
    PrReviewState, PrStatus, ReviewEvent, ReviewWatcher, ScmProvider,
};
use claudear_integrations::source::IssueSource;
use claudear_storage::FixAttemptTracker;
use futures::future::join_all;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::futures::Notified;
use tokio::sync::{Notify, RwLock, Semaphore};
use tokio::time::{interval, Duration, MissedTickBehavior};

/// A candidate issue ready for dispatch: the issue, its match result, and the
/// decided routing `Intent` (`None` for non-QA-eligible / QA-disabled sources).
type QueuedIssue = (Issue, MatchResult, Option<Intent>);

/// How many times a PR review comment may fail processing before it is given up
/// on (marked handled) instead of re-triggering the fix agent every cycle.
const MAX_REVIEW_COMMENT_ATTEMPTS: i64 = 5;

/// Issue metadata carrying the reviewed PR's repo into a review rerun.
const REVIEW_PR_REPO_KEY: &str = "review_pr_repo";

/// Decision recorded when a review rerun waits for its PR's repository to be
/// indexed.
const REVIEW_RERUN_DEFERRED_DECISION: &str = "review_rerun_deferred";

/// How many review-driven reruns a PR gets before claudear stops answering
/// review feedback on it and leaves the PR to humans.
const MAX_REVIEW_CYCLES: i32 = 3;

/// Activity recorded when a PR has used [`MAX_REVIEW_CYCLES`] and is left to
/// humans.
const REVIEW_CYCLE_CAP_REACHED_ACTIVITY: &str = "review_cycle_cap_reached";

/// Why an attempt is closed as declined when a human refuses approval.
const APPROVAL_DECLINED_REASON: &str = "Approval declined";

const MAX_RATE_LIMIT_RESET_DAYS_AHEAD: i64 = 8;

/// How often a drain re-reads the in-flight count, so a missed wake-up delays
/// shutdown by at most this long.
const DRAIN_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Shortest wait between passes of the source polling loop, so sources with
/// shorter intervals cannot make it busy-loop.
const MIN_SOURCE_POLL_PERIOD: Duration = Duration::from_secs(1);

/// Why a trigger is refused once [`Watcher::stop`] has been called.
const STOPPING_REFUSAL: &str = "Watcher is stopping; not starting new runs";

/// Why an issue triggered outside a poll is processed.
pub const MANUAL_TRIGGER: &str = "Manual trigger";

/// Prefix of the error recorded on an attempt whose retry could not run.
pub(crate) const RETRY_TRIGGER_FAILED: &str = "Retry trigger failed";

/// How [`Watcher::process_issue`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IssueRun {
    /// The issue went through the processing pipeline, whatever it
    /// concluded.
    Processed,
    /// A human refused approval, so the run did no work and closed the
    /// issue's attempt as declined.
    Declined,
    /// No usable answer came to the approval request, as it went unanswered
    /// or its reply was not understood, so the run did no work and left the
    /// issue's attempt for a later run to ask again.
    Unanswered,
    /// The issue was skipped, such as a `deploy_qa` tip that already has a
    /// verdict, a review rerun whose PR repository is not indexed or a run its
    /// [`Admission`] refused.
    Skipped,
    /// The issue is already being processed, so no run started.
    Busy,
    /// The watcher is stopping, so no run started.
    Stopping,
    /// The watcher is paused for a rate limit, so no run started.
    Paused,
}

/// Decides whether a run [`Watcher::process_issue`] admitted may start. It is
/// called at most once, after the issue is claimed and the watcher is found
/// not stopping, and before anything is recorded for the run.
type Admission<'a> = Box<dyn FnOnce() -> bool + Send + 'a>;

/// How [`Watcher::retry`] ended.
#[derive(Debug)]
pub enum RetryOutcome {
    /// The issue went through the processing pipeline, or a human refused
    /// approval for it, which closed its attempt as declined.
    Ran,
    /// The watcher is stopping, so the attempt is left as it was for the next
    /// start.
    Stopping,
    /// The issue is already being processed, so the attempt is left as it was
    /// for a later retry.
    Busy,
    /// The retry could not run. A transient error, such as a network error or
    /// a pause for a rate limit, leaves the attempt as it was, and an approval
    /// request that got no usable answer marks it failed with its retry given
    /// back; any other error spent the attempt's retry and marked it failed,
    /// unless the retry itself could not be spent.
    Failed(claudear_core::error::Error),
}

/// Whether a retry that failed to start should get its retry back.
fn retry_trigger_error_is_transient(e: &claudear_core::error::Error) -> bool {
    use claudear_core::error::Error;
    match e {
        Error::Http(_) | Error::Network(_) => true,
        Error::Source { message, .. } => message.contains("already being processed"),
        _ => false,
    }
}

/// Extracts the source name from a processing key of the form "source:issue_id".
fn source_from_processing_key(key: &str) -> &str {
    key.split_once(':').map_or(key, |(source, _)| source)
}

/// Decision from parsing an approval reply.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ApprovalDecision {
    /// User approved processing.
    Approved,
    /// User denied processing.
    Denied,
    /// User redirected to a different repo.
    Redirect { repo_name: String },
    /// Reply could not be parsed.
    Unrecognized,
    /// No reply came before the request timed out, or the request could not
    /// be sent or read.
    Unanswered,
}

/// Parse a human reply to an approval request.
fn parse_approval_reply(answer: &str) -> ApprovalDecision {
    let normalized = answer
        .trim()
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_lowercase();

    // Check redirect prefixes first
    for prefix in &["use ", "redirect to ", "try "] {
        if let Some(repo) = normalized.strip_prefix(prefix) {
            let repo = repo.trim();
            if !repo.is_empty() {
                return ApprovalDecision::Redirect {
                    repo_name: repo.to_string(),
                };
            }
        }
    }

    match normalized.as_str() {
        "yes" | "y" | "approve" | "ok" | "sure" | "go ahead" | "lgtm" | "yep" | "yeah"
        | "proceed" => ApprovalDecision::Approved,
        "no" | "n" | "skip" | "deny" | "reject" | "nope" | "nah" | "stop" | "pass" => {
            ApprovalDecision::Denied
        }
        _ => ApprovalDecision::Unrecognized,
    }
}

/// Tracks which issues are currently being processed, with O(1) per-source count lookups.
struct ProcessingState {
    keys: HashSet<String>,
    source_counts: HashMap<String, usize>,
    qa_source_counts: HashMap<String, usize>,
    qa_keys: HashSet<String>,
}

impl ProcessingState {
    fn new() -> Self {
        Self {
            keys: HashSet::new(),
            source_counts: HashMap::new(),
            qa_source_counts: HashMap::new(),
            qa_keys: HashSet::new(),
        }
    }

    /// Insert a processing key. Returns `true` if the key was newly inserted.
    fn insert(&mut self, key: String) -> bool {
        if self.keys.insert(key.clone()) {
            let source = source_from_processing_key(&key).to_string();
            *self.source_counts.entry(source).or_insert(0) += 1;
            true
        } else {
            false
        }
    }

    /// Remove a processing key. Returns `true` if the key was present.
    fn remove(&mut self, key: &str) -> bool {
        if self.keys.remove(key) {
            let source = source_from_processing_key(key).to_string();
            if self.qa_keys.remove(key) {
                if let Some(count) = self.qa_source_counts.get_mut(&source) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        self.qa_source_counts.remove(&source);
                    }
                }
            } else if let Some(count) = self.source_counts.get_mut(&source) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.source_counts.remove(&source);
                }
            }
            true
        } else {
            false
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.keys.contains(key)
    }

    /// O(1) count of active processing items for a given source.
    fn source_count(&self, source: &str) -> usize {
        self.source_counts.get(source).copied().unwrap_or(0)
    }

    fn insert_qa(&mut self, key: String) -> bool {
        if self.keys.insert(key.clone()) {
            let source = source_from_processing_key(&key).to_string();
            *self.qa_source_counts.entry(source).or_insert(0) += 1;
            self.qa_keys.insert(key);
            true
        } else {
            false
        }
    }

    fn qa_source_count(&self, source: &str) -> usize {
        self.qa_source_counts.get(source).copied().unwrap_or(0)
    }

    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.keys.len()
    }

    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// A dispatched `deploy_qa` tip's hold on its `qa.max_concurrent` slot,
/// released on drop so a run that panics or is aborted frees the slot instead
/// of holding it until restart.
struct DeployQaTipClaim {
    watcher: Arc<Watcher>,
    issue_id: String,
}

impl Drop for DeployQaTipClaim {
    fn drop(&mut self) {
        self.watcher
            .lock_dispatched_deploy_qa_tips()
            .remove(&self.issue_id);
    }
}

/// An issue's hold on its processing key, its concurrency slot and the
/// shutdown drain, released on drop so a run that panics or is aborted frees
/// them instead of refusing the issue as in-flight, consuming the slot until
/// restart and stalling shutdown.
struct ProcessingClaim<'a> {
    watcher: &'a Watcher,
    key: String,
}

impl Drop for ProcessingClaim<'_> {
    fn drop(&mut self) {
        self.watcher.lock_processing().remove(&self.key);
        self.watcher
            .active_processing
            .fetch_sub(1, Ordering::SeqCst);
        self.watcher.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.watcher.slot_available.notify_waiters();
    }
}

/// A retry's or housekeeping run's hold on the shutdown drain, released on
/// drop. It takes no processing slot, so [`Watcher::active_count`] leaves it
/// out.
///
/// Take it before checking whether the watcher stopped: [`Watcher::stop`]
/// sets `stopped` and clears `is_running` before the drain reads
/// [`Watcher::in_flight`], so either the check sees the stop or the drain sees
/// the claim.
struct RunClaim<'a> {
    watcher: &'a Watcher,
}

impl Drop for RunClaim<'_> {
    fn drop(&mut self) {
        self.watcher.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.watcher.slot_available.notify_waiters();
    }
}

/// Options for creating a watcher.
pub struct WatcherOptions {
    pub config: Config,
    pub sources: Vec<Arc<dyn IssueSource>>,
    pub notifier: Arc<dyn Notifier>,
    pub tracker: Arc<dyn FixAttemptTracker>,
    pub inferrer: Option<RepoInferrer>,
    pub embedding_client: Option<Arc<claudear_analysis::feedback::EmbeddingClient>>,
    pub review_watcher: Option<Arc<ReviewWatcher>>,
    pub issue_embedding_service: Option<Arc<IssueEmbeddingService>>,
    pub code_search_service: Option<Arc<claudear_analysis::repo::code_index::CodeSearchService>>,
    pub discord_search_service: Option<Arc<claudear_analysis::knowledgebase::DiscordSearchService>>,
    pub discord_index_orchestrator: Option<Arc<crate::discord_index::DiscordIndexOrchestrator>>,
    pub relationships: Option<RepoRelationships>,
    pub github_client: Option<Arc<GitHubClient>>,
    /// Generic SCM provider for PR status checking (GitLab, etc.).
    /// When set, this is used for merge detection instead of github_client.
    pub scm_provider: Option<Arc<dyn ScmProvider>>,
    pub user_registry: UserRegistry,
    pub agent: Arc<dyn AgentRunner>,
    /// Optional separate agent runner for classification (intent + repo default).
    /// Falls back to `agent` if not set.
    pub classification_agent: Option<Arc<dyn AgentRunner>>,
    /// Optional runner for repository classification specifically (uses `repo_model`).
    /// Falls back to `classification_agent`, then `agent`, when not set.
    pub repo_classification_agent: Option<Arc<dyn AgentRunner>>,
    /// Optional runner for answering questions (uses `qa_model`).
    /// Falls back to `agent` when not set.
    pub qa_agent: Option<Arc<dyn AgentRunner>>,
    pub dry_run: bool,
    /// Optional pre-loaded LLM engine for repo classification.
    pub llm_engine: Option<Arc<claudear_integrations::chat::llm::LlmEngine>>,
}

/// Main watcher that coordinates sources, Claude, and notifications.
pub struct Watcher {
    config: Config,
    sources: Vec<Arc<dyn IssueSource>>,
    notifier: Arc<dyn Notifier>,
    tracker: Arc<dyn FixAttemptTracker>,
    inferrer: Option<RepoInferrer>,
    embedding_client: Option<Arc<claudear_analysis::feedback::EmbeddingClient>>,
    review_watcher: Option<Arc<ReviewWatcher>>,
    issue_embedding_service: Option<Arc<IssueEmbeddingService>>,
    code_search_service: Option<Arc<claudear_analysis::repo::code_index::CodeSearchService>>,
    discord_search_service: Option<Arc<claudear_analysis::knowledgebase::DiscordSearchService>>,
    discord_index_orchestrator: Option<Arc<crate::discord_index::DiscordIndexOrchestrator>>,
    relationships: Option<RepoRelationships>,
    github_client: Option<Arc<GitHubClient>>,
    scm_provider: Option<Arc<dyn ScmProvider>>,
    user_registry: UserRegistry,
    agent: Arc<dyn AgentRunner>,
    /// Optional runner for answering questions (uses `qa_model`).
    /// Falls back to `agent` when not set.
    qa_agent: Option<Arc<dyn AgentRunner>>,
    dry_run: bool,
    is_running: AtomicBool,
    /// Set for good by [`Self::stop`] before it clears `is_running`, so a
    /// concurrent [`Self::mark_running`] cannot leave a stopped watcher running.
    stopped: AtomicBool,
    /// Keys of the issues being processed. Each key is held by a
    /// [`ProcessingClaim`], whose `Drop` needs a synchronous lock.
    processing: Mutex<ProcessingState>,
    active_processing: AtomicUsize,
    /// Runs the shutdown drain waits for, one per [`ProcessingClaim`] and
    /// [`RunClaim`].
    in_flight: AtomicUsize,
    /// Feedback analyzer for learning from past outcomes
    feedback_analyzer: tokio::sync::Mutex<FeedbackAnalyzer>,
    /// Last seen release tag per upstream repo (for release-triggered cascades).
    last_seen_releases: RwLock<HashMap<String, String>>,
    /// Per-provider rate-limit pause times (clears on restart).
    rate_limit_pause_until: RwLock<HashMap<String, DateTime<Utc>>>,
    /// Notifies waiters when a processing slot becomes available.
    slot_available: Notify,
    /// Global ceiling on concurrent Claude sessions across every source and
    /// path. Each `process_issue` call holds one permit for its whole lifetime,
    /// so the total number of issues being worked on at once never exceeds
    /// `config.max_concurrent_sessions`, no matter how high per-source
    /// `max_concurrent` limits sum. Per-source limits still apply on top.
    session_limiter: Semaphore,
    /// Optional LLM analyzer for enhanced analysis across the pipeline.
    llm_analyzer: Option<Arc<crate::llm_analyzer::LlmAnalyzerImpl>>,
    /// Intent classifier for QA-vs-fix routing. Backend selected by `qa.use_llm`:
    /// agent-based (Claude Code) by default, local-LLM-based when `qa.use_llm` is
    /// set.
    intent_classifier: Option<Arc<dyn crate::intent::IntentClassifier>>,
    /// Join handles for spawned issue-processing tasks (used by tests to drain).
    spawn_handles: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Issue ids of `deploy_qa` tips whose dispatched runs have not finished,
    /// so overlapping dispatches never start the same tip twice. Each id is
    /// held by a [`DeployQaTipClaim`], whose `Drop` needs a synchronous lock.
    dispatched_deploy_qa_tips: Mutex<HashSet<String>>,
    /// How often this watcher's runs send heartbeats, and how long its orphan
    /// sweeps let a run stay silent before releasing its attempt.
    liveness: Liveness,
    /// URLs of the PRs whose review reruns wait for their repository to be
    /// indexed, so each wait is recorded once rather than every cycle.
    deferred_review_reruns: Mutex<HashSet<String>>,
}

/// A ledger comment carried by a review batch: `(scm_comment_id, comment_kind)`.
type CommentRef = (i64, &'static str);

/// Actionable review feedback grouped for one PR:
/// `(pr_url, feedback_summary, feedback_count, comment_refs)`.
type PrReviewFeedback = (String, String, usize, Vec<CommentRef>);

/// What became of review feedback handed to
/// [`Watcher::process_review_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewOutcome {
    /// The feedback is dealt with: a rerun addressing it started, or the PR
    /// was left to humans.
    Handled,
    /// The rerun waits for the PR's repository to be indexed, so the
    /// feedback stays outstanding without spending any of its retries.
    Deferred,
}

/// What became of the review cycle [charged](Watcher::charge_review_cycle)
/// for a review rerun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewCycleCharge {
    /// The cycle counts toward [`MAX_REVIEW_CYCLES`], so a rerun that never
    /// starts gives it back.
    Charged,
    /// The PR has used [`MAX_REVIEW_CYCLES`], so no rerun may start.
    CapReached,
    /// The cycle could not be counted, so the rerun starts without it.
    Uncounted,
}

impl Watcher {
    /// Create a new watcher.
    pub fn new(options: WatcherOptions) -> Self {
        let feedback_analyzer = FeedbackAnalyzer::new().with_tracker(options.tracker.clone());

        // Wire classifier into inferrer: prefer agent-based when use_agent is set
        let mut inferrer = options.inferrer;
        if options.config.llm.use_agent {
            if let Some(ref mut inf) = inferrer {
                // Repo classification prefers its own model (`repo_model`), then
                // the shared classification runner, then the main agent.
                let classifier_runner = options
                    .repo_classification_agent
                    .clone()
                    .or_else(|| options.classification_agent.clone())
                    .unwrap_or_else(|| options.agent.clone());
                let agent_classifier =
                    crate::agent_classifier::AgentRepoClassifier::new(classifier_runner);
                inf.set_classifier(Arc::new(agent_classifier));
                tracing::info!("Agent-based repo classifier enabled (using configured agent)");
            }
        } else if let (Some(ref mut inf), Some(ref engine)) = (&mut inferrer, &options.llm_engine) {
            let classifier = LlmRepoClassifier::new(engine.clone());
            inf.set_classifier(Arc::new(classifier));
            tracing::info!("LLM repo classifier enabled");
        }

        // Create LLM analyzer for enhanced pipeline analysis
        let llm_analyzer = options.llm_engine.as_ref().map(|engine| {
            tracing::info!("LLM analyzer enabled");
            Arc::new(crate::llm_analyzer::LlmAnalyzerImpl::new(engine.clone()))
        });

        // Intent-classification backend, selected by `qa.use_llm`: local LLM when
        // set, else the coding agent (preferring the cheaper classification agent).
        let intent_classifier: Option<Arc<dyn crate::intent::IntentClassifier>> =
            if options.config.qa.use_llm {
                options.llm_engine.clone().map(|engine| {
                    tracing::info!("Local-LLM intent classifier enabled (qa.use_llm = true)");
                    Arc::new(crate::llm_classifier::LocalLlmIntentClassifier::new(engine))
                        as Arc<dyn crate::intent::IntentClassifier>
                })
            } else {
                let classifier_runner = options
                    .classification_agent
                    .clone()
                    .unwrap_or_else(|| options.agent.clone());
                tracing::info!("Agent-based intent classifier enabled (using configured agent)");
                Some(Arc::new(
                    crate::agent_classifier::AgentIntentClassifier::new(classifier_runner),
                ))
            };

        let session_limit = options.config.max_concurrent_sessions.max(1);

        Self {
            agent: options.agent,
            qa_agent: options.qa_agent,
            config: options.config,
            sources: options.sources,
            notifier: options.notifier,
            tracker: options.tracker,
            inferrer,
            embedding_client: options.embedding_client,
            review_watcher: options.review_watcher,
            issue_embedding_service: options.issue_embedding_service,
            code_search_service: options.code_search_service,
            discord_search_service: options.discord_search_service,
            discord_index_orchestrator: options.discord_index_orchestrator,
            relationships: options.relationships,
            github_client: options.github_client,
            scm_provider: options.scm_provider,
            user_registry: options.user_registry,
            dry_run: options.dry_run,
            is_running: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            processing: Mutex::new(ProcessingState::new()),
            active_processing: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            feedback_analyzer: tokio::sync::Mutex::new(feedback_analyzer),
            last_seen_releases: RwLock::new(HashMap::new()),
            rate_limit_pause_until: RwLock::new(HashMap::new()),
            slot_available: Notify::new(),
            session_limiter: Semaphore::new(session_limit),
            llm_analyzer,
            intent_classifier,
            spawn_handles: tokio::sync::Mutex::new(Vec::new()),
            dispatched_deploy_qa_tips: Mutex::new(HashSet::new()),
            liveness: Liveness::default(),
            deferred_review_reruns: Mutex::new(HashSet::new()),
        }
    }

    /// Wait for all spawned issue-processing tasks to complete.
    ///
    /// Primarily useful in tests that need to assert on processing outcomes
    /// after a non-blocking `poll_source` call.
    pub async fn drain_spawned_tasks(&self) {
        let handles: Vec<_> = {
            let mut guard = self.spawn_handles.lock().await;
            std::mem::take(&mut *guard)
        };
        for handle in handles {
            let _ = handle.await;
        }
    }

    /// Get a trait-object reference to the LLM analyzer, if available.
    fn llm(&self) -> Option<&dyn claudear_analysis::llm::LlmAnalyzer> {
        self.llm_analyzer
            .as_deref()
            .map(|a| a as &dyn claudear_analysis::llm::LlmAnalyzer)
    }

    /// Send a cron check-in to Sentry's HTTP API (fire-and-forget).
    ///
    /// Parses the DSN from SENTRY_DSN env var and sends a check-in for the
    /// "claudear-watcher-poll" monitor. Does nothing if SENTRY_DSN is not set.
    pub fn send_cron_check_in(
        &self,
        status: &str,
        check_in_id: &str,
        duration: Option<f64>,
        poll_interval_ms: u64,
    ) {
        let dsn = match std::env::var("CLAUDEAR_SENTRY_DSN") {
            Ok(d) if !d.is_empty() => d,
            _ => return,
        };

        // Parse DSN: https://<public_key>@<host>/<project_id>
        let parsed = match url::Url::parse(&dsn) {
            Ok(u) => u,
            Err(_) => return,
        };
        let public_key = parsed.username();
        if public_key.is_empty() {
            return;
        }
        let project_id = parsed.path().trim_start_matches('/');
        if project_id.is_empty() {
            return;
        }
        let ingest = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));

        let environment = std::env::var("CLAUDEAR_SENTRY_ENVIRONMENT").unwrap_or_default();
        let interval_minutes = (poll_interval_ms / 60_000).max(1);

        let mut url = format!(
            "{}/api/{}/cron/claudear-watcher-poll/{}/?status={}&check_in_id={}",
            ingest, project_id, public_key, status, check_in_id,
        );
        if !environment.is_empty() {
            url.push_str(&format!("&environment={}", environment));
        }
        if let Some(d) = duration {
            url.push_str(&format!("&duration={:.1}", d));
        }

        // Include monitor_config for upsert on in_progress check-ins
        let body = if status == "in_progress" {
            Some(serde_json::json!({
                "monitor_config": {
                    "schedule": {
                        "type": "interval",
                        "value": interval_minutes,
                        "unit": "minute"
                    },
                    "checkin_margin": 5,
                    "max_runtime": 30
                }
            }))
        } else {
            None
        };

        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let req = if let Some(body) = body {
                client.post(&url).json(&body)
            } else {
                client.get(&url)
            };
            if let Err(e) = req.send().await {
                tracing::debug!(error = %e, "Failed to send Sentry cron check-in");
            }
        });
    }

    /// Build a repository inferrer from config.
    ///
    /// This uses the fallback mechanism: if `auto_discover_paths` is configured,
    /// it scans the local filesystem. Otherwise, if a GitHub token is configured,
    /// it fetches repos via the GitHub API.
    pub async fn build_inferrer(
        config: &Config,
        github_client: Option<&claudear_integrations::github::GitHubClient>,
        tracker: Option<&dyn FixAttemptTracker>,
    ) -> Result<Option<RepoInferrer>> {
        if config.known_orgs.is_empty() {
            tracing::info!("No known_orgs configured, inference disabled");
            return Ok(None);
        }

        // Check if we have any discovery method available
        let has_local_paths = !config.auto_discover_paths.is_empty();
        let has_github_client = github_client.map(|c| c.is_enabled()).unwrap_or(false);

        if !has_local_paths && !has_github_client {
            tracing::info!(
                "No auto_discover_paths configured and no GitHub token available, inference disabled"
            );
            return Ok(None);
        }

        let mut index = build_repo_index_with_fallback(
            &config.known_orgs,
            &config.auto_discover_paths,
            github_client,
            None, // gitlab_provider
            &[],  // gitlab_groups
            &config.workspace,
            config.github().use_ssh,
        )
        .await?;

        if index.is_empty() {
            tracing::warn!("Repository index is empty, no repos discovered");
            return Ok(None);
        }

        // Load known repository renames so vendor-path inference can resolve
        // old package names (e.g. utopia-php/framework) to the current repo.
        if let Some(t) = tracker {
            match t.get_all_repo_aliases() {
                Ok(aliases) => {
                    let count = aliases.len();
                    for (former, current) in aliases {
                        index.add_alias(&former, &current);
                    }
                    if count > 0 {
                        tracing::debug!(count, "Loaded repository rename aliases into index");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "Failed to load repo aliases"),
            }
        }

        tracing::info!(
            repos = index.len(),
            files = index.total_files(),
            "Repository index built for inference"
        );

        Ok(Some(RepoInferrer::new(index)))
    }

    /// Build a repository inferrer with embeddings for semantic matching.
    ///
    /// This uses the fallback mechanism: if `auto_discover_paths` is configured,
    /// it scans the local filesystem. Otherwise, if a GitHub token is configured,
    /// it fetches repos via the GitHub API.
    pub async fn build_inferrer_with_embeddings(
        config: &Config,
        github_client: Option<&claudear_integrations::github::GitHubClient>,
        tracker: Option<&dyn FixAttemptTracker>,
    ) -> Result<(
        Option<RepoInferrer>,
        Option<Arc<claudear_analysis::feedback::EmbeddingClient>>,
    )> {
        use claudear_analysis::feedback::{EmbeddingClient, EmbeddingConfig};
        use claudear_analysis::inference::build_repo_embeddings;

        if config.known_orgs.is_empty() {
            tracing::info!("No known_orgs configured, inference disabled");
            return Ok((None, None));
        }

        // Check if we have any discovery method available
        let has_local_paths = !config.auto_discover_paths.is_empty();
        let has_github_client = github_client.map(|c| c.is_enabled()).unwrap_or(false);

        if !has_local_paths && !has_github_client {
            tracing::info!(
                "No auto_discover_paths configured and no GitHub token available, inference disabled"
            );
            return Ok((None, None));
        }

        let mut index = build_repo_index_with_fallback(
            &config.known_orgs,
            &config.auto_discover_paths,
            github_client,
            None, // gitlab_provider
            &[],  // gitlab_groups
            &config.workspace,
            config.github().use_ssh,
        )
        .await?;

        if index.is_empty() {
            tracing::warn!("Repository index is empty, no repos discovered");
            return Ok((None, None));
        }

        if let Some(t) = tracker {
            match t.get_all_repo_aliases() {
                Ok(aliases) => {
                    let count = aliases.len();
                    for (former, current) in aliases {
                        index.add_alias(&former, &current);
                    }
                    if count > 0 {
                        tracing::debug!(count, "Loaded repository rename aliases into index");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "Failed to load repo aliases"),
            }
        }

        tracing::info!(
            repos = index.len(),
            files = index.total_files(),
            "Repository index built for inference"
        );

        // Build execution providers from config
        #[allow(unused_mut)]
        let mut execution_providers = Vec::new();
        if config.embedding.gpu {
            #[cfg(feature = "cuda")]
            {
                let cuda_ep = ort::execution_providers::CUDA::default()
                    .with_device_id(config.embedding.device_id)
                    .build();
                execution_providers.push(cuda_ep);
                tracing::info!(
                    device_id = config.embedding.device_id,
                    "CUDA execution provider configured for embeddings"
                );
            }
            #[cfg(not(feature = "cuda"))]
            {
                tracing::warn!(
                    "embedding.gpu = true but binary was compiled without --features cuda; falling back to CPU"
                );
            }
        }

        let emb_pool_size = if config.embedding.pool_size > 0 {
            config.embedding.pool_size as usize
        } else if config.embedding.gpu {
            1 // GPU: default to single instance to avoid wasting VRAM
        } else {
            0 // 0 triggers auto-detection in EmbeddingConfig::default()
        };

        let emb_config = EmbeddingConfig {
            pool_size: if emb_pool_size > 0 {
                emb_pool_size
            } else {
                EmbeddingConfig::default().pool_size
            },
            execution_providers,
            sub_batch_size: config.embedding.sub_batch_size as usize,
            ..EmbeddingConfig::default()
        };

        // Try to initialize embedding client
        match EmbeddingClient::new(emb_config) {
            Ok(client) => {
                // Build embeddings for all repos
                match build_repo_embeddings(&index, &client).await {
                    Ok(embeddings) => {
                        tracing::info!(
                            "Semantic inference enabled with {} repo embeddings",
                            embeddings.len()
                        );
                        // Use with_discovery to enable incremental updates
                        let inferrer = RepoInferrer::with_discovery(
                            index,
                            embeddings,
                            config.known_orgs.clone(),
                            config.auto_discover_paths.clone(),
                        );
                        Ok((Some(inferrer), Some(Arc::new(client))))
                    }
                    Err(e) => {
                        tracing::warn!("Failed to build repo embeddings: {}, falling back to file-based inference", e);
                        Ok((Some(RepoInferrer::new(index)), None))
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to initialize embedding client: {}, using file-based inference only",
                    e
                );
                Ok((Some(RepoInferrer::new(index)), None))
            }
        }
    }

    // Repository resolution is now handled by the inference engine (RepoInferrer).
    // See src/inference/mod.rs for the new implementation.

    /// Refresh the repo index and embed any new repositories.
    ///
    /// Returns the number of new repos discovered and embedded.
    pub async fn refresh_repos(&self) -> Result<usize> {
        let (inferrer, client) = match (&self.inferrer, &self.embedding_client) {
            (Some(inf), Some(cli)) => (inf, cli),
            _ => return Ok(0),
        };

        inferrer.refresh_repos(client).await
    }

    /// Discover dependencies between indexed repos and save them to the database.
    pub async fn discover_dependencies(&self) {
        let inferrer = match &self.inferrer {
            Some(inf) => inf,
            None => return,
        };

        let known_orgs = self.config.known_orgs.clone();
        if known_orgs.is_empty() {
            return;
        }

        let repo_paths: Vec<String> = match inferrer.with_index(|index| {
            Ok(index
                .list()
                .iter()
                .map(|r| r.path.to_string_lossy().to_string())
                .collect())
        }) {
            Ok(paths) => paths,
            Err(e) => {
                tracing::warn!("Failed to get repo paths for dependency discovery: {}", e);
                return;
            }
        };

        if repo_paths.is_empty() {
            return;
        }

        let tracker = self.tracker.clone();
        let result = tokio::task::spawn_blocking(move || -> claudear_core::error::Result<usize> {
            let discovery = claudear_analysis::repo::DependencyDiscovery::new(known_orgs);
            let discovered = discovery.scan_directories(&repo_paths)?;
            let mut count = 0;
            for dep in &discovered {
                if let Err(e) = tracker.add_dependency(&dep.depends_on, &dep.repo, &dep.dep_type) {
                    tracing::warn!(
                        error = %e,
                        upstream = %dep.depends_on,
                        downstream = %dep.repo,
                        "Failed to save dependency"
                    );
                } else {
                    count += 1;
                }
            }
            Ok(count)
        })
        .await;

        match result {
            Ok(Ok(count)) if count > 0 => {
                tracing::info!("Discovered and saved {} dependencies", count);
            }
            Ok(Err(e)) => {
                tracing::warn!("Dependency discovery failed: {}", e);
            }
            Err(e) => {
                tracing::warn!("Dependency discovery task panicked: {}", e);
            }
            _ => {}
        }
    }

    /// Sync repository index to the database.
    ///
    /// Updates repository paths and optionally file lists in the database
    /// from the in-memory RepoIndex.
    pub fn sync_repos_to_db(&self, sync_files: bool) -> Result<usize> {
        let inferrer = match &self.inferrer {
            Some(inf) => inf,
            None => return Ok(0),
        };

        inferrer.with_index(|index| self.tracker.sync_from_index(index, sync_files))
    }

    /// Incrementally re-index a single repository's code after a fetch.
    ///
    /// Uses file-content hashing so only changed files are re-parsed and re-embedded.
    /// No-ops when code indexing is disabled or the embedding client is unavailable.
    async fn reindex_repo(&self, repo_name: &str, repo_path: &std::path::Path) {
        if !self.config.code_index.enabled {
            return;
        }
        let emb_client = match self.embedding_client {
            Some(ref c) => c.clone(),
            None => return,
        };
        let code_indexer = claudear_analysis::repo::code_index::CodeIndexer::with_config(
            self.tracker.clone(),
            emb_client,
            self.config.code_index.max_file_size_kb,
            self.config.code_index.batch_size,
        );
        match code_indexer.index_repo(repo_name, repo_path).await {
            Ok(stats) => {
                if stats.files_processed > 0 {
                    tracing::info!(
                        repo = %repo_name,
                        files = stats.files_processed,
                        chunks = stats.chunks_created,
                        "Re-indexed repo after fetch"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(repo = %repo_name, error = %e, "Failed to re-index repo after fetch");
            }
        }
    }

    /// Pull (fetch) and re-index all known repositories.
    ///
    /// Iterates through every repo in the index, runs `git fetch origin`, then
    /// incrementally re-indexes changed files.  Called periodically by the
    /// housekeeping worker based on `code_index.reindex_interval_hours`.
    pub async fn pull_and_reindex_all_repos(&self) {
        let inferrer = match &self.inferrer {
            Some(inf) => inf,
            None => return,
        };

        let repos: Vec<(String, std::path::PathBuf, String)> = inferrer
            .with_index(|index| {
                Ok(index
                    .list()
                    .into_iter()
                    .filter(|r| r.path.exists())
                    .map(|r| (r.name.clone(), r.path.clone(), r.scm_url.clone()))
                    .collect())
            })
            .unwrap_or_default();

        if repos.is_empty() {
            return;
        }

        tracing::info!(
            count = repos.len(),
            "Pulling and re-indexing all repositories"
        );

        for (name, path, scm_url) in &repos {
            match GitOps::ensure_repo_synced(path, scm_url).await {
                Ok(default_branch) => {
                    tracing::debug!(repo = %name, default_branch = %default_branch, "Fetched repo");
                }
                Err(e) => {
                    tracing::warn!(repo = %name, error = %e, "Failed to fetch repo during periodic reindex");
                    continue;
                }
            }
            self.reindex_repo(name, path).await;
        }

        tracing::info!("Periodic pull and re-index complete");
    }

    /// Warm-start: clone repos, sync to DB, index code, and load feedback outcomes.
    ///
    /// This is called at the beginning of `start()` and can also be used independently
    /// by the `HousekeepingWorker` to prepare the watcher for background tasks.
    pub async fn warm_start(&self) -> Result<()> {
        // Clone any API-discovered repos that aren't local yet
        if let Some(inferrer) = &self.inferrer {
            let parallelism = std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(4);
            match inferrer.clone_and_index_all(parallelism).await {
                Ok(0) => {} // No repos to clone
                Ok(n) => tracing::info!("Cloned and indexed {} repositories", n),
                Err(e) => tracing::warn!("Error cloning repositories: {}", e),
            }
        }

        // Sync repository index to database (includes file lists)
        // Use spawn_blocking since sync_repos_to_db performs blocking I/O
        let inferrer = self.inferrer.clone();
        let tracker = self.tracker.clone();
        let sync_result =
            tokio::task::spawn_blocking(move || -> claudear_core::error::Result<usize> {
                let inferrer = match &inferrer {
                    Some(inf) => inf,
                    None => return Ok(0),
                };
                inferrer.with_index(|index| tracker.sync_from_index(index, true))
            })
            .await;

        match sync_result {
            Ok(Ok(count)) if count > 0 => {
                tracing::info!("Synced {} repositories to database", count);
            }
            Ok(Err(e)) => {
                tracing::warn!("Failed to sync repos to database: {}", e);
            }
            Err(e) => {
                tracing::warn!("Sync task panicked: {}", e);
            }
            _ => {}
        }

        // Discover dependencies between indexed repos
        self.discover_dependencies().await;

        // Tree-sitter code indexing for all repos on disk
        if self.config.code_index.enabled {
            if let Some(inferrer) = &self.inferrer {
                if let Some(ref emb_client) = self.embedding_client {
                    {
                        let code_indexer =
                            claudear_analysis::repo::code_index::CodeIndexer::with_config(
                                self.tracker.clone(),
                                emb_client.clone(),
                                self.config.code_index.max_file_size_kb,
                                self.config.code_index.batch_size,
                            );

                        // Collect repos that exist on disk
                        let repos: Vec<(String, std::path::PathBuf)> = inferrer
                            .with_index(|index| {
                                Ok(index
                                    .list()
                                    .into_iter()
                                    .filter(|r| r.path.exists())
                                    .map(|r| (r.name.clone(), r.path.clone()))
                                    .collect())
                            })
                            .unwrap_or_default();

                        if !repos.is_empty() {
                            tracing::info!(
                                count = repos.len(),
                                "Starting code indexing for repositories"
                            );
                            let _ = self.tracker.start_indexing_progress(repos.len());
                            let mut total_chunks = 0usize;
                            let mut total_indexed = 0usize;
                            for (name, path) in &repos {
                                let _ = self.tracker.update_indexing_progress(
                                    total_indexed,
                                    name,
                                    0,
                                    total_chunks,
                                );
                                match code_indexer.index_repo(name, path).await {
                                    Ok(stats) => {
                                        total_chunks += stats.chunks_created;
                                        if stats.files_processed > 0 {
                                            total_indexed += 1;
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(repo = %name, error = %e, "Failed to index repo code");
                                    }
                                }
                            }
                            let _ = self.tracker.finish_indexing_progress();
                            tracing::info!(
                                repos = total_indexed,
                                chunks = total_chunks,
                                "Code indexing complete"
                            );
                        }
                    }
                } else {
                    tracing::warn!("Embedding client not available for code indexing");
                }
            }
        }

        // Discord knowledge-source indexing (channels/threads -> embeddings)
        self.reindex_discord_knowledgebase().await;

        // Load feedback outcomes from DB for learning
        match self.tracker.get_feedback_outcomes(None, 1000) {
            Ok(outcomes) if !outcomes.is_empty() => {
                let count = outcomes.len();
                let mut analyzer = self.feedback_analyzer.lock().await;
                analyzer.load_outcomes(outcomes);
                tracing::info!(count = count, "Loaded feedback outcomes for learning");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "Failed to load feedback outcomes"),
        }

        Ok(())
    }

    /// Set the running state of the watcher, even after [`Self::stop`].
    #[cfg(test)]
    pub fn set_running(&self, running: bool) {
        self.is_running.store(running, Ordering::SeqCst);
    }

    /// Mark the watcher as running, unless it has been stopped.
    ///
    /// Returns `false`, leaving the watcher stopped, when [`Self::stop`] came
    /// first, such as a stop requested while warm start was still running.
    pub(crate) fn mark_running(&self) -> bool {
        self.is_running.store(true, Ordering::SeqCst);
        if self.is_stopped() {
            self.is_running.store(false, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// Whether [`Self::stop`] has been called. Unlike a watcher that is not
    /// [running](Self::is_running), one that was never started is not stopped.
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Release attempts whose runs were orphaned by a crash, restart or
    /// shutdown, which otherwise stay `pending` and block their issues from
    /// ever being picked or retried again.
    ///
    /// A daemon, a foreground `poll`, one-shot triggers and the webhook server
    /// can all be running attempts against the same database, and every run
    /// keeps its attempt's [`Heartbeat`] going until it ends. Only attempts
    /// silent for longer than [`Liveness::stale_after`] are released, so a
    /// live run is never swept however long it waits on approval, a question
    /// or its agent, and a run that died is recovered within minutes. This
    /// runs on start and every housekeeping cycle.
    fn release_orphaned_attempts(&self) {
        let stale_after = self.liveness.stale_after;
        match self.tracker.release_orphaned_pending_attempts(stale_after) {
            Ok(0) => {}
            Ok(released) => tracing::info!(
                component = "watcher",
                released,
                stale_after_secs = stale_after.as_secs(),
                "Released orphaned pending attempts"
            ),
            Err(e) => {
                tracing::warn!(component = "watcher", error = %e, "Failed to release orphaned attempts")
            }
        }
    }

    /// Start the watcher with polling.
    ///
    /// Returns once both the polling and housekeeping loops have stopped, so a
    /// housekeeping cycle still running at [`Self::stop`] finishes first. The
    /// loops log their failures and carry on, so only warm start and the
    /// initial poll can fail.
    pub async fn start(self: &Arc<Self>, interval_ms: Option<u64>) -> Result<()> {
        self.clear_rate_limit_pause().await;

        if !self.dry_run {
            self.release_orphaned_attempts();
        }

        let configured_poll_interval = interval_ms.unwrap_or(self.config.poll_interval_ms);
        let poll_interval = configured_poll_interval.max(1000);
        if configured_poll_interval < 1000 {
            tracing::warn!(
                component = "watcher",
                configured = configured_poll_interval,
                clamped = poll_interval,
                "Poll interval below 1000ms, clamping to 1000ms to avoid busy-loop"
            );
        }

        tracing::info!("");
        tracing::info!(
            "Starting Claude Watcher{}",
            if self.dry_run { " (DRY RUN)" } else { "" }
        );
        tracing::info!("  Workspace: {:?}", self.config.workspace);
        tracing::info!("  Known orgs: {}", self.config.known_orgs.len());
        tracing::info!("  Poll interval: {}ms (global)", poll_interval);
        tracing::info!(
            "  Max issues per cycle: {} (global)",
            self.config.max_issues_per_cycle
        );
        tracing::info!("  Max concurrent: {} (global)", self.config.max_concurrent);
        tracing::info!(
            "  Max concurrent sessions: {} (global ceiling)",
            self.config.max_concurrent_sessions
        );
        for source in &self.sources {
            let src_max_issues = self.config.max_issues_per_cycle_for(source.name());
            let src_max_concurrent = self.config.max_concurrent_for(source.name());
            let src_poll_interval = self.config.poll_interval_ms_for(source.name());
            if src_max_issues != self.config.max_issues_per_cycle
                || src_max_concurrent != self.config.max_concurrent
                || src_poll_interval != poll_interval
            {
                tracing::info!(
                    "    {}: poll_interval={}ms, max_issues={}, max_concurrent={}",
                    source.name(),
                    src_poll_interval,
                    src_max_issues,
                    src_max_concurrent
                );
            }
        }
        tracing::info!("  Processing delay: {}ms", self.config.processing_delay_ms);
        tracing::info!(
            "  Sources: {}",
            self.sources
                .iter()
                .map(|s| s.display_name())
                .collect::<Vec<_>>()
                .join(", ")
        );

        if self.config.cascade.enabled {
            tracing::info!("  Cascade: enabled");
            if self.config.cascade.max_depth > 0 {
                tracing::info!("    Max depth: {}", self.config.cascade.max_depth);
            } else {
                tracing::info!("    Max depth: unlimited");
            }
            if let Some(ref rels) = self.relationships {
                let repo_count = rels.list_repositories().len();
                tracing::info!("    Repos in dependency graph: {}", repo_count);
            }
        } else {
            tracing::info!("  Cascade: disabled");
        }

        if self.dry_run {
            tracing::warn!("");
            tracing::warn!("  DRY RUN MODE - No issues will be processed");
        }

        tracing::info!("");

        self.warm_start().await?;
        if !self.mark_running() {
            return Ok(());
        }

        self.poll().await?;

        let worker = crate::housekeeping::HousekeepingWorker::new(Arc::clone(self), poll_interval);
        tokio::join!(self.run_source_poll_loop(poll_interval), worker.run_loop());
        Ok(())
    }

    /// Run the source polling loop until the watcher stops.
    ///
    /// Each source gets its own long-lived worker task that polls that source
    /// at its configured interval, independently of the others. A slow source
    /// can no longer delay a fast one, and each source's own `max_concurrent`
    /// budget (plus the global session ceiling) bounds how many fixes it runs.
    /// Housekeeping is handled separately by
    /// [`HousekeepingWorker`](crate::housekeeping::HousekeepingWorker).
    async fn run_source_poll_loop(self: &Arc<Self>, poll_interval: u64) {
        let min_period_ms = MIN_SOURCE_POLL_PERIOD.as_millis() as u64;
        let mut workers = Vec::with_capacity(self.sources.len());
        for (idx, source) in self.sources.iter().enumerate() {
            let src_interval = self
                .config
                .poll_interval_ms_for(source.name())
                .max(min_period_ms);
            tracing::info!(
                source = source.name(),
                interval_ms = src_interval,
                max_concurrent = self.config.max_concurrent_for(source.name()),
                "Starting source worker"
            );
            let watcher = Arc::clone(self);
            workers.push(tokio::spawn(async move {
                watcher.run_source_worker(idx, src_interval).await;
            }));
        }

        // Nothing configured: idle until stopped so the caller's join! still
        // has a future to hold.
        if workers.is_empty() {
            let _ = poll_interval;
            while self.is_running.load(Ordering::SeqCst) {
                tokio::time::sleep(MIN_SOURCE_POLL_PERIOD).await;
            }
            return;
        }

        for worker in workers {
            let _ = worker.await;
        }
    }

    /// Worker loop for a single source: poll it every `interval_ms` until the
    /// watcher stops. The first poll fires one interval after start (the
    /// initial fan-out poll already ran in [`Self::start`]).
    ///
    /// The timer ticks on a fixed [`MIN_SOURCE_POLL_PERIOD`] granularity so a
    /// stop is noticed promptly even when `interval_ms` is large; the source is
    /// actually polled only once its interval has elapsed, and the clock is
    /// reset *after* the poll completes so a slow poll (slow API, waiting for
    /// dispatch slots) never triggers a burst of catch-up polls.
    async fn run_source_worker(self: Arc<Self>, source_idx: usize, interval_ms: u64) {
        let source_interval = Duration::from_millis(interval_ms);
        let mut timer = interval(MIN_SOURCE_POLL_PERIOD);
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        timer.tick().await; // consume the immediate first tick
        let mut last_poll = std::time::Instant::now();

        while self.is_running.load(Ordering::SeqCst) {
            timer.tick().await;
            if !self.is_running.load(Ordering::SeqCst) {
                break;
            }
            if self.is_rate_limit_paused().await {
                continue;
            }
            if last_poll.elapsed() < source_interval {
                continue;
            }
            let source = &self.sources[source_idx];
            if let Err(e) = self.poll_source(source).await {
                tracing::error!(
                    component = "watcher",
                    source = source.name(),
                    error = %e,
                    "Error polling source"
                );
            }
            last_poll = std::time::Instant::now();
        }
    }

    /// Stop the watcher for good.
    ///
    /// Stops taking new work: the polling and housekeeping loops exit after
    /// their current cycle, and a `start` still warming up returns without
    /// polling. Runs already in flight carry on; [`Self::stop_and_drain`] waits
    /// for them. Safe to call more than once.
    pub fn stop(&self) {
        let already_stopped = self.stopped.swap(true, Ordering::SeqCst);
        self.is_running.store(false, Ordering::SeqCst);
        if already_stopped {
            return;
        }
        tracing::info!(
            in_flight = self.in_flight(),
            "Stopping Claude Watcher, waiting for active tasks to complete..."
        );
        // Wake any tasks blocked on slot_available so they re-check is_running and exit.
        self.slot_available.notify_waiters();
    }

    /// Stop the watcher and wait up to [`DRAIN_TIMEOUT`] for in-flight runs to
    /// finish.
    ///
    /// Returns `true` once no run is [in flight](Self::in_flight), or `false`
    /// when the timeout elapses first.
    pub async fn stop_and_drain(&self) -> bool {
        self.stop();

        if tokio::time::timeout(DRAIN_TIMEOUT, self.wait_until_idle())
            .await
            .is_ok()
        {
            tracing::info!("Claude Watcher stopped gracefully");
            return true;
        }
        tracing::warn!(
            remaining = self.in_flight(),
            "Graceful shutdown timeout reached, some tasks may not have completed"
        );
        false
    }

    /// Wait until no run is [in flight](Self::in_flight), waking on each
    /// release and re-checking at a short interval in case a wake-up is missed.
    pub async fn wait_until_idle(&self) {
        let mut reported = 0;
        loop {
            let released = self.next_slot_release();
            let in_flight = self.in_flight();
            if in_flight == 0 {
                return;
            }
            if in_flight != reported {
                tracing::info!(in_flight, "Waiting for active tasks to complete...");
                reported = in_flight;
            }
            let _ = tokio::time::timeout(DRAIN_RECHECK_INTERVAL, released).await;
        }
    }

    /// Check if the watcher is currently running.
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::SeqCst)
    }

    /// Number of issues being processed, the `active_processing` metric.
    pub fn active_count(&self) -> usize {
        self.active_processing.load(Ordering::SeqCst)
    }

    /// Number of holds [`Self::stop_and_drain`] waits for: one per issue being
    /// processed, and one per retry, review run, merge follow-up or release
    /// cascade in progress.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Check if the watcher is in dry-run mode.
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Returns the configured periodic reindex interval, or `None` if disabled (0).
    pub fn reindex_interval(&self) -> Option<std::time::Duration> {
        let hours = self.config.code_index.reindex_interval_hours;
        if !self.config.code_index.enabled || hours <= 0.0 {
            return None;
        }
        Some(std::time::Duration::from_secs_f64(hours * 3600.0))
    }

    /// Periodic reindex interval for the Discord knowledge source, or `None`
    /// when the source is absent/disabled or the interval is 0.
    pub fn discord_reindex_interval(&self) -> Option<std::time::Duration> {
        self.discord_index_orchestrator.as_ref()?;
        let cfg = self.config.knowledgebase.discord.as_ref()?;
        if !cfg.enabled || cfg.reindex_interval_hours <= 0.0 {
            return None;
        }
        Some(std::time::Duration::from_secs_f64(
            cfg.reindex_interval_hours * 3600.0,
        ))
    }

    /// Run the Discord knowledge-source indexer once, if configured. No-op when
    /// the orchestrator is absent (source disabled or missing token/guild).
    pub async fn reindex_discord_knowledgebase(&self) {
        let Some(orchestrator) = self.discord_index_orchestrator.as_ref() else {
            return;
        };
        let Some(cfg) = self.config.knowledgebase.discord.as_ref() else {
            return;
        };
        match orchestrator.run(cfg).await {
            Ok(stats) => {
                tracing::info!(%stats, "Discord knowledgebase indexing complete")
            }
            Err(e) => {
                tracing::warn!(error = %e, "Discord knowledgebase indexing failed")
            }
        }
    }

    /// Check for new PR reviews that require action.
    ///
    /// This polls the ReviewWatcher for any new CHANGES_REQUESTED or COMMENTED reviews
    /// and triggers Claude to address the feedback.
    pub async fn check_reviews(&self) -> Result<()> {
        let review_watcher = match &self.review_watcher {
            Some(rw) => rw,
            None => return Ok(()),
        };
        if !self.is_running() {
            return Ok(());
        }

        let events = review_watcher.check_for_reviews().await?;
        for (pr_url, feedback_summary, feedback_count, comment_refs) in
            Self::group_review_feedback_by_pr(events)
        {
            tracing::info!(
                pr_url = %pr_url,
                feedback_count,
                "Review feedback received, processing..."
            );

            if let Some(attempt) = self.tracker.get_attempt_by_pr_url(&pr_url)? {
                if Self::is_terminal_attempt_status(attempt.status) {
                    tracing::info!(
                        pr_url = %pr_url,
                        source = %attempt.source,
                        issue_id = %attempt.issue_id,
                        status = %attempt.status,
                        "Skipping review feedback for terminal attempt status"
                    );
                    // The PR is merged/closed/cannot-fix/declined: close out the
                    // ledger so its comments stop being re-surfaced, then stop
                    // watching.
                    if let Err(e) = self.tracker.mark_pr_review_comments_handled(&pr_url) {
                        tracing::warn!(pr_url = %pr_url, error = %e, "Failed to close review-comment ledger for terminal PR");
                    }
                    review_watcher.unwatch_pr(&pr_url);
                    continue;
                }
                let _claim = self.claim_run();
                if !self.is_running() {
                    break;
                }
                match self
                    .process_review_action(&attempt, &feedback_summary)
                    .await
                {
                    Ok(ReviewOutcome::Handled) => {
                        // Durably handled: acknowledge exactly the comments in this
                        // batch so they aren't re-surfaced, without touching any
                        // comment recorded concurrently while we were processing.
                        if let Err(e) = self
                            .tracker
                            .mark_pr_review_comments_handled_by_ids(&pr_url, &comment_refs)
                        {
                            tracing::warn!(pr_url = %pr_url, error = %e, "Failed to mark review comments handled");
                        }
                    }
                    Ok(ReviewOutcome::Deferred) => {
                        tracing::debug!(
                            pr_url = %pr_url,
                            "Leaving review feedback outstanding until the PR's repository is indexed"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            pr_url = %pr_url,
                            error = %e,
                            "Failed to process review feedback; will retry next cycle"
                        );
                        // Leave the batch's comments unhandled so they retry, but
                        // count the failure so a poison comment eventually gives up.
                        if self.is_running() {
                            if let Err(error) = self.tracker.note_pr_review_comment_failure_by_ids(
                                &pr_url,
                                &comment_refs,
                                MAX_REVIEW_COMMENT_ATTEMPTS,
                            ) {
                                tracing::warn!(pr_url = %pr_url, %error, "Failed to record review-comment failure");
                            }
                        }
                    }
                }
            } else {
                tracing::warn!(
                    pr_url = %pr_url,
                    "Received review for unknown PR, skipping"
                );
                // No attempt to act on; count it as a failure so this batch's
                // uncorrelated comments don't re-surface forever.
                if let Err(e) = self.tracker.note_pr_review_comment_failure_by_ids(
                    &pr_url,
                    &comment_refs,
                    MAX_REVIEW_COMMENT_ATTEMPTS,
                ) {
                    tracing::warn!(pr_url = %pr_url, error = %e, "Failed to record review-comment failure");
                }
            }
        }

        Ok(())
    }

    fn is_terminal_attempt_status(status: FixAttemptStatus) -> bool {
        matches!(
            status,
            FixAttemptStatus::Merged
                | FixAttemptStatus::Closed
                | FixAttemptStatus::CannotFix
                | FixAttemptStatus::Declined
        )
    }

    /// Group actionable review events per PR into (pr_url, feedback_summary,
    /// feedback_count, comment_refs). `comment_refs` are exactly the ledger comments
    /// carried by the batch as `(scm_comment_id, comment_kind)`, so acknowledgement
    /// targets those specific rows rather than the whole PR (which would wrongly
    /// mark a concurrently-recorded comment handled) or a colliding id in the other
    /// namespace.
    fn group_review_feedback_by_pr(events: Vec<ReviewEvent>) -> Vec<PrReviewFeedback> {
        let mut feedback_by_pr: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let mut refs_by_pr: std::collections::HashMap<String, Vec<CommentRef>> =
            std::collections::HashMap::new();
        let mut pr_order: Vec<String> = Vec::new();

        for event in events {
            if !event.requires_action() {
                continue;
            }

            let pr_url = event.pr_url().to_string();
            if !feedback_by_pr.contains_key(&pr_url) {
                pr_order.push(pr_url.clone());
            }
            refs_by_pr
                .entry(pr_url.clone())
                .or_default()
                .extend(event.comment_refs());
            feedback_by_pr
                .entry(pr_url)
                .or_default()
                .push(event.get_feedback_summary());
        }

        pr_order
            .into_iter()
            .filter_map(|pr_url| {
                let refs = refs_by_pr.remove(&pr_url).unwrap_or_default();
                feedback_by_pr.remove(&pr_url).map(|feedbacks| {
                    let count = feedbacks.len();
                    (pr_url, feedbacks.join("\n\n---\n\n"), count, refs)
                })
            })
            .collect()
    }

    /// Process review feedback by triggering Claude to address the feedback.
    ///
    /// This creates a new Claude session with the original issue context plus
    /// the review feedback appended to help Claude understand what to fix. A
    /// rerun that never starts fails without counting toward
    /// [`MAX_REVIEW_CYCLES`], so its feedback is retried instead of being lost
    /// to the cap. A rerun whose PR repository cannot be resolved from the
    /// index is [deferred](ReviewOutcome::Deferred) before anything can
    /// charge the feedback for it, and runs once the repository is indexed.
    ///
    /// A PR that has used the cap is left to humans before any work starts,
    /// and the cap is enforced again when the cycle is charged, since another
    /// process sharing the database may take the PR's last cycle in between.
    async fn process_review_action(
        &self,
        attempt: &claudear_core::types::FixAttempt,
        feedback: &str,
    ) -> Result<ReviewOutcome> {
        tracing::info!(
            source = %attempt.source,
            issue_id = %attempt.issue_id,
            short_id = %attempt.short_id,
            feedback_preview = %feedback.chars().take(100).collect::<String>(),
            "Processing review feedback for issue"
        );

        if let Some(ref pr_url) = attempt.pr_url {
            if let Some(pr_record) = self.review_cycle_record(attempt, pr_url) {
                if pr_record.review_cycles >= MAX_REVIEW_CYCLES {
                    self.leave_pr_to_humans(attempt, pr_url, pr_record.review_cycles);
                    return Ok(ReviewOutcome::Handled);
                }
            }
        }

        if let (Some(pr_url), Some(repo)) = (attempt.pr_url.as_deref(), attempt.scm_repo.as_deref())
        {
            if let RepoResolution::Skip { reason } =
                resolve_repo_for_cascade(self.inferrer.as_ref(), repo)
            {
                self.defer_review_rerun(attempt, pr_url, repo, &reason);
                return Ok(ReviewOutcome::Deferred);
            }
            self.lock_deferred_review_reruns().remove(pr_url);
        }

        if self.config.learning.review_classification {
            if let Some(repo) = &attempt.scm_repo {
                // Parse feedback as review comments for classification
                let mock_comment = claudear_integrations::scm::ReviewComment {
                    id: 0,
                    path: String::new(),
                    position: None,
                    original_position: None,
                    body: feedback.to_string(),
                    user: claudear_integrations::scm::ReviewUser {
                        login: "reviewer".to_string(),
                        id: 0,
                        user_type: None,
                    },
                    created_at: String::new(),
                    updated_at: String::new(),
                    html_url: String::new(),
                    pull_request_review_id: None,
                    start_line: None,
                    line: None,
                    side: None,
                };

                if let Err(e) =
                    claudear_analysis::learning::ReviewClassifier::process_review_comments_with_llm(
                        self.tracker.as_ref(),
                        repo,
                        &[mock_comment],
                        Some(feedback),
                        self.llm(),
                    )
                {
                    tracing::warn!(error = %e, "Failed to classify review feedback");
                }

                // Check if any patterns should be promoted
                if let Ok(promotable) =
                    claudear_analysis::learning::ReviewClassifier::check_promotion_threshold(
                        self.tracker.as_ref(),
                        repo,
                        self.config.learning.review_promotion_threshold,
                    )
                {
                    for pattern in &promotable {
                        if let Err(e) =
                            claudear_analysis::learning::RepoKnowledgeManager::learn_from_review_pattern(
                                self.tracker.as_ref(),
                                repo,
                                pattern,
                            )
                        {
                            tracing::warn!(error = %e, "Failed to learn from promoted review pattern");
                        }
                    }
                }
            }
        }

        // Find the source for this issue
        let source = match self.sources.iter().find(|s| s.name() == attempt.source) {
            Some(s) => s,
            None => {
                tracing::warn!(
                    source = %attempt.source,
                    "Source not found for review action"
                );
                return Ok(ReviewOutcome::Handled);
            }
        };

        // Verify the issue exists before processing
        let issue_exists = source.get_issue(&attempt.issue_id).await.is_ok();

        if !issue_exists {
            tracing::warn!(
                issue_id = %attempt.issue_id,
                "Could not find original issue for review action"
            );
            return Err(claudear_core::error::Error::source(
                source.name(),
                format!("Issue {} not found for review action", attempt.issue_id),
            ));
        }

        // Process the issue with the review feedback appended to context.
        let pr_url = match &attempt.pr_url {
            Some(url) => url,
            None => {
                tracing::warn!(
                    source = %attempt.source,
                    issue_id = %attempt.issue_id,
                    short_id = %attempt.short_id,
                    "Cannot process review feedback: attempt has no PR URL"
                );
                return Err(claudear_core::error::Error::source(
                    &attempt.source,
                    format!(
                        "Attempt {} has no PR URL, cannot address review feedback",
                        attempt.short_id
                    ),
                ));
            }
        };

        // Look up the existing PR branch so the worktree can check it out
        let existing_pr_branch = self
            .tracker
            .get_pr(pr_url)
            .ok()
            .flatten()
            .and_then(|pr| pr.head_branch);

        tracing::info!(
            pr_url = %pr_url,
            branch = ?existing_pr_branch,
            "Re-processing issue to address review feedback"
        );

        let charge = self.charge_review_cycle(attempt, pr_url);
        if charge == ReviewCycleCharge::CapReached {
            self.leave_pr_to_humans(attempt, pr_url, MAX_REVIEW_CYCLES);
            return Ok(ReviewOutcome::Handled);
        }
        let rerun = self
            .rerun_with_review_feedback(attempt, feedback, existing_pr_branch)
            .await;
        if rerun.is_err() && charge == ReviewCycleCharge::Charged {
            self.refund_review_cycle(pr_url);
        }
        rerun.map(|()| ReviewOutcome::Handled)
    }

    /// Stop answering review feedback on `pr_url`, which has used
    /// `review_cycles` of its [`MAX_REVIEW_CYCLES`], and leave the PR to
    /// humans.
    fn leave_pr_to_humans(&self, attempt: &FixAttempt, pr_url: &str, review_cycles: i32) {
        tracing::warn!(
            pr_url = %pr_url,
            short_id = %attempt.short_id,
            review_cycles,
            "Review cycle cap reached; leaving PR to humans"
        );
        self.tracker
            .record_activity(
                &ActivityLogEntry::new(
                    REVIEW_CYCLE_CAP_REACHED_ACTIVITY,
                    format!(
                        "Stopped addressing review feedback for {} after {} cycles",
                        attempt.short_id, review_cycles
                    ),
                )
                .with_source(attempt.source.clone())
                .with_issue(attempt.issue_id.clone(), attempt.short_id.clone())
                .with_metadata(json!({ "pr_url": pr_url })),
            )
            .ok();
        if let Some(review_watcher) = &self.review_watcher {
            review_watcher.unwatch_pr(pr_url);
        }
    }

    /// Leave `attempt`'s review feedback outstanding because the repository of
    /// the PR under review, `repo`, cannot be resolved from the index,
    /// recording the wait once rather than every cycle it lasts.
    fn defer_review_rerun(&self, attempt: &FixAttempt, pr_url: &str, repo: &str, reason: &str) {
        let newly_deferred = self
            .lock_deferred_review_reruns()
            .insert(pr_url.to_string());
        if !newly_deferred {
            tracing::debug!(
                pr_url = %pr_url,
                repo = %repo,
                "Review rerun still waiting for its PR repository"
            );
            return;
        }
        tracing::warn!(
            short_id = %attempt.short_id,
            repo = %repo,
            reason = %reason,
            "PR repo not resolvable, deferring review rerun"
        );
        self.record_attempt_decision(
            attempt,
            REVIEW_RERUN_DEFERRED_DECISION,
            format!(
                "Deferred review rerun for {}: PR repository {} is not resolvable",
                attempt.short_id, repo
            ),
            json!({ "pr_repo": repo, "reason": reason }),
        );
    }

    /// Lock the PRs whose review reruns are deferred, recovering them from a
    /// poisoned lock: the set only gains or loses whole URLs, so a holder that
    /// panicked cannot leave it inconsistent.
    fn lock_deferred_review_reruns(&self) -> MutexGuard<'_, HashSet<String>> {
        self.deferred_review_reruns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Rerun `attempt`'s issue with review `feedback` on its PR branch, in
    /// the PR's repository.
    ///
    /// A run of the same issue already in flight is waited on, for up to five
    /// minutes, so the feedback is not silently dropped.
    async fn rerun_with_review_feedback(
        &self,
        attempt: &FixAttempt,
        feedback: &str,
        existing_pr_branch: Option<String>,
    ) -> Result<()> {
        let processing_key = format!("{}:{}", attempt.source, attempt.issue_id);
        let wait_started = std::time::Instant::now();
        let max_wait = std::time::Duration::from_secs(300);
        loop {
            while self.lock_processing().contains(&processing_key) {
                if !self.is_running.load(Ordering::SeqCst) {
                    return Err(claudear_core::error::Error::source(
                        &attempt.source,
                        format!(
                            "Watcher stopping while waiting for in-flight processing of {}",
                            attempt.short_id
                        ),
                    ));
                }
                if wait_started.elapsed() >= max_wait {
                    return Err(claudear_core::error::Error::source(
                        &attempt.source,
                        format!(
                            "Timed out waiting for in-flight processing of {}",
                            attempt.short_id
                        ),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            match self
                .trigger_issue_inner(
                    &attempt.source,
                    &attempt.issue_id,
                    Some(feedback.to_string()),
                    existing_pr_branch.clone(),
                    Some("Review feedback received".into()),
                    attempt.scm_repo.as_deref(),
                )
                .await
            {
                Ok(()) => break,
                Err(e) => {
                    if self.lock_processing().contains(&processing_key) {
                        if !self.is_running.load(Ordering::SeqCst) {
                            return Err(claudear_core::error::Error::source(
                                &attempt.source,
                                format!(
                                    "Watcher stopping while waiting for in-flight processing of {}",
                                    attempt.short_id
                                ),
                            ));
                        }
                        if wait_started.elapsed() >= max_wait {
                            return Err(claudear_core::error::Error::source(
                                &attempt.source,
                                format!(
                                    "Timed out waiting for in-flight processing of {}",
                                    attempt.short_id
                                ),
                            ));
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    /// The PR record `attempt`'s review cycles are counted on. Cascade PRs are
    /// watched without one, so it is built from the attempt for them and the
    /// cap still counts their reruns.
    fn review_cycle_record(&self, attempt: &FixAttempt, pr_url: &str) -> Option<PrRecord> {
        match self.tracker.get_pr(pr_url) {
            Ok(Some(record)) => Some(record),
            Ok(None) => match (&attempt.scm_repo, attempt.scm_pr_number) {
                (Some(repo), Some(number)) => {
                    let mut record = PrRecord::new(pr_url, repo, number);
                    record.attempt_id = Some(attempt.id);
                    record.issue_id = Some(attempt.issue_id.clone());
                    record.issue_source = Some(attempt.source.clone());
                    Some(record)
                }
                _ => None,
            },
            Err(e) => {
                tracing::warn!(pr_url = %pr_url, error = %e, "Failed to load PR record for review cycle cap");
                None
            }
        }
    }

    /// Count a review-driven rerun of `pr_url` toward [`MAX_REVIEW_CYCLES`]
    /// unless the PR has used them all. Storage checks the cap and counts the
    /// cycle in one step, so processes sharing the database cannot both take
    /// the PR's last cycle.
    fn charge_review_cycle(&self, attempt: &FixAttempt, pr_url: &str) -> ReviewCycleCharge {
        let Some(record) = self.review_cycle_record(attempt, pr_url) else {
            return ReviewCycleCharge::Uncounted;
        };
        match self
            .tracker
            .charge_pr_review_cycle(&record, MAX_REVIEW_CYCLES)
        {
            Ok(true) => ReviewCycleCharge::Charged,
            Ok(false) => ReviewCycleCharge::CapReached,
            Err(error) => {
                tracing::warn!(pr_url = %pr_url, error = %error, "Failed to update PR review cycles");
                ReviewCycleCharge::Uncounted
            }
        }
    }

    /// Give back the review cycle charged for a rerun of `pr_url` that never
    /// started, so reruns that keep failing to start cannot use up
    /// [`MAX_REVIEW_CYCLES`] before any feedback is addressed. Only that one
    /// cycle is given back, whatever other processes counted meanwhile.
    fn refund_review_cycle(&self, pr_url: &str) {
        if let Err(error) = self.tracker.refund_pr_review_cycle(pr_url) {
            tracing::warn!(pr_url = %pr_url, error = %error, "Failed to refund PR review cycle");
        }
    }

    /// Trigger cascade processing for downstream repos after a PR is merged
    /// or a release is published.
    ///
    /// Looks up the merged repo in the dependency graph and spawns Claude
    /// in each direct dependent repo with context about the upstream changes.
    /// The `trigger_type` controls which cascade rules are matched.
    pub async fn trigger_cascade(
        &self,
        attempt: &claudear_core::types::FixAttempt,
        pr_url: &str,
        trigger_type: claudear_config::config::CascadeTrigger,
    ) -> Result<()> {
        let relationships = match &self.relationships {
            Some(r) => r,
            None => return Ok(()),
        };

        if !self.config.cascade.enabled {
            return Ok(());
        }

        let scm_repo = match &attempt.scm_repo {
            Some(r) => r.clone(),
            None => return Ok(()),
        };

        if attempt.scm_pr_number.is_none() {
            return Ok(());
        }

        // Check cascade depth limit
        if self.config.cascade.max_depth > 0 {
            let depth = self.get_cascade_depth(attempt);
            if depth >= self.config.cascade.max_depth {
                tracing::info!(
                    short_id = %attempt.short_id,
                    depth = depth,
                    max_depth = self.config.cascade.max_depth,
                    "Cascade depth limit reached, stopping"
                );
                return Ok(());
            }
        }

        // Try full owner/repo name first (used when dependencies are loaded from DB),
        // fall back to short name for backwards compatibility with hardcoded defaults.
        let repo_short_name = scm_repo.split('/').next_back().unwrap_or(&scm_repo);
        let (dependants, graph_key) = {
            let full = relationships.get_dependants(&scm_repo);
            if !full.is_empty() {
                (full, scm_repo.to_string())
            } else {
                let short = relationships.get_dependants(repo_short_name);
                (short, repo_short_name.to_string())
            }
        };

        // Collect downstream repo names from the dependency graph
        let graph_names: std::collections::HashSet<&str> =
            dependants.iter().map(|d| d.name.as_str()).collect();

        // Also collect downstream repos from explicit cascade rules (config-driven).
        // This allows cascades to work even without detected code-level dependencies.
        // Only include rules that match the current trigger type.
        let rule_only_downstreams: Vec<&str> = self
            .config
            .cascade
            .rules
            .iter()
            .filter(|r| {
                (r.upstream == scm_repo || r.upstream == repo_short_name)
                    && r.trigger == trigger_type
                    && !graph_names.contains(r.downstream.as_str())
            })
            .map(|r| r.downstream.as_str())
            .collect();

        if dependants.is_empty() && rule_only_downstreams.is_empty() {
            tracing::debug!(
                repo = %scm_repo,
                short_name = %repo_short_name,
                trigger = ?trigger_type,
                "No downstream dependants found for cascade"
            );
            return Ok(());
        }

        tracing::info!(
            repo = %scm_repo,
            trigger = ?trigger_type,
            graph_dependants = dependants.len(),
            rule_dependants = rule_only_downstreams.len(),
            "Triggering cascade for downstream repos"
        );

        let upstream_pr_url = pr_url.to_string();
        let graph = relationships.get_graph();

        // Process graph dependants (have actual code dependencies)
        for dependant in dependants {
            let dep_type = graph
                .get_first_hop_dependency_type_to_target(&graph_key, &dependant.name)
                .map(|t| t.as_str())
                .unwrap_or("unknown");

            // Look up per-dependency cascade rule for this trigger type
            let rule = self
                .config
                .cascade
                .find_rule_for_trigger(&scm_repo, &dependant.name, &trigger_type)
                .or_else(|| {
                    self.config.cascade.find_rule_for_trigger(
                        repo_short_name,
                        &dependant.name,
                        &trigger_type,
                    )
                });

            // If no rule matches this trigger type, check if there's a rule with a
            // different trigger — if so, skip (the other trigger path will handle it).
            // If no rule exists at all, graph dependants cascade on merge by default.
            if rule.is_none() {
                let any_rule = self.config.cascade.find_rule(&scm_repo, &dependant.name);
                if let Some(r) = any_rule {
                    if r.trigger != trigger_type {
                        tracing::info!(
                            upstream = %scm_repo,
                            downstream = %dependant.name,
                            rule_trigger = ?r.trigger,
                            current_trigger = ?trigger_type,
                            "Skipping cascade — rule requires different trigger"
                        );
                        continue;
                    }
                } else if trigger_type != claudear_config::config::CascadeTrigger::Merge {
                    // No explicit rule and this isn't a merge trigger —
                    // graph dependants only auto-cascade on merge.
                    continue;
                }
            }

            if let Err(e) = self
                .cascade_to_repo(
                    attempt,
                    &dependant.name,
                    &scm_repo,
                    &upstream_pr_url,
                    dep_type,
                    rule,
                )
                .await
            {
                tracing::error!(
                    upstream = %scm_repo,
                    downstream = %dependant.name,
                    error = %e,
                    "Failed to cascade to downstream repo"
                );
            }
        }

        // Process cascade-rule-only downstreams (explicitly configured, no code dependency detected)
        for downstream in rule_only_downstreams {
            let rule =
                self.config
                    .cascade
                    .find_rule_for_trigger(&scm_repo, downstream, &trigger_type);

            if let Err(e) = self
                .cascade_to_repo(
                    attempt,
                    downstream,
                    &scm_repo,
                    &upstream_pr_url,
                    "cascade",
                    rule,
                )
                .await
            {
                tracing::error!(
                    upstream = %scm_repo,
                    downstream = %downstream,
                    error = %e,
                    "Failed to cascade to downstream repo"
                );
            }
        }

        Ok(())
    }

    /// Check for new releases on upstream repos with release-triggered cascade rules.
    /// When a new release is detected, finds the most recently merged attempt for that
    /// repo and triggers cascade with `CascadeTrigger::Release`.
    pub async fn check_releases_and_cascade(&self) -> Result<()> {
        if !self.config.cascade.enabled {
            return Ok(());
        }

        let upstreams = self.config.cascade.release_trigger_upstreams();
        if upstreams.is_empty() {
            return Ok(());
        }

        let has_scm = self.scm_provider.is_some() || self.github_client.is_some();
        if !has_scm {
            return Ok(());
        }

        for upstream in upstreams {
            if !self.is_running() {
                break;
            }
            let release_result = if let Some(ref provider) = self.scm_provider {
                provider.get_latest_release(upstream).await
            } else if let Some(ref gh) = self.github_client {
                gh.get_latest_release(upstream).await
            } else {
                break;
            };

            let release = match release_result {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        upstream = %upstream,
                        error = %e,
                        "Failed to check latest release for cascade"
                    );
                    continue;
                }
            };

            {
                let seen = self.last_seen_releases.read().await;
                if seen.get(upstream).map(|t| t.as_str()) == Some(&release.tag) {
                    continue;
                }
            }

            let _claim = self.claim_run();
            if !self.is_running() {
                break;
            }

            tracing::info!(
                upstream = %upstream,
                tag = %release.tag,
                "New release detected, checking for release-triggered cascades"
            );

            // Mark as seen before processing (avoid duplicate cascades)
            {
                let mut seen = self.last_seen_releases.write().await;
                seen.insert(upstream.to_string(), release.tag.clone());
            }

            let merged_attempt = self
                .tracker
                .get_most_recent_merged_attempt_for_repo(upstream)
                .ok()
                .flatten();

            let attempt = match merged_attempt {
                Some(a) => a,
                None => {
                    tracing::info!(
                        upstream = %upstream,
                        "No merged attempt found for release-triggered cascade"
                    );
                    continue;
                }
            };

            let pr_url = attempt.pr_url.clone().unwrap_or_default();
            match self
                .trigger_cascade(
                    &attempt,
                    &pr_url,
                    claudear_config::config::CascadeTrigger::Release,
                )
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        upstream = %upstream,
                        tag = %release.tag,
                        "Release-triggered cascade completed"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        upstream = %upstream,
                        tag = %release.tag,
                        error = %e,
                        "Failed to trigger release cascade"
                    );
                }
            }
        }

        Ok(())
    }

    /// Get the cascade depth of an attempt (0 for root, 1 for first cascade, etc.)
    ///
    /// Includes cycle detection via a visited set to prevent infinite loops
    /// if cyclic parent references exist in the database.
    fn get_cascade_depth(&self, attempt: &claudear_core::types::FixAttempt) -> usize {
        const MAX_CASCADE_DEPTH: usize = 64;
        let mut depth = 0;
        let mut current_parent = attempt.parent_attempt_id;
        let mut visited = HashSet::new();

        while let Some(parent_id) = current_parent {
            if !visited.insert(parent_id) || depth >= MAX_CASCADE_DEPTH {
                tracing::warn!(
                    depth = depth,
                    parent_id = parent_id,
                    "Cascade depth walk terminated: cycle detected or max depth reached"
                );
                break;
            }
            depth += 1;
            match self.tracker.get_attempt_by_id(parent_id).ok().flatten() {
                Some(parent) => current_parent = parent.parent_attempt_id,
                None => break,
            }
        }

        depth
    }

    /// Execute a cascade fix in a single downstream repo.
    async fn cascade_to_repo(
        &self,
        parent_attempt: &claudear_core::types::FixAttempt,
        downstream_repo_name: &str,
        upstream_repo: &str,
        upstream_pr_url: &str,
        dep_type: &str,
        rule: Option<&claudear_config::config::CascadeRule>,
    ) -> Result<()> {
        tracing::info!(
            upstream = %upstream_repo,
            downstream = %downstream_repo_name,
            parent_id = parent_attempt.id,
            "Cascading to downstream repo"
        );

        // Resolve the downstream repo's local path
        let resolution = claudear_analysis::inference::resolve_repo_for_cascade(
            self.inferrer.as_ref(),
            downstream_repo_name,
        );

        let (project_dir, scm_url, default_branch) = match resolution {
            claudear_analysis::inference::RepoResolution::Resolved {
                project_dir,
                scm_url,
                default_branch,
                ..
            } => (project_dir, scm_url, default_branch),
            claudear_analysis::inference::RepoResolution::Skip { reason } => {
                tracing::warn!(
                    downstream = %downstream_repo_name,
                    reason = %reason,
                    "Cannot cascade — downstream repo not available"
                );
                return Ok(());
            }
        };

        // Record cascade attempt
        let attempt_id = self.tracker.record_cascade_attempt(
            &parent_attempt.source,
            &parent_attempt.issue_id,
            &parent_attempt.short_id,
            parent_attempt.id,
            &scm_url,
        )?;

        // Fetch the downstream repo (no checkout/reset — just update object store)
        let detected_default_branch = match GitOps::ensure_repo_synced(&project_dir, &scm_url).await
        {
            Ok(branch) => branch,
            Err(e) => {
                tracing::warn!(
                    downstream = %downstream_repo_name,
                    error = %e,
                    "Failed to fetch downstream repo, continuing with index default branch"
                );
                default_branch.clone()
            }
        };

        // Incrementally re-index code after fetch so code search is up-to-date
        self.reindex_repo(downstream_repo_name, &project_dir).await;

        // Create a per-cascade worktree so concurrent cascades don't interfere
        let cascade_id = format!("cascade-{}", parent_attempt.short_id);
        let wt_path = worktree_path(&self.config.workspace, downstream_repo_name, &cascade_id);
        let effective_branch = rule
            .and_then(|r| r.target_branch.as_deref())
            .unwrap_or(&detected_default_branch);
        GitOps::create_worktree(
            &project_dir,
            &wt_path,
            &format!("origin/{}", effective_branch),
        )
        .await
        .map_err(|e| {
            tracing::error!(
                downstream = %downstream_repo_name,
                error = %e,
                "Failed to create cascade worktree"
            );
            e
        })?;
        let effective_dir = &wt_path;

        // Build the cascade prompt (rule-aware)
        let version_instruction = if rule.is_none_or(|r| r.version_update) {
            format!(
                "- Update the dependency version for {} in this project's package manifest (package.json, composer.json, etc.)",
                upstream_repo
            )
        } else {
            "- No version update needed for this dependency".to_string()
        };

        let custom_instructions = rule
            .and_then(|r| r.instructions.as_deref())
            .map(|i| format!("\n\n## Additional Instructions\n{}", i))
            .unwrap_or_default();

        let prompt = format!(
            r#"A dependency has been updated in {upstream_repo}.

## Original Issue
[{short_id}] {source} issue that was fixed upstream.

## Upstream PR
{upstream_pr_url}

Review the upstream PR above to understand what changed.

## Your Task
This repository ({downstream_repo_name}) depends on {upstream_repo} via {dep_type}.
Review the upstream changes and make any necessary adaptations:
{version_instruction}
- Adapt to any API changes
- Update tests that exercise the changed functionality
- Ensure the project builds and tests pass

Create a PR with your changes.{custom_instructions}"#,
            upstream_repo = upstream_repo,
            short_id = parent_attempt.short_id,
            source = parent_attempt.source,
            upstream_pr_url = upstream_pr_url,
            downstream_repo_name = downstream_repo_name,
            dep_type = dep_type,
            version_instruction = version_instruction,
            custom_instructions = custom_instructions,
        );

        // Run Claude
        let result = self
            .agent
            .execute_with_attempt(&prompt, None, Some(attempt_id), effective_dir)
            .await?;

        if result.success {
            if let Some(ref pr_url) = result.pr_url {
                tracing::info!(
                    downstream = %downstream_repo_name,
                    pr_url = %pr_url,
                    "Cascade PR created"
                );

                // Update the cascade attempt with PR details
                if let Some((repo, pr_num)) = claudear_storage::parse_pr_url(pr_url) {
                    self.tracker
                        .update_attempt_pr(attempt_id, pr_url, &repo, pr_num)?;
                }

                // Register for review watching — this enables recursive cascade
                if let Some(ref review_watcher) = self.review_watcher {
                    if let Some((repo, pr_number)) = claudear_storage::parse_pr_url(pr_url) {
                        let state = PrReviewState::new(
                            pr_url,
                            &repo,
                            pr_number,
                            &parent_attempt.issue_id,
                            &parent_attempt.source,
                        );
                        review_watcher.watch_pr(state);
                        tracing::info!(
                            component = "cascade",
                            pr_url = %pr_url,
                            "Cascade PR registered for review watching"
                        );
                    }
                }

                // Log activity
                let activity = ActivityLogEntry::new(
                    "cascade_pr_created",
                    format!(
                        "Cascade PR created in {} for upstream {}",
                        downstream_repo_name, upstream_repo
                    ),
                )
                .with_source(parent_attempt.source.clone())
                .with_issue(
                    parent_attempt.issue_id.clone(),
                    parent_attempt.short_id.clone(),
                );
                self.tracker.record_activity(&activity).ok();

                // Notify cascade success
                let mut cascade_issue = Issue::new(
                    &parent_attempt.issue_id,
                    &parent_attempt.short_id,
                    format!("Cascade: {} -> {}", upstream_repo, downstream_repo_name),
                    pr_url,
                    &parent_attempt.source,
                );
                cascade_issue.set_metadata("cascade_upstream_repo", upstream_repo.to_string());
                cascade_issue
                    .set_metadata("cascade_downstream_repo", downstream_repo_name.to_string());
                cascade_issue.set_metadata("cascade_upstream_pr_url", upstream_pr_url.to_string());
                cascade_issue.set_metadata(
                    "cascade_original_issue_short_id",
                    parent_attempt.short_id.clone(),
                );
                if let Some(ref changelog) = result.changelog {
                    cascade_issue.set_metadata("changelog", changelog.clone());
                }
                let _ = self.notifier.notify_success(&cascade_issue, pr_url).await;
            } else {
                // Cascade succeeded but no PR
                let reason = if result.output.is_empty() {
                    "Cascade completed without creating a PR".to_string()
                } else if result.output.chars().count() > 500 {
                    let truncated: String = result.output.chars().take(497).collect();
                    format!("{}...", truncated)
                } else {
                    result.output.clone()
                };
                tracing::warn!(
                    downstream = %downstream_repo_name,
                    reason = %reason,
                    "Cascade succeeded but no PR URL"
                );
                self.tracker.mark_cascade_failed(
                    attempt_id,
                    &format!("Cascade completed without creating a PR: {}", reason),
                )?;

                let mut cascade_issue = Issue::new(
                    &parent_attempt.issue_id,
                    &parent_attempt.short_id,
                    format!("Cascade: {} -> {}", upstream_repo, downstream_repo_name),
                    "",
                    &parent_attempt.source,
                );
                cascade_issue.set_metadata("cascade_upstream_repo", upstream_repo.to_string());
                cascade_issue
                    .set_metadata("cascade_downstream_repo", downstream_repo_name.to_string());
                cascade_issue.set_metadata("cascade_upstream_pr_url", upstream_pr_url.to_string());
                cascade_issue.set_metadata(
                    "cascade_original_issue_short_id",
                    parent_attempt.short_id.clone(),
                );
                cascade_issue.set_metadata("completion_reason", reason);
                let _ = self.notifier.notify_completed(&cascade_issue).await;
            }
        } else {
            let base_error = result.error.unwrap_or_else(|| "Unknown error".to_string());
            let error = if !result.output.is_empty() {
                let summary = if result.output.chars().count() > 500 {
                    let truncated: String = result.output.chars().take(497).collect();
                    format!("{}...", truncated)
                } else {
                    result.output.clone()
                };
                format!("{}\n\nClaude's summary: {}", base_error, summary)
            } else {
                base_error
            };
            tracing::warn!(
                downstream = %downstream_repo_name,
                error = %error,
                "Cascade fix failed"
            );
            self.tracker.mark_cascade_failed(attempt_id, &error)?;

            // Notify cascade failure
            let mut cascade_issue = Issue::new(
                &parent_attempt.issue_id,
                &parent_attempt.short_id,
                format!("Cascade: {} -> {}", upstream_repo, downstream_repo_name),
                "",
                &parent_attempt.source,
            );
            cascade_issue.set_metadata("cascade_upstream_repo", upstream_repo.to_string());
            cascade_issue.set_metadata("cascade_downstream_repo", downstream_repo_name.to_string());
            cascade_issue.set_metadata("cascade_upstream_pr_url", upstream_pr_url.to_string());
            cascade_issue.set_metadata(
                "cascade_original_issue_short_id",
                parent_attempt.short_id.clone(),
            );
            let _ = self.notifier.notify_failed(&cascade_issue, &error).await;
        }

        // Cleanup cascade worktree
        if wt_path.exists() {
            if let Err(e) = GitOps::remove_worktree(&project_dir, &wt_path).await {
                tracing::warn!(
                    downstream = %downstream_repo_name,
                    error = %e,
                    "Failed to remove cascade worktree"
                );
            }
        }

        Ok(())
    }

    /// Seed the tracker with existing issues.
    ///
    /// The `deploy_qa` source is skipped: its tips are dispatched only by
    /// [`Self::dispatch_pending_deploy_qa_tips`], and a seeded failed attempt
    /// would hand a pending tip to the retry manager as well.
    pub async fn seed(&self) -> Result<SeedResult> {
        tracing::info!("");
        tracing::info!("Seeding tracker with existing issues...");

        let mut results = SeedResult::default();

        for source in &self.sources {
            if source.name() == DEPLOY_QA_SOURCE {
                continue;
            }
            match source.fetch_issues().await {
                Ok(issues) => {
                    let mut seeded = 0;
                    for issue in issues {
                        if !self.tracker.has_attempted(source.name(), &issue.id)? {
                            let labels: Vec<String> =
                                issue.get_metadata("labels").unwrap_or_default();
                            self.tracker.record_attempt_with_labels(
                                source.name(),
                                &issue.id,
                                &issue.short_id,
                                &labels,
                            )?;
                            self.tracker.mark_failed(
                                source.name(),
                                &issue.id,
                                "SEEDED: Marked as seen during initial seed",
                            )?;
                            seeded += 1;
                        }
                    }
                    results.by_source.insert(source.name().to_string(), seeded);
                    results.total += seeded;
                    tracing::info!(source = source.name(), count = seeded, "Seeded issues");
                }
                Err(e) => {
                    tracing::error!(source = source.name(), error = %e, "Error seeding");
                }
            }
        }

        tracing::info!("");
        tracing::info!(
            "Seeding complete. Total: {} issues marked as seen.",
            results.total
        );
        tracing::info!("New issues created after this will be processed normally.");
        tracing::info!("");

        Ok(results)
    }

    /// Run a single poll cycle.
    async fn poll(self: &Arc<Self>) -> Result<()> {
        if self.is_rate_limit_paused().await {
            return Ok(());
        }

        let poll_started_at = std::time::Instant::now();
        tracing::info!("");
        tracing::info!(
            "[{}] Polling...",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S")
        );

        // Poll all sources concurrently for better throughput.
        let poll_futures: Vec<_> = self
            .sources
            .iter()
            .map(|source| async move {
                if let Err(e) = self.poll_source(source).await {
                    tracing::error!(component = "watcher", source = source.name(), error = %e, "Error polling");
                }
            })
            .collect();
        join_all(poll_futures).await;

        // Process any ready retries
        if !self.dry_run {
            if let Err(e) = self.process_ready_retries().await {
                tracing::error!(component = "watcher", error = %e, "Error processing retries");
            }
        }

        // Check for PR merges and trigger cascades
        if !self.dry_run {
            if let Err(e) = self.check_pr_merges_and_cascade().await {
                tracing::error!(component = "watcher", error = %e, "Error checking PR merges for cascade");
            }
        }

        // Check for new releases and trigger release-based cascades
        if !self.dry_run {
            if let Err(e) = self.check_releases_and_cascade().await {
                tracing::error!(component = "watcher", error = %e, "Error checking releases for cascade");
            }
        }

        // Record lightweight operational telemetry for dashboard analytics.
        if !self.dry_run {
            let poll_duration_metric = ProcessingMetric::new(
                "poll_cycle_duration_secs",
                poll_started_at.elapsed().as_secs_f64(),
            );
            if let Err(e) = self.tracker.record_metric(&poll_duration_metric) {
                tracing::debug!(error = %e, "Failed to record poll_cycle_duration_secs metric");
            }

            let source_count_metric =
                ProcessingMetric::new("poll_sources", self.sources.len() as f64);
            if let Err(e) = self.tracker.record_metric(&source_count_metric) {
                tracing::debug!(error = %e, "Failed to record poll_sources metric");
            }

            let active = self.active_processing.load(Ordering::SeqCst) as f64;
            let active_metric = ProcessingMetric::new("active_processing", active);
            if let Err(e) = self.tracker.record_metric(&active_metric) {
                tracing::debug!(error = %e, "Failed to record active_processing metric");
            }

            match self.tracker.get_stats() {
                Ok(stats) => {
                    let pending_metric =
                        ProcessingMetric::new("pending_attempts", stats.pending as f64);
                    if let Err(e) = self.tracker.record_metric(&pending_metric) {
                        tracing::debug!(error = %e, "Failed to record pending_attempts metric");
                    }

                    let total_metric = ProcessingMetric::new("total_attempts", stats.total as f64);
                    if let Err(e) = self.tracker.record_metric(&total_metric) {
                        tracing::debug!(error = %e, "Failed to record total_attempts metric");
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "Failed to load stats for poll metrics");
                }
            }
        }

        Ok(())
    }

    /// The weekly schedule for the repetitive-issues digest, or `None` when the
    /// feature is disabled. The caller (housekeeping loop) owns the returned
    /// schedule and its `last_sent_at` cadence state.
    pub fn repetitive_digest_schedule(&self) -> Option<ReportSchedule> {
        let cfg = &self.config.reports.repetitive_digest;
        if !cfg.enabled {
            return None;
        }
        let day = match ReportFrequency::parse(&format!("weekly-{}", cfg.day.to_lowercase())) {
            Some(ReportFrequency::Weekly(d)) => d,
            _ => chrono::Weekday::Mon,
        };
        Some(ReportSchedule::weekly("repetitive-digest", day, cfg.hour))
    }

    /// Build and send the weekly digest of repetitive, non-actionable Sentry
    /// issues — issues the agent gave up on (`cannot_fix`) that keep recurring.
    /// Report-only; the Discord notifier mentions the configured on-call user.
    ///
    /// Built entirely from stored data: the `cannot_fix` set joined with the
    /// recurrence observed at processing time (see `record_issue_recurrence`).
    /// No live API calls, so it never surfaces issues the agent hasn't seen and
    /// tried. No-ops when nothing qualifies.
    pub async fn send_repetitive_digest(&self) -> Result<()> {
        let min_event_count = self.config.reports.repetitive_digest.min_event_count;
        let digest = ReportGenerator::new(self.tracker.clone())
            .generate_repetitive_digest(min_event_count)?;

        if digest.is_empty() {
            tracing::info!(
                component = "digest",
                "No repetitive non-actionable Sentry issues this week; nothing to send"
            );
            return Ok(());
        }

        tracing::info!(
            component = "digest",
            count = digest.entries.len(),
            "Sending weekly repetitive-issues digest"
        );
        self.notifier.notify_repetitive_digest(&digest).await
    }

    /// Run housekeeping tasks: orphaned-attempt release, retries, cascades,
    /// and metrics. Called on the global timer, separate from per-source
    /// polling.
    pub async fn run_housekeeping_cycle(&self) -> Result<()> {
        let housekeeping_started_at = std::time::Instant::now();

        if !self.dry_run {
            self.release_orphaned_attempts();
            let (retries_result, pr_merges_result, releases_result) = tokio::join!(
                self.process_ready_retries(),
                self.check_pr_merges_and_cascade(),
                self.check_releases_and_cascade(),
            );

            if let Err(e) = retries_result {
                tracing::error!(component = "watcher", error = %e, "Error processing retries");
            }
            if let Err(e) = pr_merges_result {
                tracing::error!(component = "watcher", error = %e, "Error checking PR merges for cascade");
            }
            if let Err(e) = releases_result {
                tracing::error!(component = "watcher", error = %e, "Error checking releases for cascade");
            }
        }

        // Record lightweight operational telemetry for dashboard analytics.
        if !self.dry_run {
            let duration_metric = ProcessingMetric::new(
                "housekeeping_cycle_duration_secs",
                housekeeping_started_at.elapsed().as_secs_f64(),
            );
            if let Err(e) = self.tracker.record_metric(&duration_metric) {
                tracing::debug!(error = %e, "Failed to record housekeeping_cycle_duration_secs metric");
            }

            let source_count_metric =
                ProcessingMetric::new("poll_sources", self.sources.len() as f64);
            if let Err(e) = self.tracker.record_metric(&source_count_metric) {
                tracing::debug!(error = %e, "Failed to record poll_sources metric");
            }

            let active = self.active_processing.load(Ordering::SeqCst) as f64;
            let active_metric = ProcessingMetric::new("active_processing", active);
            if let Err(e) = self.tracker.record_metric(&active_metric) {
                tracing::debug!(error = %e, "Failed to record active_processing metric");
            }

            match self.tracker.get_stats() {
                Ok(stats) => {
                    let pending_metric =
                        ProcessingMetric::new("pending_attempts", stats.pending as f64);
                    if let Err(e) = self.tracker.record_metric(&pending_metric) {
                        tracing::debug!(error = %e, "Failed to record pending_attempts metric");
                    }

                    let total_metric = ProcessingMetric::new("total_attempts", stats.total as f64);
                    if let Err(e) = self.tracker.record_metric(&total_metric) {
                        tracing::debug!(error = %e, "Failed to record total_attempts metric");
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "Failed to load stats for poll metrics");
                }
            }
        }

        Ok(())
    }

    /// Process any issues that are ready for retry.
    async fn process_ready_retries(&self) -> Result<()> {
        // Skip retries while paused for rate limits — attempting them would
        // just burn retry attempts without doing any work.
        if self.is_rate_limit_paused().await {
            tracing::debug!(
                component = "watcher",
                "Skipping ready retries while paused for Claude rate limit"
            );
            return Ok(());
        }

        let retry_manager = RetryManager::new(self.config.retry.clone(), self.tracker.clone());
        let ready = retry_manager.get_ready_retries()?;
        let ready_count = ready.len();
        self.record_source_decision(
            "watcher",
            "ready_retry_scan",
            "Scanned for ready retries",
            json!({
                "ready_count": ready_count,
            }),
        );

        let ready_found_metric = ProcessingMetric::new("ready_retries_found", ready_count as f64);
        if let Err(e) = self.tracker.record_metric(&ready_found_metric) {
            tracing::debug!(error = %e, "Failed to record ready_retries_found metric");
        }

        if ready.is_empty() {
            let retries_executed_metric =
                ProcessingMetric::new("ready_retries_executed_total", 0.0);
            if let Err(e) = self.tracker.record_metric(&retries_executed_metric) {
                tracing::debug!(error = %e, "Failed to record ready_retries_executed_total metric");
            }

            let retries_failed_metric = ProcessingMetric::new("ready_retries_failed_total", 0.0);
            if let Err(e) = self.tracker.record_metric(&retries_failed_metric) {
                tracing::debug!(error = %e, "Failed to record ready_retries_failed_total metric");
            }
            return Ok(());
        }

        tracing::info!(
            component = "watcher",
            count = ready.len(),
            "Processing ready retries"
        );

        let mut retries_executed = 0usize;
        let mut retries_failed = 0usize;

        for (i, attempt) in ready.into_iter().enumerate() {
            if !self.is_running.load(Ordering::SeqCst) {
                break;
            }

            let processing_key = format!("{}:{}", attempt.source, attempt.issue_id);
            if self.lock_processing().contains(&processing_key) {
                self.skip_inflight_retry(&attempt);
                continue;
            }

            let configured_retry_max_concurrent = self.config.max_concurrent_for(&attempt.source);
            let retry_max_concurrent = configured_retry_max_concurrent.max(1);
            if configured_retry_max_concurrent == 0 {
                tracing::warn!(
                    source = %attempt.source,
                    "max_concurrent_for source evaluated to 0, clamping to 1"
                );
            }
            loop {
                let released = self.next_slot_release();
                if self.active_processing_for_source(&attempt.source) < retry_max_concurrent {
                    break;
                }
                if !self.is_running.load(Ordering::SeqCst) {
                    return Ok(());
                }
                released.await;
            }

            let reason = {
                let detail = if attempt.status == FixAttemptStatus::Closed {
                    "PR closed without merge".to_string()
                } else if let Some(ref error) = attempt.error_message {
                    if error.len() > 80 {
                        format!("{}...", &error[..error.floor_char_boundary(77)])
                    } else {
                        error.clone()
                    }
                } else {
                    "previous failure".to_string()
                };
                format!("Retry attempt {}: {}", attempt.retry_count + 1, detail)
            };

            match self.retry(&attempt, &reason).await {
                RetryOutcome::Ran => {
                    self.record_source_decision(
                        &attempt.source,
                        "ready_retry_triggered",
                        format!("Retry triggered for {}", attempt.short_id),
                        json!({
                            "issue_id": attempt.issue_id.clone(),
                            "short_id": attempt.short_id.clone(),
                            "retry_count": attempt.retry_count,
                        }),
                    );
                    retries_executed += 1;
                    let metric = ProcessingMetric::new("ready_retry_executed", 1.0)
                        .with_source(attempt.source.clone());
                    if let Err(error) = self.tracker.record_metric(&metric) {
                        tracing::debug!(error = %error, "Failed to record ready_retry_executed metric");
                    }
                }
                RetryOutcome::Busy => self.skip_inflight_retry(&attempt),
                RetryOutcome::Stopping => break,
                RetryOutcome::Failed(error) => {
                    self.record_source_decision(
                        &attempt.source,
                        "ready_retry_trigger_failed",
                        format!("Retry trigger failed for {}", attempt.short_id),
                        json!({
                            "issue_id": attempt.issue_id.clone(),
                            "short_id": attempt.short_id.clone(),
                            "retry_count": attempt.retry_count,
                            "error": error.to_string(),
                        }),
                    );
                    retries_failed += 1;
                    let metric = ProcessingMetric::new("ready_retry_failed", 1.0)
                        .with_source(attempt.source.clone());
                    if let Err(record_error) = self.tracker.record_metric(&metric) {
                        tracing::debug!(
                            error = %record_error,
                            "Failed to record ready_retry_failed metric"
                        );
                    }
                    tracing::error!(
                        component = "watcher",
                        short_id = %attempt.short_id,
                        error = %error,
                        "Failed to trigger retry"
                    );
                }
            }

            if i + 1 < ready_count && self.config.processing_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.config.processing_delay_ms)).await;
            }
        }

        let retries_executed_metric =
            ProcessingMetric::new("ready_retries_executed_total", retries_executed as f64);
        if let Err(e) = self.tracker.record_metric(&retries_executed_metric) {
            tracing::debug!(error = %e, "Failed to record ready_retries_executed_total metric");
        }

        let retries_failed_metric =
            ProcessingMetric::new("ready_retries_failed_total", retries_failed as f64);
        if let Err(e) = self.tracker.record_metric(&retries_failed_metric) {
            tracing::debug!(error = %e, "Failed to record ready_retries_failed_total metric");
        }

        Ok(())
    }

    /// Record that `attempt` was not retried because its issue is already
    /// being processed.
    fn skip_inflight_retry(&self, attempt: &FixAttempt) {
        self.record_source_decision(
            &attempt.source,
            "ready_retry_skipped_inflight",
            format!(
                "Retry skipped because {} is already in-flight",
                attempt.short_id
            ),
            json!({
                "issue_id": attempt.issue_id.clone(),
                "short_id": attempt.short_id.clone(),
            }),
        );
        tracing::debug!(
            short_id = %attempt.short_id,
            "Issue already being processed, skipping retry"
        );
    }

    /// Retry `attempt`'s issue now, recording `reason` as why it runs again.
    ///
    /// The shutdown drain waits for the whole retry. The retry is spent once
    /// its run is admitted, or when its issue cannot be loaded or run, so an
    /// issue that keeps failing still runs out of retries. A failure that says
    /// nothing about the issue costs no retry: a retry refused because the
    /// watcher is stopping or paused for a rate limit, because the issue is
    /// already being processed, or because loading it failed transiently, such
    /// as on a network error, leaves the attempt as it was, and a run that ends
    /// on a rate limit gets its retry back, as does one whose approval request
    /// got no usable answer, so a later run asks again.
    pub async fn retry(&self, attempt: &FixAttempt, reason: &str) -> RetryOutcome {
        let _claim = self.claim_run();
        if self.is_stopped() {
            tracing::info!(
                component = "watcher",
                short_id = %attempt.short_id,
                "Not retrying because the watcher is stopping"
            );
            return RetryOutcome::Stopping;
        }
        tracing::info!(
            component = "watcher",
            source = %attempt.source,
            short_id = %attempt.short_id,
            retry_count = attempt.retry_count,
            "Retrying issue"
        );

        let retry_manager = RetryManager::new(self.config.retry.clone(), Arc::clone(&self.tracker));
        let Some(source) = self
            .sources
            .iter()
            .find(|source| source.name() == attempt.source)
        else {
            let error = claudear_core::error::Error::source(&attempt.source, "Unknown source");
            return self.fail_retry(&retry_manager, attempt, error);
        };
        let mut issue = match source.get_issue(&attempt.issue_id).await {
            Ok(issue) => issue,
            Err(error) if retry_trigger_error_is_transient(&error) => {
                return RetryOutcome::Failed(error)
            }
            Err(error) => return self.fail_retry(&retry_manager, attempt, error),
        };
        issue.set_metadata("trigger_reason", reason);

        let mut spend = None;
        let run = self
            .process_issue(
                Arc::clone(source),
                issue,
                MatchResult::matched(MANUAL_TRIGGER, MatchPriority::Urgent),
                None,
                None,
                None,
                // Spent only here, once admitted, so a stop or a busy issue costs no retry.
                Some(Box::new(|| {
                    spend
                        .insert(retry_manager.prepare_retry(&attempt.source, &attempt.issue_id))
                        .is_ok()
                })),
            )
            .await;
        let skipped =
            || claudear_core::error::Error::Other(format!("{} was skipped", attempt.short_id));
        let unanswered = || {
            claudear_core::error::Error::Other(format!(
                "{} got no usable answer to its approval request",
                attempt.short_id
            ))
        };
        match (run, spend) {
            (IssueRun::Processed, Some(Ok(()))) => {
                self.refund_rate_limited_retry(attempt);
                RetryOutcome::Ran
            }
            (IssueRun::Processed | IssueRun::Declined, _) => RetryOutcome::Ran,
            (IssueRun::Unanswered, Some(Ok(()))) => self.refund_retry(attempt, unanswered()),
            (IssueRun::Unanswered, _) => RetryOutcome::Failed(unanswered()),
            (IssueRun::Stopping, _) => RetryOutcome::Stopping,
            (IssueRun::Busy, _) => RetryOutcome::Busy,
            (IssueRun::Paused, _) => {
                RetryOutcome::Failed(claudear_core::error::Error::Other(format!(
                    "{} was not run while paused for a rate limit",
                    attempt.short_id
                )))
            }
            (IssueRun::Skipped, Some(Err(error))) => RetryOutcome::Failed(error),
            (IssueRun::Skipped, Some(Ok(()))) => self.mark_retry_failed(attempt, skipped()),
            (IssueRun::Skipped, None) => self.fail_retry(&retry_manager, attempt, skipped()),
        }
    }

    /// Spend the retry of `attempt`, whose issue could not run, and mark it
    /// failed with `error`, so an issue that keeps failing still runs out of
    /// retries.
    fn fail_retry(
        &self,
        retry_manager: &RetryManager,
        attempt: &FixAttempt,
        error: claudear_core::error::Error,
    ) -> RetryOutcome {
        if let Err(spend_error) = retry_manager.prepare_retry(&attempt.source, &attempt.issue_id) {
            tracing::warn!(
                component = "watcher",
                short_id = %attempt.short_id,
                error = %spend_error,
                "Failed to spend the retry of an issue that could not run"
            );
            return RetryOutcome::Failed(error);
        }
        self.mark_retry_failed(attempt, error)
    }

    /// Mark `attempt`, whose retry was spent without a run, failed with
    /// `error`: retries pick up only failed attempts, so it would otherwise
    /// stay pending until the watcher next starts.
    fn mark_retry_failed(
        &self,
        attempt: &FixAttempt,
        error: claudear_core::error::Error,
    ) -> RetryOutcome {
        let message = format!("{RETRY_TRIGGER_FAILED}: {error}");
        if let Err(mark_error) =
            self.tracker
                .mark_failed(&attempt.source, &attempt.issue_id, &message)
        {
            tracing::warn!(
                component = "watcher",
                short_id = %attempt.short_id,
                error = %mark_error,
                "Failed to restore retry attempt state after trigger error"
            );
        }
        RetryOutcome::Failed(error)
    }

    /// Give back the retry `attempt` spent on a run that did no work for a
    /// reason that says nothing about the issue, and mark it failed with
    /// `error` so a later retry runs it again.
    fn refund_retry(
        &self,
        attempt: &FixAttempt,
        error: claudear_core::error::Error,
    ) -> RetryOutcome {
        let message = format!("{RETRY_TRIGGER_FAILED}: {error}");
        if let Err(refund_error) =
            self.tracker
                .mark_failed_uncharged(&attempt.source, &attempt.issue_id, &message)
        {
            tracing::warn!(
                component = "watcher",
                short_id = %attempt.short_id,
                error = %refund_error,
                "Failed to refund retry of a run that did no work"
            );
        }
        RetryOutcome::Failed(error)
    }

    /// Give back the retry `attempt` spent on a run that ended on a rate
    /// limit: running out of quota says nothing about the issue.
    fn refund_rate_limited_retry(&self, attempt: &FixAttempt) {
        let Ok(Some(after)) = self.tracker.get_attempt(&attempt.source, &attempt.issue_id) else {
            return;
        };
        let hit_rate_limit = after.status == FixAttemptStatus::Failed
            && after
                .error_message
                .as_deref()
                .is_some_and(runner::is_rate_limit_error);
        if !hit_rate_limit {
            return;
        }
        let error = after.error_message.unwrap_or_default();
        if let Err(refund_error) =
            self.tracker
                .mark_failed_uncharged(&attempt.source, &attempt.issue_id, &error)
        {
            tracing::warn!(
                component = "watcher",
                short_id = %attempt.short_id,
                error = %refund_error,
                "Failed to refund retry after rate limit"
            );
        }
    }

    /// After a fix merges, post a human-sounding "fix shipped" reply back to the
    /// originating ticket. Opt-in via `[reply]`; only tracker-style sources receive
    /// a ticket comment (conversational sources are notified via their channel).
    async fn maybe_send_fix_shipped_reply(&self, attempt: &FixAttempt) {
        if !self.config.reply().enabled {
            return;
        }
        if matches!(
            attempt.source.as_str(),
            "discord" | "slack" | "telegram" | "whatsapp"
        ) {
            return;
        }
        let Some(source) = self.sources.iter().find(|s| s.name() == attempt.source) else {
            return;
        };

        // Fetch the real issue for grounding; fall back to a synthetic one.
        let issue = match source.get_issue(&attempt.issue_id).await {
            Ok(i) => i,
            Err(_) => Issue::new(
                &attempt.issue_id,
                &attempt.short_id,
                "Issue resolved",
                attempt.pr_url.as_deref().unwrap_or(""),
                &attempt.source,
            ),
        };

        let inbox_key = issue
            .get_metadata::<String>("mailbox_id")
            .unwrap_or_else(|| attempt.source.clone());
        let guideline = self.config.reply().template_for(Some(&inbox_key));
        let context = match attempt.pr_url.as_deref() {
            Some(pr) => format!("The fix shipped in PR: {pr}"),
            None => String::new(),
        };

        let scratch = crate::processing::qa_scratch_directory();

        let timeout = std::time::Duration::from_secs(self.config.qa.answer_timeout_secs.max(1));
        let reply = match tokio::time::timeout(
            timeout,
            self.agent
                .generate_reply(&issue, &context, guideline, ReplyKind::FixShipped, &scratch),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(short_id = %attempt.short_id, error = %e, "Failed to generate fix-shipped reply");
                return;
            }
            Err(_) => {
                tracing::warn!(short_id = %attempt.short_id, "Fix-shipped reply generation timed out");
                return;
            }
        };

        if let Err(e) = source.add_comment(&attempt.issue_id, &reply).await {
            tracing::warn!(short_id = %attempt.short_id, error = %e, "Failed to post fix-shipped reply");
            return;
        }
        let summary: String = reply.chars().take(500).collect();
        let _ = self.tracker.record_action_run(
            &attempt.source,
            &attempt.issue_id,
            &attempt.short_id,
            "reply",
            "fix_shipped",
            &summary,
        );
    }

    /// Check for merged PRs and trigger cascade processing.
    async fn check_pr_merges_and_cascade(&self) -> Result<()> {
        let github_client = self.github_client.as_ref();
        let scm_provider = self.scm_provider.as_ref();
        let has_scm = github_client.is_some() || scm_provider.is_some();
        let pending_prs = if has_scm {
            self.tracker.get_pending_prs()?
        } else {
            Vec::new()
        };
        let mut pr_status_checks = 0usize;
        let mut pr_status_merged = 0usize;
        let mut pr_status_closed = 0usize;
        let mut pr_status_errors = 0usize;
        let mut regression_watches_created = 0usize;
        let mut auto_resolved_on_merge = 0usize;
        let mut cascade_triggered = 0usize;
        let mut cascade_failed = 0usize;

        for attempt in &pending_prs {
            if !self.is_running() {
                break;
            }
            let repo = match &attempt.scm_repo {
                Some(r) => r,
                None => continue,
            };
            let pr_number = match attempt.scm_pr_number {
                Some(n) => n,
                None => continue,
            };
            if !has_scm {
                break;
            }

            pr_status_checks += 1;
            let pr_status = if let Some(provider) = scm_provider {
                provider.get_pr_status(repo, pr_number).await
            } else if let Some(gh) = github_client {
                gh.get_pr_status(repo, pr_number).await
            } else {
                break;
            };
            match pr_status {
                Ok(PrStatus::Merged) => {
                    let _claim = self.claim_run();
                    if !self.is_running() {
                        break;
                    }
                    pr_status_merged += 1;
                    // A cascade row shares the parent's issue id: marking by issue id
                    // updated the parent and left this row pending, so every poll saw
                    // the merge again and re-fired the cascade
                    let is_cascade = attempt.cascade_repo.is_some();
                    if is_cascade {
                        self.tracker.mark_cascade_pr_outcome(attempt.id, true)?;
                    } else {
                        self.tracker
                            .mark_merged(&attempt.source, &attempt.issue_id)?;
                    }
                    self.tracker
                        .record_activity(
                            &ActivityLogEntry::new(
                                TimelineEventStatus::PrMerged.as_str(),
                                format!("PR merged for {}", attempt.short_id),
                            )
                            .with_source(attempt.source.clone())
                            .with_issue(attempt.issue_id.clone(), attempt.short_id.clone())
                            .with_metadata(json!({ "pr_url": attempt.pr_url })),
                        )
                        .ok();
                    let _ = self
                        .tracker
                        .update_qa_outcome_stats_for_attempt(attempt.id, true);

                    if let Some(ref pr_url) = attempt.pr_url {
                        if let Ok(Some(mut pr_record)) = self.tracker.get_pr(pr_url) {
                            pr_record.status = "merged".to_string();
                            pr_record.merged_at = Some(chrono::Utc::now());
                            if let Err(e) = self.tracker.upsert_pr(&pr_record) {
                                tracing::warn!(error = %e, "Failed to update PR status to merged");
                            }
                        }
                    }

                    // For bug-type issues, create a regression watch instead of immediate auto-resolve.
                    // A downstream cascade merge does not fix the parent issue, so it
                    // neither watches for regressions nor resolves it.
                    let regression_watch_id = if !is_cascade && attempt.is_bug() {
                        let issue_type = match attempt.source.as_str() {
                            "sentry" => IssueType::SentryIssue,
                            "linear" => IssueType::LinearBug,
                            _ => IssueType::SentryIssue,
                        };
                        let mut watch =
                            RegressionWatch::new(issue_type, &attempt.issue_id, attempt.id);
                        watch.pr_merged_at = Some(chrono::Utc::now());

                        match self.tracker.create_regression_watch(&watch) {
                            Ok(watch_id) => {
                                regression_watches_created += 1;
                                tracing::info!(
                                    component = "watcher",
                                    source = %attempt.source,
                                    issue_id = %attempt.issue_id,
                                    short_id = %attempt.short_id,
                                    watch_id = watch_id,
                                    "Created regression watch for merged bug fix"
                                );
                                Some(watch_id)
                            }
                            Err(e) => {
                                tracing::error!(
                                    component = "watcher",
                                    source = %attempt.source,
                                    issue_id = %attempt.issue_id,
                                    short_id = %attempt.short_id,
                                    error = %e,
                                    "Failed to create regression watch"
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };

                    let should_resolve = !is_cascade
                        && regression_watch_id.is_none()
                        && self.config.github().auto_resolve_on_merge;
                    if should_resolve {
                        if let Some(source) =
                            self.sources.iter().find(|s| s.name() == attempt.source)
                        {
                            match source.resolve_issue(&attempt.issue_id).await {
                                Ok(()) => {
                                    auto_resolved_on_merge += 1;
                                    self.tracker
                                        .mark_resolved(&attempt.source, &attempt.issue_id)?;
                                    if let Some(pr_url) = &attempt.pr_url {
                                        let issue = Issue::new(
                                            &attempt.issue_id,
                                            &attempt.short_id,
                                            "Issue resolved",
                                            pr_url,
                                            &attempt.source,
                                        );
                                        let _ = self.notifier.notify_merged(&issue, pr_url).await;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        component = "watcher",
                                        source = %attempt.source,
                                        issue_id = %attempt.issue_id,
                                        error = %e,
                                        "Failed to resolve issue after PR merge"
                                    );
                                }
                            }
                        }
                    }

                    if !is_cascade {
                        self.maybe_send_fix_shipped_reply(attempt).await;
                    }

                    self.record_feedback_outcome_from_attempt(attempt, Outcome::Merged)
                        .await;

                    self.run_post_merge_learning(attempt).await;

                    if let (Some(review_watcher), Some(pr_url)) =
                        (self.review_watcher.as_ref(), attempt.pr_url.as_ref())
                    {
                        review_watcher.unwatch_pr(pr_url);
                    }

                    let pr_url = attempt.pr_url.as_deref().unwrap_or("");
                    if self.config.cascade.enabled {
                        match self
                            .trigger_cascade(
                                attempt,
                                pr_url,
                                claudear_config::config::CascadeTrigger::Merge,
                            )
                            .await
                        {
                            Ok(()) => {
                                cascade_triggered += 1;
                            }
                            Err(e) => {
                                cascade_failed += 1;
                                tracing::error!(
                                    component = "cascade",
                                    short_id = %attempt.short_id,
                                    error = %e,
                                    "Failed to trigger cascade after merge"
                                );
                            }
                        }
                    }
                }
                Ok(PrStatus::Closed) => {
                    let _claim = self.claim_run();
                    if !self.is_running() {
                        break;
                    }
                    pr_status_closed += 1;
                    if attempt.cascade_repo.is_some() {
                        self.tracker.mark_cascade_pr_outcome(attempt.id, false)?;
                    } else {
                        self.tracker
                            .mark_closed(&attempt.source, &attempt.issue_id)?;
                    }
                    self.tracker
                        .record_activity(
                            &ActivityLogEntry::new(
                                TimelineEventStatus::PrClosed.as_str(),
                                format!("PR closed for {}", attempt.short_id),
                            )
                            .with_source(attempt.source.clone())
                            .with_issue(attempt.issue_id.clone(), attempt.short_id.clone())
                            .with_metadata(json!({ "pr_url": attempt.pr_url })),
                        )
                        .ok();
                    let _ = self
                        .tracker
                        .update_qa_outcome_stats_for_attempt(attempt.id, false);
                    self.record_feedback_outcome_from_attempt(attempt, Outcome::Closed)
                        .await;
                    if let (Some(review_watcher), Some(pr_url)) =
                        (self.review_watcher.as_ref(), attempt.pr_url.as_ref())
                    {
                        review_watcher.unwatch_pr(pr_url);
                    }

                    if let Some(pr_url) = &attempt.pr_url {
                        let issue = Issue::new(
                            &attempt.issue_id,
                            &attempt.short_id,
                            "PR closed without merge",
                            pr_url,
                            &attempt.source,
                        );
                        let _ = self.notifier.notify_closed(&issue, pr_url).await;
                    }
                }
                Ok(_) => {} // Still open
                Err(e) => {
                    pr_status_errors += 1;
                    tracing::debug!(
                        short_id = %attempt.short_id,
                        error = %e,
                        "Failed to check PR status"
                    );
                }
            }
        }

        let cycle_metrics = [
            ("pr_status_checks", pr_status_checks as f64),
            ("pr_status_merged", pr_status_merged as f64),
            ("pr_status_closed", pr_status_closed as f64),
            ("pr_status_errors", pr_status_errors as f64),
            (
                "regression_watches_created",
                regression_watches_created as f64,
            ),
            ("auto_resolved_on_merge", auto_resolved_on_merge as f64),
            ("cascade_triggered", cascade_triggered as f64),
            ("cascade_failed", cascade_failed as f64),
        ];
        for (name, value) in cycle_metrics {
            let metric = ProcessingMetric::new(name, value);
            if let Err(e) = self.tracker.record_metric(&metric) {
                tracing::debug!(error = %e, metric = name, "Failed to record PR lifecycle metric");
            }
        }

        Ok(())
    }

    /// Poll a single source.
    ///
    /// The `deploy_qa` source is skipped: its tips are dispatched only by
    /// [`Self::dispatch_pending_deploy_qa_tips`], so a tip never runs twice.
    async fn poll_source(self: &Arc<Self>, source: &Arc<dyn IssueSource>) -> Result<()> {
        if source.name() == DEPLOY_QA_SOURCE || self.is_rate_limit_paused().await {
            return Ok(());
        }

        let issues = source.fetch_issues().await?;
        tracing::info!(source = source.name(), count = issues.len(), "Found issues");
        let fetched_metric = ProcessingMetric::new("issues_fetched", issues.len() as f64)
            .with_source(source.name().to_string());
        if let Err(e) = self.tracker.record_metric(&fetched_metric) {
            tracing::debug!(error = %e, "Failed to record issues_fetched metric");
        }

        // Get already attempted issue IDs
        let attempted_ids = self.tracker.get_attempted_issue_ids(source.name())?;
        tracing::info!(
            source = source.name(),
            count = attempted_ids.len(),
            "Already attempted issues"
        );

        // Filter and match criteria
        let mut candidates: Vec<(Issue, MatchResult)> = Vec::new();
        let mut seen_issue_ids = HashSet::new();
        let mut duplicate_skipped = 0usize;
        let mut attempted_skipped = 0usize;
        let mut inflight_skipped = 0usize;
        let mut unmatched_skipped = 0usize;

        // Pre-build regex cache for suppression rules (avoids re-compilation per issue)
        let suppression_cache = claudear_analysis::prioritisation::suppression::RegexCache::new(
            &self.config.prioritisation.suppression_rules,
        );

        for issue in issues {
            if !seen_issue_ids.insert(issue.id.clone()) {
                duplicate_skipped = duplicate_skipped.saturating_add(1);
                tracing::debug!(
                    source = source.name(),
                    issue_id = %issue.id,
                    "Skipping duplicate issue in poll payload"
                );
                continue;
            }

            // Skip if already attempted
            if attempted_ids.contains(&issue.id) {
                attempted_skipped = attempted_skipped.saturating_add(1);
                continue;
            }

            // Skip if currently processing
            let processing_key = format!("{}:{}", source.name(), issue.id);
            if self.lock_processing().contains(&processing_key) {
                inflight_skipped = inflight_skipped.saturating_add(1);
                continue;
            }

            // Early suppression check: only runs as fallback when the prioritisation
            // engine is disabled. When enabled, suppression is handled inside prioritise().
            if !self.config.prioritisation.enabled
                && !self.config.prioritisation.suppression_rules.is_empty()
            {
                let suppression =
                    claudear_analysis::prioritisation::suppression::check_issue_with_cache(
                        &self.config.prioritisation.suppression_rules,
                        &issue,
                        &suppression_cache,
                    );
                if suppression.suppressed {
                    tracing::debug!(
                        source = source.name(),
                        issue_id = %issue.short_id,
                        rule = suppression.matched_rule.as_deref().unwrap_or("?"),
                        "Issue suppressed early in poll loop"
                    );
                    continue;
                }
            }

            let match_result = source.matches_criteria(&issue);
            if match_result.matches {
                candidates.push((issue, match_result));
            } else {
                unmatched_skipped = unmatched_skipped.saturating_add(1);
            }
        }

        // Semantic dedup: filter out candidates that are duplicates of already-handled issues
        let mut semantic_duplicate_skipped = 0usize;
        if let Some(ref embedding_service) = self.issue_embedding_service {
            let mut kept = Vec::with_capacity(candidates.len());
            for (issue, match_result) in candidates {
                match embedding_service
                    .check_duplicate(&issue, source.name())
                    .await
                {
                    Ok(Some(duplicate)) => {
                        semantic_duplicate_skipped = semantic_duplicate_skipped.saturating_add(1);
                        let similar_id = duplicate
                            .embedding
                            .short_id
                            .as_deref()
                            .unwrap_or(&duplicate.embedding.issue_id);
                        tracing::info!(
                            short_id = %issue.short_id,
                            similar_to = %similar_id,
                            similarity = %format!("{:.0}%", duplicate.similarity * 100.0),
                            "Skipping semantic duplicate during poll filtering"
                        );
                    }
                    _ => {
                        kept.push((issue, match_result));
                    }
                }
            }
            candidates = kept;
        }

        let candidates_count = candidates.len();
        let matched_metric = ProcessingMetric::new("issues_matched", candidates_count as f64)
            .with_source(source.name().to_string());
        if let Err(e) = self.tracker.record_metric(&matched_metric) {
            tracing::debug!(error = %e, "Failed to record issues_matched metric");
        }

        // Apply per-source max issues per cycle limit (falls back to global)
        let source_max_issues = self.config.max_issues_per_cycle_for(source.name());
        // QA gets its own per-cycle budget so questions (answered read-only, fast) are not
        // starved by a burst of fix requests.
        let source_max_qa = self.config.qa.max_qa_per_cycle;

        // Order candidates first (prioritisation engine or legacy sort), WITHOUT capping yet —
        // the cap(s) are applied after the QA/fix partition below.
        let ordered: Vec<(Issue, MatchResult)> = if self.config.prioritisation.enabled {
            let (prioritised, suppressed) = claudear_analysis::prioritisation::prioritise(
                &self.config.prioritisation,
                candidates,
                self.tracker.as_ref(),
                &std::collections::HashMap::new(),
                self.llm(),
            );

            // Log and record suppressions
            for (issue, result) in &suppressed {
                let rule = result.matched_rule.as_deref().unwrap_or("unknown");
                let reason = result.reason.as_deref().unwrap_or("");
                tracing::info!(
                    source = source.name(),
                    issue_id = %issue.short_id,
                    rule = rule,
                    "Issue suppressed during poll"
                );
                if let Err(e) =
                    self.tracker
                        .record_suppression(source.name(), &issue.id, rule, reason)
                {
                    tracing::debug!(error = %e, "Failed to record suppression");
                }
            }

            // Store severity scores
            for pi in &prioritised {
                if let Err(e) = self.tracker.store_severity_score(
                    source.name(),
                    &pi.issue.id,
                    &pi.severity_score,
                    pi.blast_radius,
                ) {
                    tracing::debug!(error = %e, "Failed to store severity score");
                }
            }

            prioritised
                .into_iter()
                .map(|pi| (pi.issue, pi.match_result))
                .collect()
        } else {
            self.sort_by_priority(&mut candidates);
            candidates
        };

        // Decide each issue's type (QA vs fix) at poll time for QA-eligible chat sources, then
        // apply two independent caps: questions up to `source_max_qa`, fixes up to
        // `source_max_issues`. The decided `Intent` rides along so `IssueProcessor` dispatches
        // directly without re-running the classifier. Non-chat / QA-disabled sources keep the
        // single-cap behaviour with no carried type.
        let qa_split_enabled = self.config.qa.enabled
            && crate::processing::qa_eligible_source(source.name())
            && self.intent_classifier.is_some();

        let to_process: Vec<(Issue, MatchResult, Option<Intent>)> = if qa_split_enabled {
            // Classify each ordered issue via the configured backend. The local LLM
            // backend offloads its synchronous inference to a blocking thread; the
            // agent backend awaits an agent run. Fix-bias on ambiguity / errors,
            // matching `classify_intent`'s contract. Sequential to avoid fanning out
            // many concurrent agent runs.
            let classifier = self
                .intent_classifier
                .clone()
                .expect("intent_classifier present (checked by qa_split_enabled)");
            // The agent-based classifier launches a Claude session per call, so
            // it must count against the global ceiling too; otherwise a worker
            // could spawn classification sessions while every permit is held by
            // fixes. The local-LLM backend runs no Claude session, so it is not
            // gated. This happens before dispatch, so it never double-acquires
            // with the permit `process_issue` holds for an actual run.
            let gate_classification = !self.config.qa.use_llm;
            let mut intents: Vec<Intent> = Vec::with_capacity(ordered.len());
            for (issue, _) in &ordered {
                // Ground the classification in the Discord reply thread (if any) so a
                // follow-up in an ongoing QA conversation ("yes create a pr now") is
                // classified in context and can escalate out of the read-only lane,
                // instead of being judged as an isolated, ambiguous message. Only
                // Claudear's own answers feed routing, so untrusted user text in the
                // thread cannot inject a fix/PR escalation.
                let conversation = crate::processing::assemble_reply_chain(
                    &self.config,
                    self.tracker.as_ref(),
                    issue,
                    crate::processing::TranscriptTrust::ClaudearOnly,
                )
                .await;
                let intent = if gate_classification {
                    let _classify_permit = self.session_limiter.acquire().await.ok();
                    classifier
                        .classify_intent(issue, conversation.as_deref())
                        .await
                } else {
                    classifier
                        .classify_intent(issue, conversation.as_deref())
                        .await
                };
                intents.push(intent.unwrap_or(Intent::Fix));
            }

            // Partition preserving prioritisation order within each bucket. Only
            // pure questions take the QA bucket; bug/security/fix are processed.
            let mut questions: Vec<(Issue, MatchResult, Option<Intent>)> = Vec::new();
            let mut fixes: Vec<(Issue, MatchResult, Option<Intent>)> = Vec::new();
            for ((issue, match_result), intent) in ordered.into_iter().zip(intents) {
                match intent {
                    Intent::Question => {
                        questions.push((issue, match_result, Some(Intent::Question)))
                    }
                    other => fixes.push((issue, match_result, Some(other))),
                }
            }
            questions.truncate(source_max_qa);
            fixes.truncate(source_max_issues);
            tracing::info!(
                source = source.name(),
                questions = questions.len(),
                fixes = fixes.len(),
                max_qa = source_max_qa,
                max_issues = source_max_issues,
                "QA/fix split applied"
            );
            // Questions first so they grab concurrency slots ahead of slow fix runs.
            questions.into_iter().chain(fixes).collect()
        } else {
            ordered
                .into_iter()
                .take(source_max_issues)
                .map(|(issue, match_result)| (issue, match_result, None))
                .collect()
        };

        let to_process_count = to_process.len();
        let queued_short_ids: Vec<String> = to_process
            .iter()
            .map(|(issue, _, _)| issue.short_id.clone())
            .collect();
        let queued_metric = ProcessingMetric::new("issues_queued", to_process_count as f64)
            .with_source(source.name().to_string());
        if let Err(e) = self.tracker.record_metric(&queued_metric) {
            tracing::debug!(error = %e, "Failed to record issues_queued metric");
        }
        self.record_source_decision(
            source.name(),
            "poll_filtering_summary",
            format!("Poll decisions summarized for {}", source.name()),
            json!({
                "fetched": candidates_count + duplicate_skipped + attempted_skipped + inflight_skipped + unmatched_skipped + semantic_duplicate_skipped,
                "matched": candidates_count,
                "queued": to_process_count,
                "deferred": candidates_count.saturating_sub(to_process_count),
                "skipped": {
                    "duplicate": duplicate_skipped,
                    "already_attempted": attempted_skipped,
                    "inflight": inflight_skipped,
                    "unmatched": unmatched_skipped,
                    "semantic_duplicate": semantic_duplicate_skipped,
                },
                "queued_short_ids": queued_short_ids,
                "source_max_issues": source_max_issues,
                "source_max_qa": source_max_qa,
            }),
        );
        if to_process.is_empty() {
            tracing::info!(source = source.name(), "No new issues to process");
            return Ok(());
        }

        let skipped = candidates_count.saturating_sub(to_process_count);
        if skipped > 0 {
            tracing::info!(
                source = source.name(),
                count = to_process.len(),
                deferred = skipped,
                "Will process issues"
            );
        } else {
            tracing::info!(
                source = source.name(),
                count = to_process.len(),
                "Will process issues"
            );
        }

        // In dry-run mode, just show what would be processed
        if self.dry_run {
            use claudear_analysis::inference::resolve_repo_for_issue_with_embedding;

            tracing::info!("");
            tracing::info!("[DRY RUN] Would process the following issues:");
            for (issue, match_result, _intent) in &to_process {
                tracing::info!("  - [{}] {}", issue.short_id, issue.title);
                tracing::info!(
                    "    Priority: {:?}, Reason: {}",
                    match_result.priority,
                    match_result.reason
                );
                tracing::info!("    URL: {}", issue.url);

                // Generate embedding for issue if client is available
                let query_embedding = if let Some(ref client) = self.embedding_client {
                    let issue_text = format!(
                        "{}\n{}",
                        issue.title,
                        issue.description.as_deref().unwrap_or("")
                    );
                    match client.embed(&issue_text).await {
                        Ok(emb) => Some(emb),
                        Err(e) => {
                            tracing::debug!("Failed to embed issue text: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };

                // Show inferred repository (with optional semantic matching)
                let resolution = resolve_repo_for_issue_with_embedding(
                    self.inferrer.as_ref(),
                    issue,
                    Some(&self.tracker),
                    query_embedding.as_deref(),
                );
                match resolution {
                    RepoResolution::Resolved { project_dir, .. } => {
                        tracing::info!("    Repo: {}", project_dir.display());
                    }
                    RepoResolution::Skip { reason } => {
                        tracing::info!("    Repo: SKIP - {}", reason);
                    }
                }
            }
            return Ok(());
        }

        // Notify about urgent issues
        let urgent_issues: Vec<Issue> = to_process
            .iter()
            .filter(|(_, m, _)| m.priority == MatchPriority::Urgent)
            .map(|(i, _, _)| i.clone())
            .collect();

        if !urgent_issues.is_empty() {
            if let Err(e) = self.notifier.notify_urgent_issues(&urgent_issues).await {
                tracing::warn!(
                    source = source.name(),
                    error = %e,
                    "Failed to send urgent issue notification"
                );
            }
        }

        if self.is_rate_limit_paused().await {
            tracing::info!(
                source = source.name(),
                "Skipping queued issues while watcher is paused for Claude rate limit"
            );
            return Ok(());
        }

        // Process issues with rate limiting. Questions and fixes run in independent
        // concurrency lanes so a burst of slow fixes can never starve fast, read-only
        // QA answers. Each lane gates on its own per-source in-flight counter
        // (clamped to 1 to avoid deadlock).
        let fix_max = self.config.max_concurrent_for(source.name()).max(1);
        if self.config.max_concurrent_for(source.name()) == 0 {
            tracing::warn!(
                source = source.name(),
                "max_concurrent_for source evaluated to 0, clamping to 1"
            );
        }
        let qa_max = self.config.qa.max_concurrent.max(1);
        if self.config.qa.max_concurrent == 0 {
            tracing::warn!(
                source = source.name(),
                "qa.max_concurrent evaluated to 0, clamping to 1"
            );
        }

        let (questions, fixes): (Vec<QueuedIssue>, Vec<QueuedIssue>) = to_process
            .into_iter()
            .partition(|(_, _, intent)| matches!(intent, Some(Intent::Question)));

        if questions.is_empty() {
            self.dispatch_lane(source, fixes, fix_max, false).await;
        } else {
            tokio::join!(
                self.dispatch_lane(source, questions, qa_max, true),
                self.dispatch_lane(source, fixes, fix_max, false),
            );
        }

        // Record how many issues were spawned (don't fail main operation if this fails)
        let metric = ProcessingMetric::new("batch_processed", to_process_count as f64)
            .with_source(source.name().to_string());
        if let Err(e) = self.tracker.record_metric(&metric) {
            tracing::warn!(error = %e, "Failed to record batch processing metric");
        }

        Ok(())
    }

    /// Number of active *fix-lane* processing items for a specific source.
    /// QA items are tracked separately; see [`Self::active_qa_for_source`].
    fn active_processing_for_source(&self, source_name: &str) -> usize {
        self.lock_processing().source_count(source_name)
    }

    fn active_qa_for_source(&self, source_name: &str) -> usize {
        self.lock_processing().qa_source_count(source_name)
    }

    /// Resolve on the next processing slot release or [`Self::stop`].
    ///
    /// `notify_waiters` wakes only futures that already exist, so callers take
    /// this before checking whether a slot is free: a release between that
    /// check and the await then still wakes them.
    fn next_slot_release(&self) -> Pin<Box<Notified<'_>>> {
        let mut released = Box::pin(self.slot_available.notified());
        released.as_mut().enable();
        released
    }

    /// Lock the processing set, recovering it from a poisoned lock: its
    /// methods keep keys and counts in step without panicking, so a holder
    /// that panicked cannot leave it inconsistent.
    fn lock_processing(&self) -> MutexGuard<'_, ProcessingState> {
        self.processing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Take the processing slot for `key` in the QA or fix lane, or `None`
    /// when the key is already being processed.
    ///
    /// The claim is built only after the lock is released, because dropping
    /// it takes the same lock.
    fn claim_processing(&self, key: String, is_qa: bool) -> Option<ProcessingClaim<'_>> {
        let inserted = {
            let mut processing = self.lock_processing();
            if is_qa {
                processing.insert_qa(key.clone())
            } else {
                processing.insert(key.clone())
            }
        };
        if !inserted {
            return None;
        }
        self.active_processing.fetch_add(1, Ordering::SeqCst);
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        Some(ProcessingClaim { watcher: self, key })
    }

    fn claim_run(&self) -> RunClaim<'_> {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        RunClaim { watcher: self }
    }

    /// Dispatch one concurrency lane: spawn a processing task per item, gating on the
    /// lane's own per-source in-flight counter so it never blocks (nor is blocked by) the
    /// other lane. `is_qa` selects the QA counter/budget vs the fix counter/budget.
    async fn dispatch_lane(
        self: &Arc<Self>,
        source: &Arc<dyn IssueSource>,
        items: Vec<QueuedIssue>,
        max_concurrent: usize,
        is_qa: bool,
    ) {
        let lane = if is_qa { "qa" } else { "fix" };
        let total = items.len();
        for (i, (issue, match_result, intent)) in items.into_iter().enumerate() {
            if !self.is_running.load(Ordering::SeqCst) {
                break;
            }
            if self.is_rate_limit_paused().await {
                tracing::info!(
                    source = source.name(),
                    lane,
                    "Stopping lane early due to Claude rate-limit pause"
                );
                break;
            }

            // Wait for a concurrency slot in THIS lane.
            loop {
                let released = self.next_slot_release();
                let in_flight = if is_qa {
                    self.active_qa_for_source(source.name())
                } else {
                    self.active_processing_for_source(source.name())
                };
                if in_flight < max_concurrent {
                    break;
                }
                if !self.is_running.load(Ordering::SeqCst) {
                    return;
                }
                if self.is_provider_rate_limited().await {
                    tracing::info!(
                        source = source.name(),
                        lane,
                        "Stopping lane while waiting for slot due to provider rate-limit pause"
                    );
                    return;
                }
                released.await;
            }

            // Carry the trusted routing intent (classified upstream on trusted
            // content) so the answered attempt is stamped with it, letting the
            // reply-chain transcript emit a structural marker instead of
            // re-injecting the generated answer body into the classifier.
            let mut issue = issue;
            if let Some(intent) = intent {
                issue.set_metadata("routing_intent", intent.routing_label());
            }

            // Spawn processing as a background task so poll_source returns promptly and
            // the housekeeping loop (review checks, auto-close, retries) is not starved.
            let watcher = Arc::clone(self);
            let source_clone = Arc::clone(source);
            let handle = tokio::spawn(async move {
                watcher
                    .process_issue(source_clone, issue, match_result, None, None, intent, None)
                    .await;
            });
            self.spawn_handles.lock().await.push(handle);

            // Add delay between starting new issues (skip trailing delay after the last item).
            if i + 1 < total && self.config.processing_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.config.processing_delay_ms)).await;
            }
        }
    }

    /// Check whether an approval request should be sent for the given resolution.
    fn should_request_approval(&self, resolution: &RepoResolution) -> bool {
        if self.config.ask.require_approval {
            return true;
        }
        if let Some(ref threshold_str) = self.config.ask.approval_confidence_threshold {
            if let Ok(threshold) = threshold_str.parse::<Confidence>() {
                let confidence = resolution.confidence().unwrap_or(Confidence::None);
                return confidence <= threshold;
            }
        }
        false
    }

    /// Request human approval before processing an issue.
    ///
    /// Returns the parsed `ApprovalDecision`. A request that times out or
    /// cannot be sent or read is [unanswered](ApprovalDecision::Unanswered)
    /// rather than denied, since only a human's refusal closes the attempt.
    async fn request_approval(
        &self,
        source_name: &str,
        issue: &Issue,
        resolution: &RepoResolution,
    ) -> ApprovalDecision {
        // Build question text with repo + confidence context
        let repo_info = match (resolution.repo_name(), resolution.confidence()) {
            (Some(name), Some(conf)) => format!(" (inferred repo: {}, confidence: {})", name, conf),
            (Some(name), None) => format!(" (repo: {})", name),
            _ => String::new(),
        };

        let ask_request = AskRequest {
            correlation_id: build_correlation_id(&issue.short_id),
            source: source_name.to_string(),
            repo: resolution.repo_name().map(|s| s.to_string()),
            issue_id: issue.id.clone(),
            short_id: issue.short_id.clone(),
            question: BlockingQuestion {
                question: format!(
                    "Should I work on {}: {}?{}",
                    issue.short_id, issue.title, repo_info
                ),
                why: Some("Approval required before processing".to_string()),
                context: issue.description.clone(),
                options: vec![
                    "Yes".to_string(),
                    "No".to_string(),
                    "use <repo_name>".to_string(),
                ],
            },
            asked_at: chrono::Utc::now(),
            target_discord_id: None,
            target_email: None,
            target_slack_id: None,
        };

        let activity = ActivityLogEntry::new(
            "approval_requested",
            format!("Requesting approval for {}", issue.short_id),
        )
        .with_source(source_name.to_string())
        .with_issue(issue.id.clone(), issue.short_id.clone())
        .with_metadata(json!({
            "correlation_id": ask_request.correlation_id,
        }));
        self.tracker.record_activity(&activity).ok();

        let timeout_secs = self
            .config
            .ask
            .approval_timeout_secs
            .unwrap_or(self.config.ask.wait_timeout_secs);

        let reply = send_to_all_and_wait_first_reply(
            Arc::clone(&self.notifier),
            issue,
            &ask_request,
            Duration::from_secs(timeout_secs),
            Duration::from_secs(self.config.ask.poll_interval_secs),
        )
        .await;

        let decision = match reply {
            Ok(Some(ref r)) => parse_approval_reply(&r.answer),
            Ok(None) => {
                tracing::info!(
                    short_id = %issue.short_id,
                    "Approval timed out, skipping issue"
                );
                ApprovalDecision::Unanswered
            }
            Err(ref e) => {
                tracing::warn!(
                    short_id = %issue.short_id,
                    error = %e,
                    "Error requesting approval, skipping issue"
                );
                ApprovalDecision::Unanswered
            }
        };

        let (decision_label, outcome) = match &decision {
            ApprovalDecision::Approved => ("approval_granted", "granted"),
            ApprovalDecision::Redirect { .. } => ("approval_redirect", "redirected"),
            ApprovalDecision::Denied => ("approval_denied", "denied"),
            ApprovalDecision::Unrecognized => ("approval_unrecognized", "reply not understood"),
            ApprovalDecision::Unanswered => ("approval_unanswered", "left unanswered"),
        };
        self.record_issue_decision(
            issue,
            decision_label,
            format!("Approval {} for {}", outcome, issue.short_id),
            json!({
                "correlation_id": ask_request.correlation_id,
                "reply": reply.as_ref().ok().and_then(|r| r.as_ref().map(|r| &r.answer)),
            }),
        );

        decision
    }

    fn record_source_decision(
        &self,
        source: &str,
        decision: &str,
        message: impl Into<String>,
        details: serde_json::Value,
    ) {
        let activity = ActivityLogEntry::new("decision", message.into())
            .with_source(source.to_string())
            .with_metadata(json!({
                "decision": decision,
                "details": details,
            }));
        self.tracker.record_activity(&activity).ok();
    }

    fn record_issue_decision(
        &self,
        issue: &Issue,
        decision: &str,
        message: impl Into<String>,
        details: serde_json::Value,
    ) {
        let activity = ActivityLogEntry::new("decision", message.into())
            .with_source(issue.source.clone())
            .with_issue(issue.id.clone(), issue.short_id.clone())
            .with_metadata(json!({
                "decision": decision,
                "details": details,
            }));
        self.tracker.record_activity(&activity).ok();
    }

    /// Record a decision about `attempt`'s issue on the issue's timeline, for
    /// when only the attempt is at hand.
    fn record_attempt_decision(
        &self,
        attempt: &FixAttempt,
        decision: &str,
        message: impl Into<String>,
        details: serde_json::Value,
    ) {
        let activity = ActivityLogEntry::new("decision", message.into())
            .with_source(attempt.source.clone())
            .with_issue(attempt.issue_id.clone(), attempt.short_id.clone())
            .with_metadata(json!({
                "decision": decision,
                "details": details,
            }));
        self.tracker.record_activity(&activity).ok();
    }

    /// Start QA for pending `deploy_qa` release tips without waiting for it to
    /// finish, returning a handle per started run.
    ///
    /// The `[deploy_qa]` poller calls this after every poll, in every daemon
    /// mode. It is the only in-process path that runs tips: source polls skip
    /// the `deploy_qa` source. Oldest tips go first, at most
    /// `qa.max_concurrent` dispatched runs are in flight, a tip is never
    /// dispatched twice at once, a run that panics or is aborted frees its
    /// slot, and a tip that left pending before its run starts is skipped.
    /// Nothing starts while the watcher is not running (before warm start or
    /// during shutdown) or is rate limited; tips left out stay pending for a
    /// later call.
    pub async fn dispatch_pending_deploy_qa_tips(
        self: &Arc<Self>,
    ) -> Result<Vec<tokio::task::JoinHandle<()>>> {
        let source = self
            .sources
            .iter()
            .find(|source| source.name() == DEPLOY_QA_SOURCE)
            .ok_or_else(|| {
                claudear_core::error::Error::source(DEPLOY_QA_SOURCE, "Unknown source")
            })?;
        if !self.is_running() || self.is_rate_limit_paused().await {
            return Ok(Vec::new());
        }

        Ok(self
            .claim_pending_deploy_qa_tips()?
            .into_iter()
            .map(|claim| {
                let source = Arc::clone(source);
                tokio::spawn(async move {
                    claim
                        .watcher
                        .process_deploy_qa_tip(source, &claim.issue_id)
                        .await;
                })
            })
            .collect())
    }

    /// Claim the oldest pending tips that fit in the free `qa.max_concurrent`
    /// slots, skipping tips an unfinished dispatched run already holds.
    ///
    /// The claims are built only after the lock is released, because dropping
    /// one takes the same lock.
    fn claim_pending_deploy_qa_tips(self: &Arc<Self>) -> Result<Vec<DeployQaTipClaim>> {
        let issue_ids: Vec<String> = {
            let mut dispatched = self.lock_dispatched_deploy_qa_tips();
            let capacity = self
                .config
                .qa
                .max_concurrent
                .max(1)
                .saturating_sub(dispatched.len());
            let issue_ids: Vec<String> = self
                .tracker
                .list_pending_deploy_qa_tips()?
                .into_iter()
                .map(|tip| tip.issue_id)
                .filter(|issue_id| !dispatched.contains(issue_id))
                .take(capacity)
                .collect();
            dispatched.extend(issue_ids.iter().cloned());
            issue_ids
        };
        Ok(issue_ids
            .into_iter()
            .map(|issue_id| DeployQaTipClaim {
                watcher: Arc::clone(self),
                issue_id,
            })
            .collect())
    }

    /// Lock the dispatched tip set, recovering it from a poisoned lock: the set
    /// only gains or loses whole ids, so a holder that panicked cannot leave it
    /// inconsistent.
    fn lock_dispatched_deploy_qa_tips(&self) -> MutexGuard<'_, HashSet<String>> {
        self.dispatched_deploy_qa_tips
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Run QA for one dispatched tip unless it is no longer pending: another
    /// process or path may have started, finished or errored it since it was
    /// listed, and dispatch leaves an errored tip to the retry manager.
    async fn process_deploy_qa_tip(&self, source: Arc<dyn IssueSource>, issue_id: &str) {
        let still_pending = self
            .find_deploy_qa_tip(issue_id)
            .is_some_and(|tip| tip.status == DeployQaTipStatus::Pending);
        if !still_pending {
            tracing::info!(
                component = "deploy_qa",
                issue_id,
                "Skipping dispatched deploy_qa tip that is no longer pending"
            );
            return;
        }
        match source.get_issue(issue_id).await {
            Ok(issue) => {
                let match_result = source.matches_criteria(&issue);
                self.process_issue(
                    source,
                    issue,
                    match_result,
                    None,
                    None,
                    Some(Intent::Question),
                    None,
                )
                .await;
            }
            Err(error) => tracing::warn!(
                component = "deploy_qa",
                issue_id,
                error = %error,
                "Failed to load dispatched deploy_qa tip"
            ),
        }
    }

    /// Look up the tip behind a deploy_qa issue, warning when it cannot be found:
    /// QA never runs without a tip to record its status on.
    fn find_deploy_qa_tip(&self, issue_id: &str) -> Option<DeployQaTip> {
        match self.tracker.get_deploy_qa_tip_by_issue_id(issue_id) {
            Ok(Some(tip)) => Some(tip),
            Ok(None) => {
                tracing::warn!(issue_id, "deploy_qa tip not found; skipping QA run");
                None
            }
            Err(e) => {
                tracing::warn!(issue_id, error = %e, "Failed to load deploy_qa tip");
                None
            }
        }
    }

    /// Claim `tip` for this process by moving it from the status it was read
    /// with to `Running` in a single conditional update, returning whether
    /// the claim succeeded.
    ///
    /// Daemons sharing the database each keep their own processing set, so
    /// only the database can decide which of them runs a tip that all of them
    /// read as runnable. A tip another process claimed, or whose status
    /// changed since it was read, is left to its owner, and a failed claim
    /// never runs QA unclaimed.
    fn claim_deploy_qa_tip(&self, tip: &DeployQaTip, short_id: &str) -> bool {
        match self.tracker.update_deploy_qa_tip_status_if(
            tip.id,
            tip.status,
            DeployQaTipStatus::Running,
            None,
        ) {
            Ok(true) => true,
            Ok(false) => {
                tracing::info!(
                    component = "deploy_qa",
                    short_id,
                    "Skipping deploy_qa tip claimed elsewhere"
                );
                false
            }
            Err(e) => {
                tracing::warn!(
                    component = "deploy_qa",
                    short_id,
                    error = %e,
                    "Failed to claim deploy_qa tip; skipping QA run"
                );
                false
            }
        }
    }

    /// Link the attempt recording this run to the tip it claimed, leaving a
    /// tip that has already left `Running` (a verdict recorded by another
    /// process) untouched.
    fn link_deploy_qa_tip_attempt(&self, tip: &DeployQaTip, attempt_id: Option<i64>) {
        let Some(attempt_id) = attempt_id else {
            return;
        };
        if let Err(e) = self.tracker.update_deploy_qa_tip_status_if(
            tip.id,
            DeployQaTipStatus::Running,
            DeployQaTipStatus::Running,
            Some(attempt_id),
        ) {
            tracing::warn!(
                issue_id = %tip.issue_id,
                attempt_id,
                error = %e,
                "Failed to link attempt to deploy_qa tip"
            );
        }
    }

    /// Mark a tip `Errored` when its attempt ended without the source recording
    /// a verdict, so `skip_if_previous_running` does not block the track forever.
    ///
    /// The transition is a single conditional update because `claudear trigger`
    /// and `claudear action` run in separate processes that can record a
    /// verdict for the same tip at any moment.
    fn release_unfinished_deploy_qa_tip(&self, tip: &DeployQaTip) {
        match self.tracker.update_deploy_qa_tip_status_if(
            tip.id,
            DeployQaTipStatus::Running,
            DeployQaTipStatus::Errored,
            None,
        ) {
            Ok(true) => tracing::warn!(
                issue_id = %tip.issue_id,
                "deploy_qa attempt ended without a verdict; marked tip errored"
            ),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                issue_id = %tip.issue_id,
                error = %e,
                "Failed to release unfinished deploy_qa tip"
            ),
        }
    }

    /// Process a single issue.
    ///
    /// Uses the RepoInferrer engine to determine which repository to use
    /// for fixing the issue. Delegates to the shared `IssueProcessor` pipeline.
    ///
    /// A `deploy_qa` issue is skipped unless its tip
    /// [is runnable](DeployQaTipStatus::is_runnable) and this process
    /// [claims](Self::claim_deploy_qa_tip) it, so no retry, trigger or other
    /// daemon re-runs a tip that is running or already has a verdict.
    ///
    /// Once [`Self::stop`] has been called no run starts and nothing is
    /// recorded, so a run dispatched or triggered before the stop cannot
    /// start after the shutdown drain has finished. A run its `admission`
    /// refuses does not start or record anything either.
    ///
    /// A review rerun is skipped, leaving its attempt untouched, when the
    /// repository of the PR under review cannot be resolved from the index,
    /// so the feedback is retried later instead of being addressed in a
    /// repository inferred from the issue.
    ///
    /// A human refusing approval ends the run with its attempt
    /// [declined](FixAttemptStatus::Declined), so neither an orphan sweep nor
    /// the retry manager runs the issue again and asks once more. An approval
    /// request left unanswered, or answered in a way that is not understood,
    /// ends the run [unanswered](IssueRun::Unanswered) and leaves its attempt
    /// for a later run to ask again.
    ///
    /// From the moment the attempt is recorded until the run ends, a
    /// [`Heartbeat`] shows orphan sweeps in every process that the run is
    /// alive.
    #[expect(clippy::too_many_arguments)]
    async fn process_issue(
        &self,
        source: Arc<dyn IssueSource>,
        issue: Issue,
        match_result: MatchResult,
        review_feedback: Option<String>,
        existing_pr_branch: Option<String>,
        intent: Option<Intent>,
        admission: Option<Admission<'_>>,
    ) -> IssueRun {
        use crate::processing::{IssueProcessor, ProcessingInput, ProcessingOutcome};

        // Retries, IPC and review-feedback triggers pass no intent, and a
        // deploy_qa attempt must never reach the fix pipeline.
        let is_deploy_qa = source.name() == DEPLOY_QA_SOURCE;
        let intent = if is_deploy_qa {
            Some(Intent::Question)
        } else {
            intent
        };
        let deploy_qa_tip = if is_deploy_qa {
            match self.find_deploy_qa_tip(&issue.id) {
                Some(tip) if tip.status.is_runnable() => Some(tip),
                Some(tip) => {
                    tracing::info!(
                        component = "deploy_qa",
                        short_id = %issue.short_id,
                        status = %tip.status,
                        "Skipping deploy_qa tip that is running or has a verdict"
                    );
                    return IssueRun::Skipped;
                }
                None => return IssueRun::Skipped,
            }
        } else {
            None
        };

        if self.is_rate_limit_paused().await {
            tracing::info!(
                short_id = %issue.short_id,
                "Skipping issue processing while watcher is paused for Claude rate limit"
            );
            return IssueRun::Paused;
        }

        let processing_key = format!("{}:{}", source.name(), issue.id);
        // Questions are counted in a dedicated QA lane so they never compete with
        // slow fixes for the same per-source concurrency budget.
        let is_qa = matches!(intent, Some(Intent::Question));

        let Some(_claim) = self.claim_processing(processing_key, is_qa) else {
            tracing::debug!(
                short_id = %issue.short_id,
                "Issue already being processed, skipping"
            );
            return IssueRun::Busy;
        };
        // Checked only once claimed, so either this sees the stop or the drain sees the claim.
        if self.is_stopped() {
            tracing::info!(
                short_id = %issue.short_id,
                "Not starting issue processing because the watcher is stopping"
            );
            return IssueRun::Stopping;
        }
        if admission.is_some_and(|admit| !admit()) {
            return IssueRun::Skipped;
        }

        // Global session ceiling: hold one permit for the whole processing run.
        // The per-source `_claim` above bounds how many of THIS source run at
        // once; this bounds the machine-wide total across every source and path.
        // The per-source claim is already held while we wait here, so no other
        // item for this source can slip past its own budget meanwhile.
        if self.session_limiter.available_permits() == 0 {
            tracing::info!(
                short_id = %issue.short_id,
                limit = self.config.max_concurrent_sessions,
                "Global session ceiling reached, waiting for a free Claude session slot"
            );
        }
        let _session_permit = match self.session_limiter.acquire().await {
            Ok(permit) => permit,
            Err(e) => {
                tracing::error!(short_id = %issue.short_id, error = %e, "Session limiter closed, skipping issue");
                return IssueRun::Skipped;
            }
        };

        // Re-check shutdown after waiting for a permit: the watcher can begin
        // stopping while this run is queued on a saturated ceiling, and a drain
        // must not have new runs start behind it once a slot frees. Bail before
        // claiming the deploy_qa tip so a stopped run never leaves a tip in
        // `Running`.
        if self.is_stopped() {
            tracing::info!(
                short_id = %issue.short_id,
                "Not starting issue processing: watcher began stopping while waiting for a session slot"
            );
            return IssueRun::Stopping;
        }

        // Re-check the rate-limit pause after waiting for a permit: a pause can
        // begin while queued here, and a saturated queue must not keep launching
        // sessions through it. Bail before claiming the deploy_qa tip so a paused
        // run never leaves a tip stuck in `Running`.
        if self.is_rate_limit_paused().await {
            tracing::info!(
                short_id = %issue.short_id,
                "Skipping issue: watcher paused for Claude rate limit while waiting for a session slot"
            );
            return IssueRun::Paused;
        }

        if let Some(ref tip) = deploy_qa_tip {
            if !self.claim_deploy_qa_tip(tip, &issue.short_id) {
                return IssueRun::Skipped;
            }
        }

        let intent_label = match intent {
            Some(Intent::Question) => "question",
            Some(Intent::Bug) => "bug",
            Some(Intent::Security) => "security",
            Some(Intent::Fix) => "fix",
            None => "unclassified",
        };
        tracing::info!("");
        tracing::info!(
            short_id = %issue.short_id,
            title = %issue.title,
            intent = intent_label,
            "Processing issue"
        );
        tracing::info!(short_id = %issue.short_id, reason = %match_result.reason, "Match reason");
        tracing::info!(short_id = %issue.short_id, priority = ?match_result.priority, "Match priority");
        self.record_issue_decision(
            &issue,
            "issue_selected_for_processing",
            format!("Selected {} for processing", issue.short_id),
            json!({
                "match_reason": match_result.reason.clone(),
                "priority": format!("{:?}", match_result.priority),
                "review_feedback_attached": review_feedback.is_some(),
            }),
        );

        // Record/update attempt state early so preflight failures are not retried forever.
        let labels: Vec<String> = issue.get_metadata("labels").unwrap_or_default();
        if let Err(e) = self.tracker.record_attempt_with_labels(
            source.name(),
            &issue.id,
            &issue.short_id,
            &labels,
        ) {
            tracing::error!(short_id = %issue.short_id, error = %e, "Failed to record attempt");
        }
        let _heartbeat = Heartbeat::start(
            Arc::clone(&self.tracker),
            source.name(),
            &issue.id,
            self.liveness.interval,
        );

        self.tracker
            .record_activity(
                &ActivityLogEntry::new(
                    TimelineEventStatus::ProcessingStarted.as_str(),
                    format!("Started processing {}", issue.short_id),
                )
                .with_source(issue.source.clone())
                .with_issue(issue.id.clone(), issue.short_id.clone()),
            )
            .ok();

        // Persist full issue content to the issues table (independent of embeddings)
        {
            let stored = IssueEmbedding::from_issue(&issue);
            if let Err(e) = self.tracker.store_issue(&stored) {
                tracing::debug!(error = %e, "Failed to store issue content");
            }
        }

        // Persist the observed recurrence signal (Sentry event_count / escalating)
        // so the weekly repetitive-issues digest can be built from stored
        // observations rather than a live API call.
        if let Some(event_count) = issue.get_metadata::<i64>("event_count") {
            let is_escalating = issue.get_metadata::<bool>("is_escalating").unwrap_or(false);
            if let Err(e) = self.tracker.record_issue_recurrence(
                source.name(),
                &issue.id,
                event_count,
                is_escalating,
            ) {
                tracing::debug!(error = %e, "Failed to record issue recurrence");
            }
        }

        let attempt_id = self
            .tracker
            .get_attempt(source.name(), &issue.id)
            .ok()
            .flatten()
            .map(|a| a.id);

        if let Some(ref tip) = deploy_qa_tip {
            self.link_deploy_qa_tip_attempt(tip, attempt_id);
        }

        // A review rerun must work in the repo the PR lives in, never one
        // inferred from the issue: inference lands on the same wrong repo a
        // previous run already swapped away from.
        let pr_repo = review_feedback
            .as_ref()
            .and_then(|_| issue.get_metadata::<String>(REVIEW_PR_REPO_KEY));
        let mut resolution = match pr_repo {
            Some(repo) => match resolve_repo_for_cascade(self.inferrer.as_ref(), &repo) {
                resolved @ RepoResolution::Resolved { .. } => resolved,
                RepoResolution::Skip { reason } => {
                    tracing::warn!(
                        short_id = %issue.short_id,
                        repo = %repo,
                        reason = %reason,
                        "PR repo not resolvable, deferring review rerun"
                    );
                    self.record_issue_decision(
                        &issue,
                        REVIEW_RERUN_DEFERRED_DECISION,
                        format!(
                            "Deferred review rerun for {}: PR repository {} is not resolvable",
                            issue.short_id, repo
                        ),
                        json!({ "pr_repo": repo, "reason": reason }),
                    );
                    return IssueRun::Skipped;
                }
            },
            None => resolve_repo_for_issue(self.inferrer.as_ref(), &issue, Some(&self.tracker)),
        };

        match &resolution {
            RepoResolution::Resolved { project_dir, .. } => {
                self.record_issue_decision(
                    &issue,
                    "repo_resolution_selected",
                    format!("Resolved repository for {}", issue.short_id),
                    json!({
                        "repo_name": resolution.repo_name(),
                        "scm_url": resolution.scm_url(),
                        "default_branch": resolution.default_branch(),
                        "project_dir": project_dir.display().to_string(),
                    }),
                );
                self.tracker
                    .record_activity(
                        &ActivityLogEntry::new(
                            TimelineEventStatus::RepoResolved.as_str(),
                            format!("Resolved repository for {}", issue.short_id),
                        )
                        .with_source(issue.source.clone())
                        .with_issue(issue.id.clone(), issue.short_id.clone())
                        .with_metadata(json!({ "repo": resolution.repo_name() })),
                    )
                    .ok();
            }
            RepoResolution::Skip { .. } => {}
        }

        if !is_deploy_qa && self.should_request_approval(&resolution) {
            match self
                .request_approval(source.name(), &issue, &resolution)
                .await
            {
                ApprovalDecision::Approved => { /* continue processing */ }
                ApprovalDecision::Redirect { repo_name } => {
                    let redirected = resolve_repo_for_cascade(self.inferrer.as_ref(), &repo_name);
                    if redirected.is_resolved() {
                        tracing::info!(
                            short_id = %issue.short_id,
                            repo = %repo_name,
                            "Approval redirected to different repo"
                        );
                        resolution = redirected;
                    } else {
                        tracing::warn!(
                            short_id = %issue.short_id,
                            repo = %repo_name,
                            "Redirect repo not found, skipping issue"
                        );
                        return IssueRun::Skipped;
                    }
                }
                ApprovalDecision::Denied => {
                    if let Err(e) = self.tracker.mark_declined(
                        source.name(),
                        &issue.id,
                        APPROVAL_DECLINED_REASON,
                    ) {
                        tracing::error!(
                            short_id = %issue.short_id,
                            error = %e,
                            "Failed to mark attempt declined"
                        );
                    }
                    return IssueRun::Declined;
                }
                ApprovalDecision::Unrecognized | ApprovalDecision::Unanswered => {
                    return IssueRun::Unanswered;
                }
            }
        }

        let issue_short_id = issue.short_id.clone();

        let processor = IssueProcessor {
            config: self.config.clone(),
            tracker: Arc::clone(&self.tracker),
            notifier: Arc::clone(&self.notifier),
            agent: Arc::clone(&self.agent),
            qa_agent: self.qa_agent.clone(),
            inferrer: self.inferrer.clone(),
            embedding_client: self.embedding_client.clone(),
            issue_embedding_service: self.issue_embedding_service.clone(),
            code_search_service: self.code_search_service.clone(),
            discord_search_service: self.discord_search_service.clone(),
            feedback_analyzer: Arc::new(tokio::sync::Mutex::new(
                FeedbackAnalyzer::new().with_tracker(self.tracker.clone()),
            )),
            review_watcher: self.review_watcher.clone(),
            user_registry: self.user_registry.clone(),
            github_client: self.github_client.clone(),
            llm_analyzer: self.llm_analyzer.clone(),
            intent_classifier: self.intent_classifier.clone(),
        };

        let input = ProcessingInput {
            issue,
            source_name: source.name().to_string(),
            match_result,
            resolution,
            attempt_id,
            review_feedback,
            existing_pr_branch,
            intent,
            diagnosis: None,
        };

        let context_provider = crate::processing::SourceContext(source.as_ref());
        let outcome = processor.run(input, &context_provider).await;

        if let Some(ref tip) = deploy_qa_tip {
            self.release_unfinished_deploy_qa_tip(tip);
        }

        if let ProcessingOutcome::Failed { ref error } = outcome {
            if runner::is_rate_limit_error(error) {
                let tmp_issue = Issue::new("", &issue_short_id, "", "", source.name());
                self.pause_until_rate_limit_reset(&tmp_issue, error).await;
            }
        }

        match &outcome {
            ProcessingOutcome::Failed { error } if error.contains("Semantic duplicate of") => {
                IssueRun::Skipped
            }
            _ => IssueRun::Processed,
        }
    }

    async fn clear_rate_limit_pause(&self) {
        let mut pauses = self.rate_limit_pause_until.write().await;
        if !pauses.is_empty() {
            tracing::info!(
                component = "watcher",
                providers = ?pauses.keys().collect::<Vec<_>>(),
                "Cleared transient rate-limit pauses on watcher start"
            );
            pauses.clear();
        }
    }

    /// Check if the default agent provider is rate-limited.
    ///
    /// This is used for top-level poll/housekeeping guards where we don't know the
    /// specific provider yet. Returns `true` only if the default provider is paused.
    pub async fn is_rate_limit_paused(&self) -> bool {
        self.is_provider_rate_limited_for(&self.config.agent.default_provider)
            .await
    }

    /// Check if ANY provider is rate-limited (used for poll-level guards).
    async fn is_provider_rate_limited(&self) -> bool {
        let now = Utc::now();
        let pauses = self.rate_limit_pause_until.read().await;
        pauses.values().any(|until| *until > now)
    }

    /// Check if a specific provider is rate-limited. Cleans up expired entries.
    async fn is_provider_rate_limited_for(&self, provider: &str) -> bool {
        let now = Utc::now();
        let mut pauses = self.rate_limit_pause_until.write().await;
        if let Some(&until) = pauses.get(provider) {
            if until > now {
                return true;
            }
            // Expired — remove and log
            pauses.remove(provider);
            drop(pauses);

            tracing::info!(
                component = "watcher",
                provider = provider,
                reset_at = %until.to_rfc3339(),
                "Provider rate-limit pause expired; resuming"
            );
            let activity = ActivityLogEntry::new(
                "watcher_resumed",
                format!("Watcher resumed after {} rate-limit pause", provider),
            )
            .with_source("watcher".to_string())
            .with_metadata(json!({
                "reason": "provider_rate_limit",
                "provider": provider,
                "resumed_at": now.to_rfc3339(),
                "previous_pause_until": until.to_rfc3339(),
            }));
            self.tracker.record_activity(&activity).ok();
        }
        false
    }

    async fn pause_until_rate_limit_reset(
        &self,
        issue: &Issue,
        error: &str,
    ) -> Option<DateTime<Utc>> {
        let provider = self.agent.name().to_string();
        let now = Utc::now();
        let parsed_reset = Self::extract_rate_limit_reset_time(error, now);
        let fallback_reset = now + chrono::Duration::minutes(15);
        let pause_target = parsed_reset.unwrap_or(fallback_reset) + chrono::Duration::minutes(1);
        let reset_time_parsed = parsed_reset.is_some();

        let mut pauses = self.rate_limit_pause_until.write().await;
        let previous = pauses.get(&provider).copied();
        let effective_until = match previous {
            Some(current) if current >= pause_target => current,
            _ => {
                pauses.insert(provider.clone(), pause_target);
                pause_target
            }
        };
        let changed = previous != Some(effective_until);
        drop(pauses);

        if changed {
            tracing::warn!(
                component = "watcher",
                short_id = %issue.short_id,
                pause_until = %effective_until.to_rfc3339(),
                reset_time_parsed,
                "Pausing provider after rate limit"
            );

            self.record_issue_decision(
                issue,
                "watcher_rate_limit_pause",
                format!(
                    "Pausing provider {} due to rate limit for {}",
                    provider, issue.short_id
                ),
                json!({
                    "provider": provider,
                    "pause_until": effective_until.to_rfc3339(),
                    "reset_time_parsed": reset_time_parsed,
                    "error": crate::processing::truncate_error_for_activity(error),
                }),
            );

            let activity = ActivityLogEntry::new(
                "watcher_paused",
                format!(
                    "Provider {} paused due to rate limit until {}",
                    provider,
                    effective_until.to_rfc3339()
                ),
            )
            .with_source("watcher".to_string())
            .with_issue(issue.id.clone(), issue.short_id.clone())
            .with_metadata(json!({
                "reason": "provider_rate_limit",
                "provider": provider,
                "pause_until": effective_until.to_rfc3339(),
                "reset_time_parsed": reset_time_parsed,
                "fallback_minutes": if reset_time_parsed { None::<u32> } else { Some(15) },
            }));
            self.tracker.record_activity(&activity).ok();
        }

        Some(effective_until)
    }

    fn extract_rate_limit_reset_time(error: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        Self::extract_rate_limit_reset_from_resets_at(error, now)
            .or_else(|| Self::extract_rate_limit_reset_from_usage_limit(error, now))
            .or_else(|| Self::extract_rate_limit_reset_from_banner_utc(error, now))
            .or_else(|| Self::extract_rate_limit_reset_from_retry_after(error, now))
    }

    fn extract_rate_limit_reset_from_resets_at(
        error: &str,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let key = "\"resetsAt\"";
        let mut start = 0usize;

        while let Some(offset) = error[start..].find(key) {
            let idx = start + offset + key.len();
            let after_key = &error[idx..];
            if let Some(colon) = after_key.find(':') {
                let after_colon = after_key[colon + 1..].trim_start();
                if let Some(after_quote) = after_colon.strip_prefix('"') {
                    if let Some(end_quote) = after_quote.find('"') {
                        let value = &after_quote[..end_quote];
                        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value) {
                            return Some(parsed.with_timezone(&Utc));
                        }
                    }
                } else if let Some(reset) = Self::parse_leading_digits(after_colon)
                    .and_then(|seconds| Self::plausible_rate_limit_reset(seconds, now))
                {
                    return Some(reset);
                }
            }
            start = idx;
        }

        None
    }

    fn extract_rate_limit_reset_from_usage_limit(
        error: &str,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let marker = "usage limit reached|";
        let lower = error.to_ascii_lowercase();
        let idx = lower.find(marker)?;
        let seconds = Self::parse_leading_digits(&lower[idx + marker.len()..])?;
        Self::plausible_rate_limit_reset(seconds, now)
    }

    fn plausible_rate_limit_reset(epoch_seconds: i64, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let reset = DateTime::<Utc>::from_timestamp(epoch_seconds, 0)?;
        let horizon = now + chrono::Duration::days(MAX_RATE_LIMIT_RESET_DAYS_AHEAD);
        (reset > now && reset <= horizon).then_some(reset)
    }

    fn parse_leading_digits(text: &str) -> Option<i64> {
        let end = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        text[..end].parse().ok()
    }

    fn extract_rate_limit_reset_from_banner_utc(
        error: &str,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        // Example banner: "You've hit your limit · resets 6am (UTC)"
        let lower = error.to_ascii_lowercase();
        let idx = lower.find("resets")?;
        let mut tail = lower[idx + "resets".len()..].trim_start();

        let hour_end = tail.find(|c: char| !c.is_ascii_digit())?;
        let hour_12: u32 = tail[..hour_end].parse().ok()?;
        if hour_12 == 0 || hour_12 > 12 {
            return None;
        }
        tail = &tail[hour_end..];

        let mut minute: u32 = 0;
        if let Some(rest) = tail.strip_prefix(':') {
            let minute_end = rest.find(|c: char| !c.is_ascii_digit())?;
            minute = rest[..minute_end].parse().ok()?;
            tail = &rest[minute_end..];
        }
        if minute > 59 {
            return None;
        }

        tail = tail.trim_start();
        let meridiem = if let Some(rest) = tail.strip_prefix("am") {
            tail = rest;
            "am"
        } else {
            let rest = tail.strip_prefix("pm")?;
            tail = rest;
            "pm"
        };

        tail = tail.trim_start();
        if !tail.starts_with("(utc)") {
            return None;
        }

        let mut hour_24 = hour_12 % 12;
        if meridiem == "pm" {
            hour_24 += 12;
        }

        let date = now.date_naive();
        let mut reset =
            DateTime::<Utc>::from_naive_utc_and_offset(date.and_hms_opt(hour_24, minute, 0)?, Utc);
        if reset <= now {
            reset += chrono::Duration::days(1);
        }
        Some(reset)
    }

    fn extract_rate_limit_reset_from_retry_after(
        error: &str,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let lower = error.to_ascii_lowercase();
        let idx = lower.find("retry-after")?;
        let tail = &lower[idx + "retry-after".len()..];
        let digits_start = tail.find(|c: char| c.is_ascii_digit())?;
        let seconds = Self::parse_leading_digits(&tail[digits_start..])?;
        if seconds <= 0 {
            return None;
        }
        Some(now + chrono::Duration::seconds(seconds))
    }

    /// Record a feedback outcome from an attempt (when we lack the Issue object).
    /// Reconstructs a minimal Issue from attempt data and retrieves prompt from executions.
    async fn record_feedback_outcome_from_attempt(
        &self,
        attempt: &claudear_core::types::FixAttempt,
        outcome: Outcome,
    ) {
        let issue = Issue::new(
            &attempt.issue_id,
            &attempt.short_id,
            format!("Issue {}", attempt.short_id),
            String::new(),
            &attempt.source,
        );

        crate::processing::record_feedback_outcome(
            &self.tracker,
            self.embedding_client.as_deref(),
            self.issue_embedding_service.as_deref(),
            &self.feedback_analyzer,
            &attempt.source,
            &issue,
            outcome,
        )
        .await;
    }

    /// Run periodic learning subsystem tasks (QA promotion, cluster detection).
    pub async fn run_periodic_learning(&self) {
        let learning = &self.config.learning;

        // System 3: Promote repeated Q&A answers to standing instructions
        if learning.qa_promotion {
            match claudear_analysis::learning::QaPromoter::scan_and_promote(
                self.tracker.as_ref(),
                self.embedding_client.as_deref(),
                learning.qa_promotion_threshold,
                0.8,
            ) {
                Ok(0) => {}
                Ok(n) => {
                    tracing::info!(
                        promoted = n,
                        "Promoted Q&A answers to standing instructions"
                    );
                    self.record_source_decision(
                        "system",
                        "qa_promotion_completed",
                        format!("Promoted {} Q&A answers to standing instructions", n),
                        json!({ "promoted_count": n }),
                    );
                }
                Err(e) => tracing::debug!(error = %e, "Q&A promotion scan failed"),
            }
        }

        // System 8: Detect clusters of correlated issues
        if learning.cluster_detection {
            for source in &self.sources {
                match claudear_analysis::learning::ClusterDetector::detect_clusters(
                    self.tracker.as_ref(),
                    source.name(),
                    learning.cluster_window_minutes as i64,
                    learning.min_cluster_size,
                ) {
                    Ok(clusters) if !clusters.is_empty() => {
                        for cluster in &clusters {
                            match self.tracker.store_issue_cluster(cluster) {
                                Ok(_) => {
                                    tracing::info!(
                                        source = source.name(),
                                        issues = cluster.issue_ids.len(),
                                        "Detected and stored issue cluster"
                                    );
                                }
                                Err(e) => {
                                    // UNIQUE constraint violation means cluster already stored
                                    tracing::debug!(error = %e, "Failed to store cluster (may already exist)");
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, source = source.name(), "Cluster detection failed")
                    }
                }

                // Check if existing active clusters have been resolved
                if let Ok(active_clusters) = self.tracker.get_active_clusters(source.name()) {
                    for cluster in &active_clusters {
                        match claudear_analysis::learning::ClusterDetector::check_cluster_resolution(
                            self.tracker.as_ref(),
                            cluster,
                        ) {
                            Ok(true) => {
                                // Find the merged issue to record as resolver
                                let resolver = cluster.issue_ids.iter().find_map(|issue_id| {
                                    self.tracker
                                        .get_attempt(&cluster.source, issue_id)
                                        .ok()
                                        .flatten()
                                        .and_then(|a| {
                                            if a.status == FixAttemptStatus::Merged {
                                                Some((issue_id.clone(), a.id))
                                            } else {
                                                None
                                            }
                                        })
                                });
                                let (resolved_issue, resolved_attempt) =
                                    resolver.unwrap_or_else(|| ("unknown".to_string(), 0));
                                if let Err(e) = self.tracker.update_cluster_resolution(
                                    cluster.id,
                                    &resolved_issue,
                                    resolved_attempt,
                                ) {
                                    tracing::debug!(error = %e, "Failed to mark cluster resolved");
                                } else {
                                    tracing::info!(
                                        source = source.name(),
                                        cluster_key = %cluster.cluster_key,
                                        resolved_by = %resolved_issue,
                                        "Cluster resolved (at least one issue merged)"
                                    );
                                }
                            }
                            Ok(false) => {}
                            Err(e) => {
                                tracing::debug!(error = %e, "Failed to check cluster resolution")
                            }
                        }
                    }
                }
            }
        }

        // Cross-repo failure correlation
        if learning.cross_repo_correlation {
            match claudear_analysis::learning::CrossRepoCorrelator::detect_correlations(
                self.tracker.as_ref(),
                learning.cross_repo_window_hours,
            ) {
                Ok(mut insights) if !insights.is_empty() => {
                    // Build context summary from the detected insights for LLM enrichment
                    let issues_context: String = insights
                        .iter()
                        .map(|i| format!("{} \u{2194} {}: {}", i.repo_a, i.repo_b, i.message))
                        .collect::<Vec<_>>()
                        .join("\n");
                    // Enrich with LLM explanations if available
                    claudear_analysis::learning::CrossRepoCorrelator::enrich_with_llm(
                        &mut insights,
                        self.llm(),
                        &issues_context,
                    );
                    for insight in &insights {
                        tracing::info!(
                            upstream = %insight.repo_a,
                            downstream = %insight.repo_b,
                            count = insight.correlation_count,
                            "Cross-repo correlation detected"
                        );
                    }
                    self.record_source_decision(
                        "system",
                        "cross_repo_correlation",
                        format!("Detected {} cross-repo correlations", insights.len()),
                        serde_json::json!({ "correlation_count": insights.len() }),
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "Cross-repo correlation detection failed");
                }
            }
        }
    }

    /// Run post-merge learning hooks (extract learnings, analyze diff, compute quality score).
    async fn run_post_merge_learning(&self, attempt: &claudear_core::types::FixAttempt) {
        let learning = &self.config.learning;

        // System 1: Auto-extract learnings from execution logs
        if learning.auto_extract_learnings {
            if let Ok(execs) = self.tracker.get_executions_for_attempt(attempt.id) {
                if let Some(exec) = execs.first() {
                    if let Some(ref log_path) = exec.stdout_log_path {
                        let path = std::path::Path::new(log_path);
                        if path.exists() {
                            match claudear_analysis::learning::LogExtractor::extract_with_llm(
                                path,
                                self.llm(),
                            ) {
                                Ok(learnings) => {
                                    let summary =
                                        claudear_analysis::learning::LogExtractor::summarize(
                                            &learnings,
                                        );
                                    // Store learnings on the feedback outcome
                                    if let Ok(Some(outcome)) =
                                        self.tracker.get_feedback_outcome_by_attempt(attempt.id)
                                    {
                                        if let Err(e) = self
                                            .tracker
                                            .update_feedback_learnings(outcome.id, &summary)
                                        {
                                            tracing::warn!(error = %e, "Failed to store extracted learnings");
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!(error = %e, "Failed to extract learnings from log")
                                }
                            }
                        }
                    }
                }
            }
        }

        // System 2: Analyze PR diff
        if learning.diff_analysis {
            if let (Some(github), Some(repo), Some(pr_number)) = (
                self.github_client.as_ref(),
                attempt.scm_repo.as_deref(),
                attempt.scm_pr_number,
            ) {
                let pr_url = attempt.pr_url.as_deref().unwrap_or("");
                match github.get_pr_diff(repo, pr_number).await {
                    Ok(diff) => {
                        let analysis = claudear_analysis::learning::DiffAnalyzer::analyze_diff(
                            &diff, attempt.id, pr_url, repo, pr_number,
                        );

                        if let Err(e) = self.tracker.store_diff_analysis(&analysis) {
                            tracing::warn!(error = %e, "Failed to store diff analysis");
                        }

                        // Update prs record with files_changed from diff analysis
                        if let Ok(Some(mut pr_record)) = self.tracker.get_pr(pr_url) {
                            pr_record.files_changed = Some(analysis.files_changed.len() as i64);
                            if let Err(e) = self.tracker.upsert_pr(&pr_record) {
                                tracing::warn!(error = %e, "Failed to update PR files_changed");
                            }
                        }

                        // Feed into repo knowledge
                        if learning.repo_knowledge {
                            if let Err(e) =
                                claudear_analysis::learning::RepoKnowledgeManager::learn_from_diff(
                                    self.tracker.as_ref(),
                                    repo,
                                    &analysis,
                                )
                            {
                                tracing::warn!(error = %e, "Failed to learn from diff");
                            }
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "Failed to fetch PR diff for analysis"),
                }
            }
        }

        // System 7: Compute quality score
        if learning.quality_scoring {
            if let Some(ref pr_url) = attempt.pr_url {
                if let Ok(Some(pr_record)) = self.tracker.get_pr(pr_url) {
                    let quality = claudear_analysis::learning::QualityScorer::compute(&pr_record);
                    if let Err(e) = self
                        .tracker
                        .update_pr_fix_quality_score(pr_url, quality.score)
                    {
                        tracing::warn!(error = %e, "Failed to store quality score");
                    }
                }
            }
        }

        // System 9: Auto-generate AGENT.md from accumulated knowledge
        if learning.auto_agent_md {
            if let Some(repo) = attempt.scm_repo.as_deref() {
                let knowledge = self.tracker.get_repo_knowledge(repo).unwrap_or_default();
                let instructions = self
                    .tracker
                    .get_promoted_instructions(repo)
                    .unwrap_or_default();
                if !knowledge.is_empty() || !instructions.is_empty() {
                    let agent_md =
                        claudear_analysis::learning::RepoKnowledgeManager::generate_agent_md(
                            &knowledge,
                            &instructions,
                        );
                    let agent_md_path = self.config.workspace.join(repo).join("AGENT.md");
                    if let Some(parent) = agent_md_path.parent() {
                        if parent.exists() {
                            if let Err(e) = std::fs::write(&agent_md_path, &agent_md) {
                                tracing::debug!(error = %e, path = ?agent_md_path, "Failed to write AGENT.md");
                            } else {
                                tracing::info!(
                                    repo = repo,
                                    path = ?agent_md_path,
                                    "Generated AGENT.md from accumulated knowledge"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Manually trigger processing for a specific issue.
    pub async fn trigger_issue(&self, source_name: &str, issue_id: &str) -> Result<()> {
        self.trigger_issue_with_feedback(
            source_name,
            issue_id,
            None,
            None,
            Some(MANUAL_TRIGGER.into()),
        )
        .await
    }

    /// Manually trigger processing for a specific issue with optional review feedback context.
    ///
    /// Refused with [`STOPPING_REFUSAL`] once [`Self::stop`] has been called,
    /// including when the stop comes while the issue is being loaded.
    pub async fn trigger_issue_with_feedback(
        &self,
        source_name: &str,
        issue_id: &str,
        review_feedback: Option<String>,
        existing_pr_branch: Option<String>,
        trigger_reason: Option<String>,
    ) -> Result<()> {
        self.trigger_issue_inner(
            source_name,
            issue_id,
            review_feedback,
            existing_pr_branch,
            trigger_reason,
            None,
        )
        .await
    }

    /// `pr_repo` pins a review rerun to the repo of the PR under review. It must
    /// come from the reviewed attempt: a cascade row shares its parent's issue id.
    ///
    /// Fails when the run stops before processing the issue. A run whose
    /// approval a human refused counts as carried out, so a review rerun that
    /// was declined keeps its review cycle and is not tried again.
    async fn trigger_issue_inner(
        &self,
        source_name: &str,
        issue_id: &str,
        review_feedback: Option<String>,
        existing_pr_branch: Option<String>,
        trigger_reason: Option<String>,
        pr_repo: Option<&str>,
    ) -> Result<()> {
        let source = self
            .sources
            .iter()
            .find(|s| s.name() == source_name)
            .ok_or_else(|| claudear_core::error::Error::source(source_name, "Unknown source"))?;
        if self.is_stopped() {
            return Err(claudear_core::error::Error::Other(
                STOPPING_REFUSAL.to_string(),
            ));
        }

        tracing::info!(
            component = "watcher",
            source = source_name,
            issue_id = issue_id,
            "Manually triggering issue"
        );

        let mut issue = source.get_issue(issue_id).await?;
        let match_result = MatchResult::matched(MANUAL_TRIGGER, MatchPriority::Urgent);

        if let Some(reason) = trigger_reason {
            issue.set_metadata("trigger_reason", reason);
        }
        if let Some(repo) = pr_repo {
            issue.set_metadata(REVIEW_PR_REPO_KEY, repo);
        }

        let run = self
            .process_issue(
                Arc::clone(source),
                issue,
                match_result,
                review_feedback,
                existing_pr_branch,
                None,
                None,
            )
            .await;
        match run {
            IssueRun::Processed | IssueRun::Declined => Ok(()),
            IssueRun::Unanswered | IssueRun::Skipped | IssueRun::Busy | IssueRun::Paused => {
                Err(claudear_core::error::Error::source(
                    source_name,
                    format!(
                        "Issue {} is already being processed; trigger deferred",
                        issue_id
                    ),
                ))
            }
            IssueRun::Stopping => Err(claudear_core::error::Error::Other(
                STOPPING_REFUSAL.to_string(),
            )),
        }
    }

    /// Run a single, explicitly-chosen action (reply/verify/resolve) against an
    /// issue, bypassing classification. Backs the `claudear action ...` CLI.
    ///
    /// Every action is refused for `deploy_qa` issues, which only run live QA:
    /// `resolve` would enter the fix pipeline, and `reply` / `verify` would post
    /// through the source, which records any comment as the release's QA verdict.
    pub async fn run_action(
        &self,
        action: ActionKind,
        source_name: &str,
        issue_id: &str,
    ) -> Result<crate::processing::ProcessingOutcome> {
        use crate::processing::{refuse_live_qa_action, IssueProcessor, ProcessingInput};

        let source = self
            .sources
            .iter()
            .find(|s| s.name() == source_name)
            .ok_or_else(|| claudear_core::error::Error::source(source_name, "Unknown source"))?;

        if source.name() == DEPLOY_QA_SOURCE {
            tracing::warn!(
                component = "watcher",
                source = source_name,
                issue_id = issue_id,
                %action,
                "Refusing manual action for live-QA deploy_qa issue"
            );
            return Ok(refuse_live_qa_action(action));
        }

        let issue = source.get_issue(issue_id).await?;
        let match_result = MatchResult::matched("Manual action", MatchPriority::Urgent);
        let resolution =
            resolve_repo_for_issue(self.inferrer.as_ref(), &issue, Some(&self.tracker));
        let attempt_id = self
            .tracker
            .get_attempt(source.name(), &issue.id)
            .ok()
            .flatten()
            .map(|a| a.id);

        let processor = IssueProcessor {
            config: self.config.clone(),
            tracker: Arc::clone(&self.tracker),
            notifier: Arc::clone(&self.notifier),
            agent: Arc::clone(&self.agent),
            qa_agent: self.qa_agent.clone(),
            inferrer: self.inferrer.clone(),
            embedding_client: self.embedding_client.clone(),
            issue_embedding_service: self.issue_embedding_service.clone(),
            code_search_service: self.code_search_service.clone(),
            discord_search_service: self.discord_search_service.clone(),
            feedback_analyzer: Arc::new(tokio::sync::Mutex::new(
                FeedbackAnalyzer::new().with_tracker(self.tracker.clone()),
            )),
            review_watcher: self.review_watcher.clone(),
            user_registry: self.user_registry.clone(),
            github_client: self.github_client.clone(),
            llm_analyzer: self.llm_analyzer.clone(),
            intent_classifier: self.intent_classifier.clone(),
        };

        let input = ProcessingInput {
            issue,
            source_name: source.name().to_string(),
            match_result,
            resolution,
            attempt_id,
            review_feedback: None,
            existing_pr_branch: None,
            intent: None,
            diagnosis: None,
        };

        let context_provider = crate::processing::SourceContext(source.as_ref());
        Ok(processor
            .run_single_action(action, input, &context_provider)
            .await)
    }

    /// Reset a failed attempt to allow retry.
    pub fn reset_attempt(&self, source_name: &str, issue_id: &str) -> Result<()> {
        self.tracker.reset_attempt(source_name, issue_id)?;
        tracing::info!(
            component = "watcher",
            source = source_name,
            issue_id = issue_id,
            "Reset attempt"
        );
        Ok(())
    }

    /// Get statistics.
    pub fn get_stats(&self) -> Result<FixAttemptStats> {
        self.tracker.get_stats()
    }

    /// Check for PRs that should be auto-closed due to issue state changes.
    ///
    /// This checks all pending PRs and closes any whose source issue has been
    /// resolved, cancelled, or otherwise moved to a terminal state. Once
    /// [`Self::stop`] has been called, the remaining PRs are left for the next
    /// start.
    pub async fn check_and_auto_close_prs(&self) -> Result<Vec<String>> {
        let pending_prs = self.tracker.get_pending_prs()?;
        let mut auto_closed = Vec::new();

        for attempt in pending_prs {
            if self.is_stopped() {
                break;
            }
            if let Some(source) = self.sources.iter().find(|s| s.name() == attempt.source) {
                match source.get_issue_status(&attempt.issue_id).await {
                    Ok(status) if source.is_terminal_status(&status) => {
                        let _claim = self.claim_run();
                        if self.is_stopped() {
                            break;
                        }
                        tracing::info!(
                            source = %attempt.source,
                            issue_id = %attempt.issue_id,
                            short_id = %attempt.short_id,
                            status = %status,
                            "Auto-closing PR: issue reached terminal state"
                        );

                        let activity = ActivityLogEntry::new(
                            "pr_auto_closed",
                            format!(
                                "PR auto-closed: issue {} is now {}",
                                attempt.short_id, status
                            ),
                        )
                        .with_source(attempt.source.clone())
                        .with_issue(attempt.issue_id.clone(), attempt.short_id.clone())
                        .with_metadata(json!({
                            "pr_url": attempt.pr_url,
                            "issue_status": status,
                            "reason": "issue_terminal_state"
                        }));
                        let _ = self.tracker.record_activity(&activity);

                        if let Err(e) = self.tracker.mark_closed(&attempt.source, &attempt.issue_id)
                        {
                            tracing::warn!(
                                error = %e,
                                "Failed to mark attempt as closed"
                            );
                        }
                        let _ = self
                            .tracker
                            .update_qa_outcome_stats_for_attempt(attempt.id, false);

                        let issue = Issue::new(
                            &attempt.issue_id,
                            &attempt.short_id,
                            format!("Issue {} (auto-closed)", attempt.short_id),
                            attempt.pr_url.clone().unwrap_or_default(),
                            &attempt.source,
                        );
                        let _ = self
                            .notifier
                            .notify_failed(
                                &issue,
                                &format!("PR auto-closed: source issue is now {}", status),
                            )
                            .await;

                        self.record_feedback_outcome_from_attempt(&attempt, Outcome::Closed)
                            .await;

                        if let (Some(review_watcher), Some(pr_url)) =
                            (self.review_watcher.as_ref(), attempt.pr_url.as_ref())
                        {
                            review_watcher.unwatch_pr(pr_url);
                        }

                        if let Some(ref url) = attempt.pr_url {
                            auto_closed.push(url.clone());
                        }
                    }
                    Ok(_) => {} // Issue still active
                    Err(e) => {
                        tracing::debug!(
                            source = %attempt.source,
                            issue_id = %attempt.issue_id,
                            error = %e,
                            "Failed to check issue status for auto-close"
                        );
                    }
                }
            }
        }

        if !auto_closed.is_empty() {
            tracing::info!(
                count = auto_closed.len(),
                "Auto-closed PRs due to issue state changes"
            );
        }

        Ok(auto_closed)
    }

    /// Sort issues by priority for processing order.
    fn sort_by_priority(&self, issues: &mut [(Issue, MatchResult)]) {
        issues.sort_by(|a, b| {
            // Sort by match priority first
            let priority_cmp = priority_order(&a.1.priority).cmp(&priority_order(&b.1.priority));
            if priority_cmp != std::cmp::Ordering::Equal {
                return priority_cmp;
            }

            // Then by issue priority
            issue_priority_order(&b.0.priority).cmp(&issue_priority_order(&a.0.priority))
        });
    }
}

fn priority_order(p: &MatchPriority) -> u8 {
    match p {
        MatchPriority::Urgent => 0,
        MatchPriority::High => 1,
        MatchPriority::Normal => 2,
        MatchPriority::Low => 3,
    }
}

fn issue_priority_order(p: &claudear_core::types::IssuePriority) -> u8 {
    match p {
        claudear_core::types::IssuePriority::Critical => 4,
        claudear_core::types::IssuePriority::High => 3,
        claudear_core::types::IssuePriority::Medium => 2,
        claudear_core::types::IssuePriority::Low => 1,
        claudear_core::types::IssuePriority::None => 0,
    }
}

/// Result of seeding operation.
#[derive(Debug, Default)]
pub struct SeedResult {
    pub total: usize,
    pub by_source: std::collections::HashMap<String, usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use claudear_analysis::deploy_qa::{VERDICT_ALL_VERIFIED, VERDICT_FAIL, VERDICT_PREFIX};
    use claudear_config::config::{
        CascadeRule, CascadeTrigger, DeployQaConfig, DeployQaTrackConfig,
    };
    use claudear_core::types::{IndexedRepo, IssuePriority, RepoIndex};
    use claudear_integrations::notifier::Notifier;
    use claudear_integrations::reports::Report;
    use claudear_integrations::scm::{
        CodeReview, PrInfo, RemoteRepo, ReviewComment, ReviewUser, ScmRelease,
    };
    use claudear_integrations::source::{DeployQaSource, IssueSource};
    use claudear_storage::{ActivityStore, AttemptTracker, SqliteTracker};
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    // Mock notifier for testing
    struct MockNotifier {
        enabled: bool,
        call_count: AtomicUsize,
        fail_urgent_notify: bool,
    }

    impl MockNotifier {
        fn new(enabled: bool) -> Self {
            Self {
                enabled,
                call_count: AtomicUsize::new(0),
                fail_urgent_notify: false,
            }
        }

        fn with_urgent_failure(enabled: bool) -> Self {
            Self {
                enabled,
                call_count: AtomicUsize::new(0),
                fail_urgent_notify: true,
            }
        }

        fn get_call_count(&self) -> usize {
            self.call_count.load(AtomicOrdering::SeqCst)
        }
    }

    #[test]
    fn test_extract_rate_limit_reset_from_banner_utc_same_day() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T04:11:25Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "You've hit your limit · resets 6am (UTC)";
        let parsed = Watcher::extract_rate_limit_reset_from_banner_utc(msg, now).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-02-23T06:00:00+00:00");
    }

    #[test]
    fn test_extract_rate_limit_reset_from_banner_utc_next_day_when_past_reset() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T23:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "You've hit your limit · resets 6am (UTC)";
        let parsed = Watcher::extract_rate_limit_reset_from_banner_utc(msg, now).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-02-24T06:00:00+00:00");
    }

    #[async_trait]
    impl Notifier for MockNotifier {
        fn name(&self) -> &str {
            "mock"
        }
        fn is_enabled(&self) -> bool {
            self.enabled
        }
        async fn notify_start(&self, _issue: &Issue) -> Result<()> {
            self.call_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
        async fn notify_success(&self, _issue: &Issue, _pr_url: &str) -> Result<()> {
            self.call_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
        async fn notify_completed(&self, _issue: &Issue) -> Result<()> {
            self.call_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
        async fn notify_failed(&self, _issue: &Issue, _error: &str) -> Result<()> {
            self.call_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
        async fn notify_status(&self, _message: &str) -> Result<()> {
            self.call_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
        async fn notify_urgent_issues(&self, _issues: &[Issue]) -> Result<()> {
            if self.fail_urgent_notify {
                return Err(claudear_core::error::Error::notifier(
                    "mock",
                    "urgent notify failed",
                ));
            }
            self.call_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
        async fn notify_merged(&self, _issue: &Issue, _pr_url: &str) -> Result<()> {
            self.call_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
        async fn notify_report(&self, _report: &Report) -> Result<()> {
            self.call_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
    }

    // Mock source for testing
    struct MockSource {
        name: String,
        issues: Vec<Issue>,
        match_priority: MatchPriority,
        issue_status_calls: AtomicUsize,
        fetch_calls: AtomicUsize,
    }

    impl MockSource {
        fn new(name: &str) -> Self {
            Self::with_issues(name, vec![])
        }

        fn with_issues(name: &str, issues: Vec<Issue>) -> Self {
            Self::with_priority(name, issues, MatchPriority::Normal)
        }

        fn with_priority(name: &str, issues: Vec<Issue>, match_priority: MatchPriority) -> Self {
            Self {
                name: name.to_string(),
                issues,
                match_priority,
                issue_status_calls: AtomicUsize::new(0),
                fetch_calls: AtomicUsize::new(0),
            }
        }

        fn issue_status_call_count(&self) -> usize {
            self.issue_status_calls.load(AtomicOrdering::SeqCst)
        }

        fn fetch_call_count(&self) -> usize {
            self.fetch_calls.load(AtomicOrdering::SeqCst)
        }
    }

    #[async_trait]
    impl IssueSource for MockSource {
        fn name(&self) -> &str {
            &self.name
        }
        fn display_name(&self) -> &str {
            &self.name
        }
        async fn fetch_issues(&self) -> Result<Vec<Issue>> {
            self.fetch_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(self.issues.clone())
        }
        fn matches_criteria(&self, _issue: &Issue) -> MatchResult {
            MatchResult::matched("Mock match", self.match_priority)
        }
        async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
            Ok(format!("Context for {}", issue.short_id))
        }
        async fn get_issue(&self, id: &str) -> Result<Issue> {
            self.issues
                .iter()
                .find(|i| i.id == id)
                .cloned()
                .ok_or_else(|| claudear_core::error::Error::source(&self.name, "Issue not found"))
        }
        async fn get_issue_status(&self, issue_id: &str) -> Result<String> {
            self.issue_status_calls.fetch_add(1, AtomicOrdering::SeqCst);
            let issue = self.get_issue(issue_id).await?;
            Ok(format!("{:?}", issue.status))
        }
    }

    fn test_issue() -> Issue {
        Issue::new(
            "123",
            "TEST-123",
            "Test Issue",
            "https://example.com",
            "test",
        )
    }

    fn test_issue_with_priority(id: &str, priority: IssuePriority) -> Issue {
        let mut issue = Issue::new(
            id,
            format!("TEST-{}", id),
            "Test",
            "https://example.com",
            "test",
        );
        issue.priority = priority;
        issue
    }

    fn test_config() -> Config {
        Config {
            workspace: std::path::PathBuf::from("/tmp/repos"),
            known_orgs: vec!["test-org".to_string()],
            auto_discover_paths: vec![],
            poll_interval_ms: 60000,
            webhook_port: 8080,
            bind_address: "127.0.0.1".to_string(),
            db_path: std::path::PathBuf::from(":memory:"),
            max_issues_per_cycle: 5,
            max_concurrent: 2,
            max_concurrent_sessions: 12,
            processing_delay_ms: 1000,
            max_activity_entries: 100,
            ipc_timeout_secs: 30,
            debug_logging: false,
            agent: claudear_config::config::AgentConfig::default(),
            scm: claudear_config::config::ScmConfig::default(),
            issues: claudear_config::config::IssuesConfig::default(),
            notifiers: claudear_config::config::NotifiersConfig::default(),
            ask: claudear_config::config::AskConfig::default(),
            retry: claudear_config::config::RetryConfig::default(),
            regression: claudear_config::config::RegressionConfig::default(),
            deploy_qa: claudear_config::config::DeployQaConfig::default(),
            cascade: claudear_config::config::CascadeConfig::default(),
            users: std::collections::HashMap::new(),
            learning: claudear_config::config::LearningConfig::default(),
            prioritisation: claudear_config::config::PrioritisationConfig::default(),
            code_index: claudear_config::config::CodeIndexConfig::default(),
            retrieval_eval: Default::default(),
            evaluation: claudear_config::config::EvaluationConfig::default(),
            storage_dir: "/tmp/claudear-storage".into(),
            dashboard: claudear_config::config::DashboardConfig::default(),
            llm: claudear_config::config::LlmModelConfig::default(),
            chat: claudear_config::config::ChatConfig::default(),
            tls: claudear_config::config::TlsConfig::default(),
            embedding: claudear_config::config::EmbeddingModelConfig::default(),
            qa: claudear_config::config::QaConfig::default(),
            knowledgebase: claudear_config::config::KnowledgebasesConfig::default(),
            reports: claudear_config::config::ReportsConfig::default(),
            mcp_server: claudear_config::config::McpSearchServerConfig::default(),
        }
    }

    fn create_test_watcher(
        notifier: Arc<dyn Notifier>,
        tracker: Arc<dyn FixAttemptTracker>,
        sources: Vec<Arc<dyn IssueSource>>,
        dry_run: bool,
    ) -> Arc<Watcher> {
        build_test_watcher(notifier, tracker, sources, dry_run, None)
    }

    /// A live test watcher that resolves repositories through `inferrer`.
    fn create_test_watcher_with_inferrer(
        notifier: Arc<dyn Notifier>,
        tracker: Arc<dyn FixAttemptTracker>,
        sources: Vec<Arc<dyn IssueSource>>,
        inferrer: RepoInferrer,
    ) -> Arc<Watcher> {
        build_test_watcher(notifier, tracker, sources, false, Some(inferrer))
    }

    /// An inferrer whose index holds only `repo`.
    fn inferrer_indexing(repo: IndexedRepo) -> RepoInferrer {
        let mut index = RepoIndex::new();
        index.add_repo(repo);
        RepoInferrer::new(index)
    }

    fn build_test_watcher(
        notifier: Arc<dyn Notifier>,
        tracker: Arc<dyn FixAttemptTracker>,
        sources: Vec<Arc<dyn IssueSource>>,
        dry_run: bool,
        inferrer: Option<RepoInferrer>,
    ) -> Arc<Watcher> {
        Arc::new(Watcher::new(WatcherOptions {
            inferrer,
            dry_run,
            ..live_watcher_options(notifier, tracker, sources)
        }))
    }

    /// Options for a live test watcher, for tests to override field by field.
    fn live_watcher_options(
        notifier: Arc<dyn Notifier>,
        tracker: Arc<dyn FixAttemptTracker>,
        sources: Vec<Arc<dyn IssueSource>>,
    ) -> WatcherOptions {
        let agent: Arc<dyn AgentRunner> =
            Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            ));
        WatcherOptions {
            config: test_config(),
            sources,
            notifier,
            tracker,
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent,
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }
    }

    fn create_test_watcher_with_config(config: Config) -> Arc<Watcher> {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let agent: Arc<dyn claudear_integrations::runner::AgentRunner> =
            Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            ));
        Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent,
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }))
    }

    #[test]
    fn test_session_limiter_sized_from_config() {
        let mut config = test_config();
        config.max_concurrent_sessions = 3;
        let watcher = create_test_watcher_with_config(config);
        assert_eq!(watcher.session_limiter.available_permits(), 3);
    }

    #[test]
    fn test_session_limiter_clamps_zero_to_one() {
        let mut config = test_config();
        config.max_concurrent_sessions = 0;
        let watcher = create_test_watcher_with_config(config);
        // A zero ceiling would deadlock every issue, so it clamps to 1.
        assert_eq!(watcher.session_limiter.available_permits(), 1);
    }

    #[test]
    fn test_watcher_new() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![Arc::new(MockSource::new("test"))];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        assert!(!watcher.dry_run);
        assert!(!watcher.is_running.load(Ordering::SeqCst));
        assert_eq!(watcher.active_processing.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_watcher_new_dry_run() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, true);

        assert!(watcher.dry_run);
    }

    #[test]
    fn test_watcher_stop() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);
        watcher.is_running.store(true, Ordering::SeqCst);

        assert!(watcher.is_running.load(Ordering::SeqCst));
        watcher.stop();
        assert!(!watcher.is_running.load(Ordering::SeqCst));
    }

    #[test]
    fn test_watcher_get_stats() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let stats = watcher.get_stats().unwrap();
        assert_eq!(stats.total, 0);
        assert_eq!(stats.success, 0);
    }

    #[test]
    fn test_watcher_reset_attempt() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        // Record an attempt first
        tracker.record_attempt("test", "123", "TEST-123").unwrap();

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, false);

        assert!(tracker.has_attempted("test", "123").unwrap());
        watcher.reset_attempt("test", "123").unwrap();
        assert!(!tracker.has_attempted("test", "123").unwrap());
    }

    #[test]
    fn test_watcher_sort_by_priority() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let mut issues = vec![
            (
                test_issue(),
                MatchResult::matched("Low", MatchPriority::Low),
            ),
            (
                test_issue(),
                MatchResult::matched("Urgent", MatchPriority::Urgent),
            ),
            (
                test_issue(),
                MatchResult::matched("High", MatchPriority::High),
            ),
            (
                test_issue(),
                MatchResult::matched("Normal", MatchPriority::Normal),
            ),
        ];

        watcher.sort_by_priority(&mut issues);

        assert_eq!(issues[0].1.priority, MatchPriority::Urgent);
        assert_eq!(issues[1].1.priority, MatchPriority::High);
        assert_eq!(issues[2].1.priority, MatchPriority::Normal);
        assert_eq!(issues[3].1.priority, MatchPriority::Low);
    }

    #[test]
    fn test_watcher_sort_by_priority_with_issue_priority() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        // All same match priority, different issue priorities
        let mut issues = vec![
            (
                test_issue_with_priority("1", IssuePriority::Low),
                MatchResult::matched("Same", MatchPriority::Normal),
            ),
            (
                test_issue_with_priority("2", IssuePriority::Critical),
                MatchResult::matched("Same", MatchPriority::Normal),
            ),
            (
                test_issue_with_priority("3", IssuePriority::Medium),
                MatchResult::matched("Same", MatchPriority::Normal),
            ),
        ];

        watcher.sort_by_priority(&mut issues);

        // Should be sorted by issue priority (Critical first)
        assert_eq!(issues[0].0.priority, IssuePriority::Critical);
        assert_eq!(issues[1].0.priority, IssuePriority::Medium);
        assert_eq!(issues[2].0.priority, IssuePriority::Low);
    }

    #[test]
    fn test_watcher_sort_empty_list() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let mut issues: Vec<(Issue, MatchResult)> = vec![];
        watcher.sort_by_priority(&mut issues);
        assert!(issues.is_empty());
    }

    #[test]
    fn test_watcher_sort_single_item() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let mut issues = vec![(
            test_issue(),
            MatchResult::matched("Single", MatchPriority::High),
        )];
        watcher.sort_by_priority(&mut issues);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].1.priority, MatchPriority::High);
    }

    #[tokio::test]
    async fn test_watcher_seed_empty_sources() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let result = watcher.seed().await.unwrap();
        assert_eq!(result.total, 0);
        assert!(result.by_source.is_empty());
    }

    #[tokio::test]
    async fn test_watcher_seed_with_issues() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![
            Issue::new("1", "T-1", "Issue 1", "http://example.com/1", "mock"),
            Issue::new("2", "T-2", "Issue 2", "http://example.com/2", "mock"),
        ];
        let source = Arc::new(MockSource::with_issues("mock", issues)) as Arc<dyn IssueSource>;
        let sources = vec![source];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, false);

        let result = watcher.seed().await.unwrap();
        assert_eq!(result.total, 2);
        assert_eq!(*result.by_source.get("mock").unwrap(), 2);

        // Verify issues are marked as seen
        assert!(tracker.has_attempted("mock", "1").unwrap());
        assert!(tracker.has_attempted("mock", "2").unwrap());
    }

    #[tokio::test]
    async fn test_watcher_seed_skips_already_seeded() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Pre-seed one issue
        tracker.record_attempt("mock", "1", "T-1").unwrap();

        let issues = vec![
            Issue::new("1", "T-1", "Issue 1", "http://example.com/1", "mock"),
            Issue::new("2", "T-2", "Issue 2", "http://example.com/2", "mock"),
        ];
        let source = Arc::new(MockSource::with_issues("mock", issues)) as Arc<dyn IssueSource>;
        let sources = vec![source];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, false);

        let result = watcher.seed().await.unwrap();
        // Only 1 new issue should be seeded
        assert_eq!(result.total, 1);
        assert_eq!(*result.by_source.get("mock").unwrap(), 1);
    }

    #[tokio::test]
    async fn test_watcher_trigger_issue_unknown_source() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let result = watcher.trigger_issue("nonexistent", "123").await;
        assert!(result.is_err());
    }

    #[test]
    fn test_seed_result_default() {
        let result = SeedResult::default();
        assert_eq!(result.total, 0);
        assert!(result.by_source.is_empty());
    }

    #[test]
    fn test_is_terminal_attempt_status() {
        assert!(!Watcher::is_terminal_attempt_status(
            FixAttemptStatus::Pending
        ));
        assert!(!Watcher::is_terminal_attempt_status(
            FixAttemptStatus::Success
        ));
        assert!(!Watcher::is_terminal_attempt_status(
            FixAttemptStatus::Failed
        ));
        assert!(Watcher::is_terminal_attempt_status(
            FixAttemptStatus::Merged
        ));
        assert!(Watcher::is_terminal_attempt_status(
            FixAttemptStatus::Closed
        ));
        assert!(Watcher::is_terminal_attempt_status(
            FixAttemptStatus::CannotFix
        ));
    }

    #[test]
    fn test_seed_result_debug() {
        let result = SeedResult {
            total: 5,
            by_source: [("test".to_string(), 5)].into_iter().collect(),
        };
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("total: 5"));
        assert!(debug_str.contains("test"));
    }

    #[test]
    fn test_watcher_options_struct_fields() {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let options = WatcherOptions {
            config: test_config(),
            sources: sources.clone(),
            notifier: notifier.clone(),
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: true,
            llm_engine: None,
        };

        assert!(options.dry_run);
        assert!(options.sources.is_empty());
        assert!(options.inferrer.is_none());
    }

    #[test]
    fn test_priority_ordering() {
        assert!(priority_order(&MatchPriority::Urgent) < priority_order(&MatchPriority::High));
        assert!(priority_order(&MatchPriority::High) < priority_order(&MatchPriority::Normal));
        assert!(priority_order(&MatchPriority::Normal) < priority_order(&MatchPriority::Low));
    }

    #[test]
    fn test_issue_priority_ordering() {
        use claudear_core::types::IssuePriority;

        assert!(
            issue_priority_order(&IssuePriority::Critical)
                > issue_priority_order(&IssuePriority::High)
        );
        assert!(
            issue_priority_order(&IssuePriority::High)
                > issue_priority_order(&IssuePriority::Medium)
        );
        assert!(
            issue_priority_order(&IssuePriority::Medium)
                > issue_priority_order(&IssuePriority::Low)
        );
        assert!(
            issue_priority_order(&IssuePriority::Low) > issue_priority_order(&IssuePriority::None)
        );
    }

    #[test]
    fn test_priority_order_values() {
        assert_eq!(priority_order(&MatchPriority::Urgent), 0);
        assert_eq!(priority_order(&MatchPriority::High), 1);
        assert_eq!(priority_order(&MatchPriority::Normal), 2);
        assert_eq!(priority_order(&MatchPriority::Low), 3);
    }

    #[test]
    fn test_issue_priority_order_values() {
        use claudear_core::types::IssuePriority;

        assert_eq!(issue_priority_order(&IssuePriority::Critical), 4);
        assert_eq!(issue_priority_order(&IssuePriority::High), 3);
        assert_eq!(issue_priority_order(&IssuePriority::Medium), 2);
        assert_eq!(issue_priority_order(&IssuePriority::Low), 1);
        assert_eq!(issue_priority_order(&IssuePriority::None), 0);
    }

    #[test]
    fn test_match_priority_sorting() {
        // Verify that sorting by priority_order puts Urgent first
        let mut priorities = [
            MatchPriority::Low,
            MatchPriority::Urgent,
            MatchPriority::Normal,
            MatchPriority::High,
        ];

        priorities.sort_by_key(priority_order);

        assert_eq!(priorities[0], MatchPriority::Urgent);
        assert_eq!(priorities[1], MatchPriority::High);
        assert_eq!(priorities[2], MatchPriority::Normal);
        assert_eq!(priorities[3], MatchPriority::Low);
    }

    #[test]
    fn test_issue_priority_sorting() {
        use claudear_core::types::IssuePriority;

        let mut priorities = [
            IssuePriority::None,
            IssuePriority::Critical,
            IssuePriority::Low,
            IssuePriority::High,
            IssuePriority::Medium,
        ];

        priorities.sort_by_key(|p| std::cmp::Reverse(issue_priority_order(p)));

        assert_eq!(priorities[0], IssuePriority::Critical);
        assert_eq!(priorities[1], IssuePriority::High);
        assert_eq!(priorities[2], IssuePriority::Medium);
        assert_eq!(priorities[3], IssuePriority::Low);
        assert_eq!(priorities[4], IssuePriority::None);
    }

    #[test]
    fn test_watcher_options_struct() {
        use claudear_storage::SqliteTracker;

        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        // Verify tracker can be created
        assert!(tracker.get_stats().is_ok());
    }

    #[test]
    fn test_match_result_matched() {
        let result = MatchResult::matched("Reason", MatchPriority::High);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::High);
        assert_eq!(result.reason, "Reason");
    }

    #[test]
    fn test_match_result_not_matched() {
        let result = MatchResult::not_matched("Not matching reason");
        assert!(!result.matches);
        assert_eq!(result.priority, MatchPriority::Normal);
        assert_eq!(result.reason, "Not matching reason");
    }

    #[test]
    fn test_fix_attempt_stats_default() {
        let stats = FixAttemptStats::default();
        assert_eq!(stats.total, 0);
        assert_eq!(stats.success, 0);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.pending, 0);
    }

    #[test]
    fn test_match_priority_variants() {
        // Test all MatchPriority variants exist and can be compared
        let urgent = MatchPriority::Urgent;
        let high = MatchPriority::High;
        let normal = MatchPriority::Normal;
        let low = MatchPriority::Low;

        assert_ne!(urgent, high);
        assert_ne!(high, normal);
        assert_ne!(normal, low);
    }

    #[test]
    fn test_priority_order_all_priorities() {
        // Ensure all priorities have unique order values
        let orders: Vec<u8> = vec![
            priority_order(&MatchPriority::Urgent),
            priority_order(&MatchPriority::High),
            priority_order(&MatchPriority::Normal),
            priority_order(&MatchPriority::Low),
        ];

        // All unique
        let unique: HashSet<_> = orders.iter().collect();
        assert_eq!(unique.len(), 4);

        // Urgent is lowest (highest priority)
        assert_eq!(
            *orders.iter().min().unwrap(),
            priority_order(&MatchPriority::Urgent)
        );
    }

    #[test]
    fn test_issue_priority_order_all_priorities() {
        use claudear_core::types::IssuePriority;

        let orders: Vec<u8> = vec![
            issue_priority_order(&IssuePriority::Critical),
            issue_priority_order(&IssuePriority::High),
            issue_priority_order(&IssuePriority::Medium),
            issue_priority_order(&IssuePriority::Low),
            issue_priority_order(&IssuePriority::None),
        ];

        // All unique
        let unique: HashSet<_> = orders.iter().collect();
        assert_eq!(unique.len(), 5);

        // Critical is highest
        assert_eq!(
            *orders.iter().max().unwrap(),
            issue_priority_order(&IssuePriority::Critical)
        );
        // None is lowest
        assert_eq!(
            *orders.iter().min().unwrap(),
            issue_priority_order(&IssuePriority::None)
        );
    }

    #[test]
    fn test_match_result_default_priority() {
        let result = MatchResult::matched("Test", MatchPriority::Normal);
        assert_eq!(result.priority, MatchPriority::Normal);
    }

    #[test]
    fn test_fix_attempt_stats_with_values() {
        let stats = FixAttemptStats {
            total: 100,
            success: 75,
            failed: 20,
            pending: 5,
            merged: 50,
            closed: 10,
            cannot_fix: 5,
            by_source: std::collections::HashMap::new(),
        };

        assert_eq!(stats.total, 100);
        assert_eq!(stats.success, 75);
        assert_eq!(stats.failed, 20);
        assert_eq!(stats.pending, 5);
        assert_eq!(stats.merged, 50);
        assert_eq!(stats.closed, 10);
        assert_eq!(stats.cannot_fix, 5);
    }

    #[test]
    fn test_match_result_urgent_priority() {
        let result = MatchResult::matched("Urgent issue", MatchPriority::Urgent);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Urgent);
    }

    #[test]
    fn test_match_result_low_priority() {
        let result = MatchResult::matched("Low priority", MatchPriority::Low);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Low);
    }

    #[test]
    fn test_empty_priority_sorting() {
        let priorities: Vec<MatchPriority> = vec![];
        let sorted: Vec<_> = priorities.to_vec();

        assert!(sorted.is_empty());
    }

    #[test]
    fn test_single_priority_sorting() {
        let mut priorities = [MatchPriority::High];
        priorities.sort_by_key(priority_order);
        assert_eq!(priorities[0], MatchPriority::High);
    }

    #[test]
    fn test_duplicate_priorities_sorting() {
        let mut priorities = [
            MatchPriority::Normal,
            MatchPriority::Urgent,
            MatchPriority::Normal,
            MatchPriority::Urgent,
        ];
        priorities.sort_by_key(priority_order);

        // First two should be Urgent
        assert_eq!(priorities[0], MatchPriority::Urgent);
        assert_eq!(priorities[1], MatchPriority::Urgent);
        // Last two should be Normal
        assert_eq!(priorities[2], MatchPriority::Normal);
        assert_eq!(priorities[3], MatchPriority::Normal);
    }

    #[tokio::test]
    async fn test_watcher_poll_with_no_sources() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        // Poll should succeed even with no sources
        let result = watcher.poll().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_watcher_poll_records_cycle_metrics() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, false);
        watcher.poll().await.unwrap();

        let poll_cycle = tracker
            .get_metrics("poll_cycle_duration_secs", None, 10)
            .unwrap();
        assert_eq!(poll_cycle.len(), 1);
        assert!(poll_cycle[0].metric_value >= 0.0);

        let poll_sources = tracker.get_metrics("poll_sources", None, 10).unwrap();
        assert_eq!(poll_sources.len(), 1);
        assert_eq!(poll_sources[0].metric_value, 0.0);

        assert_eq!(
            tracker
                .get_metrics("ready_retries_found", None, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            tracker
                .get_metrics("ready_retries_executed_total", None, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            tracker
                .get_metrics("ready_retries_failed_total", None, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            tracker
                .get_metrics("pr_status_checks", None, 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_watcher_poll_dry_run() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![Issue::new(
            "1",
            "T-1",
            "Issue 1",
            "http://example.com/1",
            "mock",
        )];
        let source = Arc::new(MockSource::with_issues("mock", issues)) as Arc<dyn IssueSource>;
        let sources = vec![source];

        let watcher = create_test_watcher(notifier.clone(), tracker.clone(), sources, true);

        // Poll in dry run mode - should succeed
        let result = watcher.poll().await;
        assert!(result.is_ok());

        // In dry run mode, issues are NOT marked as attempted (just logged)
        assert!(!tracker.has_attempted("mock", "1").unwrap());
    }

    #[tokio::test]
    async fn test_watcher_poll_with_multiple_sources() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let source1 = Arc::new(MockSource::with_issues(
            "source1",
            vec![Issue::new(
                "1",
                "S1-1",
                "Issue 1",
                "http://example.com/1",
                "source1",
            )],
        )) as Arc<dyn IssueSource>;

        let source2 = Arc::new(MockSource::with_issues(
            "source2",
            vec![Issue::new(
                "2",
                "S2-1",
                "Issue 2",
                "http://example.com/2",
                "source2",
            )],
        )) as Arc<dyn IssueSource>;

        let sources = vec![source1, source2];
        let watcher = create_test_watcher(notifier, tracker.clone(), sources, true);

        let result = watcher.poll().await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_watcher_is_running_flag() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        // Initially not running
        assert!(!watcher.is_running.load(Ordering::SeqCst));

        // Set running
        watcher.is_running.store(true, Ordering::SeqCst);
        assert!(watcher.is_running.load(Ordering::SeqCst));

        // Stop should clear flag
        watcher.stop();
        assert!(!watcher.is_running.load(Ordering::SeqCst));
    }

    #[test]
    fn test_watcher_active_processing_counter() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        // Initially 0
        assert_eq!(watcher.active_processing.load(Ordering::SeqCst), 0);

        // Increment
        watcher.active_processing.fetch_add(1, Ordering::SeqCst);
        assert_eq!(watcher.active_processing.load(Ordering::SeqCst), 1);

        // Decrement
        watcher.active_processing.fetch_sub(1, Ordering::SeqCst);
        assert_eq!(watcher.active_processing.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_watcher_poll_source_with_empty_issues() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let source = Arc::new(MockSource::new("empty")) as Arc<dyn IssueSource>;
        let sources = vec![source.clone()];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        // poll_source returns Result<()>, not Vec
        let result = watcher.poll_source(&source).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_watcher_poll_source_records_zero_stage_metrics() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let source = Arc::new(MockSource::new("empty")) as Arc<dyn IssueSource>;
        let sources = vec![source.clone()];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, false);
        watcher.poll_source(&source).await.unwrap();

        let fetched = tracker.get_metrics("issues_fetched", None, 10).unwrap();
        let matched = tracker.get_metrics("issues_matched", None, 10).unwrap();
        let queued = tracker.get_metrics("issues_queued", None, 10).unwrap();

        assert_eq!(fetched.len(), 1);
        assert_eq!(matched.len(), 1);
        assert_eq!(queued.len(), 1);
        assert_eq!(fetched[0].metric_value, 0.0);
        assert_eq!(matched[0].metric_value, 0.0);
        assert_eq!(queued[0].metric_value, 0.0);
    }

    #[tokio::test]
    async fn test_watcher_poll_source_with_issues() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![
            Issue::new("1", "T-1", "Issue 1", "http://example.com/1", "test"),
            Issue::new("2", "T-2", "Issue 2", "http://example.com/2", "test"),
        ];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;
        let sources = vec![source.clone()];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, true); // dry run

        // poll_source returns Result<()>
        let result = watcher.poll_source(&source).await;
        assert!(result.is_ok());
        // In dry run mode, issues are NOT recorded (just logged)
        assert!(!tracker.has_attempted("test", "1").unwrap());
        assert!(!tracker.has_attempted("test", "2").unwrap());
    }

    #[tokio::test]
    async fn test_watcher_poll_source_deduplicates_duplicate_issue_ids() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![
            Issue::new("1", "T-1", "Issue 1", "http://example.com/1", "test"),
            Issue::new(
                "1",
                "T-1",
                "Issue 1 duplicate",
                "http://example.com/1",
                "test",
            ),
        ];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;
        let sources = vec![source.clone()];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, true); // dry run

        watcher.poll_source(&source).await.unwrap();

        let matched = tracker.get_metrics("issues_matched", None, 10).unwrap();
        let queued = tracker.get_metrics("issues_queued", None, 10).unwrap();

        assert_eq!(matched.len(), 1);
        assert_eq!(queued.len(), 1);
        assert_eq!(matched[0].metric_value, 1.0);
        assert_eq!(queued[0].metric_value, 1.0);
    }

    #[tokio::test]
    async fn test_watcher_poll_source_continues_when_urgent_notification_fails() {
        let notifier = Arc::new(MockNotifier::with_urgent_failure(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let source = Arc::new(MockSource::with_priority(
            "urgent",
            vec![Issue::new(
                "1",
                "URGENT-1",
                "Urgent issue",
                "http://example.com/urgent/1",
                "urgent",
            )],
            MatchPriority::Urgent,
        )) as Arc<dyn IssueSource>;
        let sources = vec![source.clone()];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, false);
        watcher.is_running.store(true, Ordering::SeqCst);

        let result = watcher.poll_source(&source).await;
        assert!(result.is_ok());
        watcher.drain_spawned_tasks().await;

        let attempt = tracker.get_attempt("urgent", "1").unwrap().unwrap();
        assert_eq!(
            attempt.status,
            claudear_core::types::FixAttemptStatus::Failed
        );
    }

    #[tokio::test]
    async fn test_watcher_poll_source_skips_trailing_processing_delay() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let source = Arc::new(MockSource::with_issues(
            "timing",
            vec![
                Issue::new("1", "TIME-1", "Issue 1", "http://example.com/1", "timing"),
                Issue::new("2", "TIME-2", "Issue 2", "http://example.com/2", "timing"),
            ],
        )) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.max_issues_per_cycle = 5;
        config.processing_delay_ms = 250;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source.clone()],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));
        watcher.is_running.store(true, Ordering::SeqCst);

        let started = std::time::Instant::now();
        watcher.poll_source(&source).await.unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(450),
            "poll_source took too long: {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_watcher_poll_source_not_blocked_by_other_source_activity() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let source = Arc::new(MockSource::with_issues(
            "target",
            vec![Issue::new(
                "1",
                "TARGET-1",
                "Target issue",
                "http://example.com/target/1",
                "target",
            )],
        )) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.max_concurrent = 1;
        config.processing_delay_ms = 0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source.clone()],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));
        watcher.is_running.store(true, Ordering::SeqCst);

        // Simulate unrelated in-flight work from another source.
        watcher.active_processing.fetch_add(1, Ordering::SeqCst);
        watcher
            .lock_processing()
            .insert("other:inflight".to_string());

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            watcher.poll_source(&source),
        )
        .await;
        assert!(result.is_ok(), "poll_source timed out unexpectedly");
        assert!(result.unwrap().is_ok());
        watcher.drain_spawned_tasks().await;

        let attempt = tracker.get_attempt("target", "1").unwrap().unwrap();
        assert_eq!(
            attempt.status,
            claudear_core::types::FixAttemptStatus::Failed
        );

        // Clean up simulated work so test state remains consistent.
        watcher.lock_processing().remove("other:inflight");
        watcher.active_processing.fetch_sub(1, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn test_session_ceiling_caps_concurrent_processing_and_frees_slots() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let issues = vec![
            Issue::new("1", "CEIL-1", "Ceiling issue 1", "http://example.com/1", "mock"),
            Issue::new("2", "CEIL-2", "Ceiling issue 2", "http://example.com/2", "mock"),
            Issue::new("3", "CEIL-3", "Ceiling issue 3", "http://example.com/3", "mock"),
        ];
        let source = Arc::new(MockSource::with_issues("mock", issues.clone()))
            as Arc<dyn IssueSource>;

        let mut config = test_config();
        // Global ceiling of 2; a generous per-source budget so the per-source
        // lane is never the binding limit (the ceiling is what we test).
        config.max_concurrent_sessions = 2;
        config.max_concurrent = 10;
        config.processing_delay_ms = 0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source.clone()],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));
        watcher.is_running.store(true, Ordering::SeqCst);

        // Saturate the global ceiling by holding both session permits, standing
        // in for two runs already in flight.
        let held_a = watcher
            .session_limiter
            .try_acquire()
            .expect("the first session permit should be free");
        let held_b = watcher
            .session_limiter
            .try_acquire()
            .expect("the second session permit should be free");
        assert_eq!(watcher.session_limiter.available_permits(), 0);

        // Start three more real processing runs. With the ceiling saturated,
        // none may begin work: concurrent processing cannot exceed the limit.
        let handles: Vec<_> = issues
            .into_iter()
            .map(|issue| {
                let w = Arc::clone(&watcher);
                let s = Arc::clone(&source);
                tokio::spawn(async move {
                    w.process_issue(
                        s,
                        issue,
                        MatchResult::matched("Test", MatchPriority::Normal),
                        None,
                        None,
                        None,
                        None,
                    )
                    .await
                })
            })
            .collect();

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            handles.iter().all(|h| !h.is_finished()),
            "a run started even though the session ceiling was saturated"
        );
        assert_eq!(watcher.session_limiter.available_permits(), 0);

        // Freeing the ceiling lets the parked runs proceed; each must release
        // its slot on completion, so all permits return.
        drop(held_a);
        drop(held_b);
        let drained = tokio::time::timeout(Duration::from_secs(10), async {
            for handle in handles {
                let _ = handle.await;
            }
        })
        .await;
        assert!(
            drained.is_ok(),
            "processing did not complete after the session slots were freed"
        );
        assert_eq!(
            watcher.session_limiter.available_permits(),
            2,
            "completing every run must free its session slot"
        );
    }

    /// Source whose `fetch_issues` blocks at a gate, standing in for a slow API,
    /// and counts how many times it was polled.
    struct BlockingFetchSource {
        name: &'static str,
        gate: Arc<Gate>,
        fetch_calls: AtomicUsize,
    }

    #[async_trait]
    impl IssueSource for BlockingFetchSource {
        fn name(&self) -> &str {
            self.name
        }
        fn display_name(&self) -> &str {
            self.name
        }
        async fn fetch_issues(&self) -> Result<Vec<Issue>> {
            self.fetch_calls.fetch_add(1, AtomicOrdering::SeqCst);
            self.gate.pass().await;
            Ok(vec![])
        }
        fn matches_criteria(&self, _issue: &Issue) -> MatchResult {
            MatchResult::matched("blocking match", MatchPriority::Normal)
        }
        async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
            Ok(format!("Context for {}", issue.short_id))
        }
        async fn get_issue(&self, id: &str) -> Result<Issue> {
            Err(claudear_core::error::Error::source(
                self.name,
                format!("Issue {id} not found"),
            ))
        }
    }

    #[tokio::test]
    async fn test_source_workers_poll_independently() {
        // A source stuck in a slow `fetch_issues` must not delay another
        // source's polling: each runs in its own worker.
        let gate = Arc::new(Gate::default());
        let slow = Arc::new(BlockingFetchSource {
            name: "slow",
            gate: Arc::clone(&gate),
            fetch_calls: AtomicUsize::new(0),
        });
        let fast = Arc::new(MockSource::new("fast"));
        let sources: Vec<Arc<dyn IssueSource>> = vec![
            Arc::clone(&slow) as Arc<dyn IssueSource>,
            Arc::clone(&fast) as Arc<dyn IssueSource>,
        ];

        let mut config = test_config();
        config.poll_interval_ms = 1000; // one poll per second per source

        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources,
            notifier: Arc::new(MockNotifier::new(true)),
            tracker: Arc::clone(&tracker),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker,
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: true,
            llm_engine: None,
        }));
        watcher.is_running.store(true, Ordering::SeqCst);

        let poller = {
            let w = Arc::clone(&watcher);
            tokio::spawn(async move { w.run_source_poll_loop(1000).await })
        };

        // Wait (robustly, not on a fixed sleep that flakes under CI load) until
        // the fast source has polled at least twice while the slow source stays
        // stuck in its first fetch.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while fast.fetch_call_count() < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let fast_polls = fast.fetch_call_count();
        let slow_polls = slow.fetch_calls.load(AtomicOrdering::SeqCst);
        assert!(
            fast_polls >= 2,
            "the fast source should have polled repeatedly while the slow source was blocked, got {fast_polls}"
        );
        // The slow source is parked in its first fetch, so it cannot poll again:
        // its worker makes no progress while the fast one keeps going.
        assert_eq!(
            slow_polls, 1,
            "the slow source should still be parked in its first fetch, got {slow_polls}"
        );
        assert!(
            !gate.was_cancelled(),
            "the slow source's fetch should still be waiting at the gate"
        );

        // Release the slow source and stop; the loop must wind down cleanly.
        gate.open();
        watcher.stop();
        let _ = tokio::time::timeout(Duration::from_secs(10), poller)
            .await
            .expect("the poll loop did not stop after the watcher was stopped");
    }

    #[tokio::test]
    async fn test_dispatch_lane_wakes_for_slot_freed_during_rate_limit_check() {
        let issue = Issue::new("1", "T-1", "Test Issue", "http://example.com/1", "mock");
        let source =
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])) as Arc<dyn IssueSource>;
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(
            Arc::new(MockNotifier::new(true)),
            tracker.clone(),
            vec![source.clone()],
            true,
        );
        watcher.set_running(true);
        let items = vec![(
            issue,
            MatchResult::matched("Test", MatchPriority::Normal),
            None,
        )];
        let inflight = watcher
            .claim_processing("mock:inflight".to_string(), false)
            .expect("the in-flight key should be free");
        let pauses = watcher.rate_limit_pause_until.write().await;
        // The lane queues on the rate-limit lock for its pause check. Handing
        // it the lock and queuing again parks the lane on the same lock in its
        // slot wait, after it has seen the lane full, where the slot is freed.
        let free_slot_during_wait = async {
            tokio::task::yield_now().await;
            drop(pauses);
            let pauses = watcher.rate_limit_pause_until.write().await;
            drop(inflight);
            drop(pauses);
        };

        let dispatched = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                watcher.dispatch_lane(&source, items, 1, false),
                free_slot_during_wait,
            )
        })
        .await;

        assert!(
            dispatched.is_ok(),
            "a slot freed while the lane checks for a rate-limit pause must wake the lane"
        );
        watcher.drain_spawned_tasks().await;
        assert!(
            tracker.get_attempt("mock", "1").unwrap().is_some(),
            "the lane should process its issue in the freed slot"
        );
    }

    #[tokio::test]
    async fn test_watcher_poll_source_zero_max_concurrent_does_not_deadlock() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let source = Arc::new(MockSource::with_issues(
            "zero-concurrency",
            vec![Issue::new(
                "1",
                "ZERO-1",
                "Zero concurrency issue",
                "http://example.com/zero/1",
                "zero-concurrency",
            )],
        )) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.max_concurrent = 0;
        config.processing_delay_ms = 0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source.clone()],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));
        watcher.is_running.store(true, Ordering::SeqCst);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            watcher.poll_source(&source),
        )
        .await;
        assert!(
            result.is_ok(),
            "poll_source timed out with max_concurrent=0"
        );
        assert!(result.unwrap().is_ok());
        watcher.drain_spawned_tasks().await;

        let attempt = tracker
            .get_attempt("zero-concurrency", "1")
            .unwrap()
            .unwrap();
        assert_eq!(
            attempt.status,
            claudear_core::types::FixAttemptStatus::Failed
        );
    }

    #[tokio::test]
    async fn test_process_ready_retries_zero_max_concurrent_does_not_deadlock() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        tracker
            .record_attempt("mock", "missing-retry", "MOCK-RETRY")
            .unwrap();
        tracker
            .mark_failed("mock", "missing-retry", "initial failure")
            .unwrap();

        let source = Arc::new(MockSource::new("mock")) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.max_concurrent = 0;
        config.processing_delay_ms = 0;
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));
        watcher.is_running.store(true, Ordering::SeqCst);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            watcher.process_ready_retries(),
        )
        .await;
        assert!(
            result.is_ok(),
            "process_ready_retries timed out with max_concurrent=0"
        );
        assert!(result.unwrap().is_ok());

        let attempt = tracker
            .get_attempt("mock", "missing-retry")
            .unwrap()
            .unwrap();
        assert_eq!(
            attempt.status,
            claudear_core::types::FixAttemptStatus::Failed
        );
        assert_eq!(attempt.retry_count, 1);
    }

    #[tokio::test]
    async fn test_watcher_start_dry_run_skips_auto_close_checks() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        tracker.record_attempt("mock", "1", "MOCK-1").unwrap();
        tracker
            .mark_success("mock", "1", "https://github.com/org/repo/pull/1")
            .unwrap();

        let mock_source = Arc::new(MockSource::with_issues(
            "mock",
            vec![Issue::new(
                "1",
                "MOCK-1",
                "Mock issue",
                "http://example.com/mock/1",
                "mock",
            )],
        ));
        let source = Arc::clone(&mock_source) as Arc<dyn IssueSource>;

        let watcher = Arc::new(create_test_watcher(notifier, tracker, vec![source], true));

        let runner = {
            let watcher = Arc::clone(&watcher);
            tokio::spawn(async move { watcher.start(Some(50)).await })
        };

        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        watcher.stop();

        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), runner).await;
        assert!(joined.is_ok(), "watcher start loop did not stop in time");
        assert!(joined.unwrap().expect("task join failed").is_ok());
        assert_eq!(
            mock_source.issue_status_call_count(),
            0,
            "dry_run should not call get_issue_status via auto-close checks"
        );
    }

    #[tokio::test]
    async fn test_watcher_start_zero_interval_clamped_without_panic() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let source = Arc::new(MockSource::new("mock")) as Arc<dyn IssueSource>;
        let watcher = Arc::new(create_test_watcher(notifier, tracker, vec![source], true));

        let runner = {
            let watcher = Arc::clone(&watcher);
            tokio::spawn(async move { watcher.start(Some(0)).await })
        };

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        watcher.stop();

        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), runner).await;
        assert!(
            joined.is_ok(),
            "watcher start loop timed out with zero interval"
        );
        assert!(
            joined.unwrap().expect("task join failed").is_ok(),
            "watcher returned an error with zero interval"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_start_after_stop_stays_stopped_without_polling() {
        let source = Arc::new(MockSource::new("mock"));
        let watcher = create_test_watcher(
            Arc::new(MockNotifier::new(true)),
            Arc::new(SqliteTracker::in_memory().unwrap()),
            vec![Arc::clone(&source) as Arc<dyn IssueSource>],
            false,
        );

        watcher.stop();
        let started = tokio::time::timeout(Duration::from_secs(5), watcher.start(Some(50))).await;

        assert!(
            matches!(started, Ok(Ok(()))),
            "start after a stop must return instead of polling: {started:?}"
        );
        assert!(
            !watcher.is_running(),
            "a stop that came first must keep the watcher stopped"
        );
        assert_eq!(
            source.fetch_call_count(),
            0,
            "a stopped watcher must not poll its sources"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_housekeeping_start_after_stop_stays_stopped_without_a_cycle() {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(
            Arc::new(MockNotifier::new(true)),
            tracker.clone(),
            vec![],
            false,
        );
        let worker = crate::housekeeping::HousekeepingWorker::new(Arc::clone(&watcher), 50);

        watcher.stop();
        let started = tokio::time::timeout(Duration::from_secs(5), worker.start()).await;

        assert!(
            matches!(started, Ok(Ok(()))),
            "housekeeping start after a stop must return instead of looping: {started:?}"
        );
        assert!(
            !watcher.is_running(),
            "a stop that came first must keep the watcher stopped"
        );
        assert!(
            tracker
                .get_metrics("housekeeping_cycle_duration_secs", None, 10)
                .unwrap()
                .is_empty(),
            "a stopped watcher must not run a housekeeping cycle"
        );
    }

    /// Holds each caller of [`Gate::pass`] until [`Gate::open`], signalling its
    /// arrival and recording whether a held caller was dropped instead.
    #[derive(Default)]
    struct Gate {
        arrived: Notify,
        opened: Notify,
        cancelled: AtomicBool,
    }

    impl Gate {
        async fn pass(&self) {
            let held = HeldAtGate(self);
            self.arrived.notify_one();
            self.opened.notified().await;
            std::mem::forget(held);
        }

        async fn arrival(&self) {
            self.arrived.notified().await;
        }

        /// Wait for a caller to arrive, failing instead of hanging when `task`
        /// ends without reaching the gate.
        async fn arrival_during<T: std::fmt::Debug>(&self, task: &mut tokio::task::JoinHandle<T>) {
            tokio::select! {
                () = self.arrival() => {}
                ended = task => panic!("the task ended before reaching the gate: {ended:?}"),
            }
        }

        fn open(&self) {
            self.opened.notify_one();
        }

        fn was_cancelled(&self) -> bool {
            self.cancelled.load(AtomicOrdering::SeqCst)
        }
    }

    /// Marks its gate cancelled when the caller held there is dropped.
    struct HeldAtGate<'a>(&'a Gate);

    impl Drop for HeldAtGate<'_> {
        fn drop(&mut self) {
            self.0.cancelled.store(true, AtomicOrdering::SeqCst);
        }
    }

    const GATED_SOURCE: &str = "gated";

    /// Source named [`GATED_SOURCE`] unless given another name, that lists no
    /// issues and holds every `get_issue` at its gate before returning its
    /// issue, or reporting the issue missing when it has none.
    struct GatedSource {
        name: &'static str,
        gate: Gate,
        issue: Option<Issue>,
    }

    impl Default for GatedSource {
        fn default() -> Self {
            Self {
                name: GATED_SOURCE,
                gate: Gate::default(),
                issue: None,
            }
        }
    }

    #[async_trait]
    impl IssueSource for GatedSource {
        fn name(&self) -> &str {
            self.name
        }
        fn display_name(&self) -> &str {
            self.name
        }
        async fn fetch_issues(&self) -> Result<Vec<Issue>> {
            Ok(vec![])
        }
        fn matches_criteria(&self, _issue: &Issue) -> MatchResult {
            MatchResult::matched("Gated match", MatchPriority::Normal)
        }
        async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
            Ok(format!("Context for {}", issue.short_id))
        }
        async fn get_issue(&self, id: &str) -> Result<Issue> {
            self.gate.pass().await;
            self.issue
                .clone()
                .filter(|issue| issue.id == id)
                .ok_or_else(|| {
                    claudear_core::error::Error::source(self.name, format!("Issue {id} not found"))
                })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_start_after_stop_waits_for_the_housekeeping_retry_in_flight() {
        let source = Arc::new(GatedSource::default());
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;
        let watcher = watcher_with_agent(
            config,
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            Arc::new(ScriptedQaAgent {
                calls: Arc::new(AtomicUsize::new(0)),
                answer: QaAnswer::Crash,
            }),
        );
        let start = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.start(Some(50)).await }
        });

        // The initial poll runs ready retries inline, so seed the retry only
        // once that poll has finished; the housekeeping loop must run it.
        while tracker
            .get_metrics("poll_cycle_duration_secs", None, 1)
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tracker
            .record_attempt(GATED_SOURCE, "retry-1", "GATED-1")
            .unwrap();
        tracker
            .mark_failed(GATED_SOURCE, "retry-1", "initial failure")
            .unwrap();

        source.gate.arrival().await;
        watcher.stop();
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert!(
            !start.is_finished(),
            "start must not return while a housekeeping retry is still running"
        );

        source.gate.open();
        let finished = tokio::time::timeout(Duration::from_secs(5), start).await;

        assert!(
            matches!(finished, Ok(Ok(Ok(())))),
            "start should return once the retry finishes: {finished:?}"
        );
        assert!(
            !source.gate.was_cancelled(),
            "stopping must not cancel the housekeeping retry mid-run"
        );
    }

    const SCM_REPO: &str = "org/repo";
    const PR_NUMBER: i64 = 7;
    const PR_URL: &str = "https://github.com/org/repo/pull/7";
    const REVIEW_TRIGGER: &str = "@claudear";
    const FIX_REQUEST_ID: i64 = 70;
    const FIX_REQUEST_TIME: &str = "2026-01-01T00:00:00Z";

    /// A PR conversation comment that asks for a fix with [`REVIEW_TRIGGER`].
    fn fix_request() -> ReviewComment {
        ReviewComment {
            id: FIX_REQUEST_ID,
            path: String::new(),
            position: None,
            original_position: None,
            body: format!("{REVIEW_TRIGGER} please handle an empty list"),
            user: ReviewUser {
                id: 1,
                login: "reviewer".to_string(),
                user_type: Some("User".to_string()),
            },
            created_at: FIX_REQUEST_TIME.to_string(),
            updated_at: FIX_REQUEST_TIME.to_string(),
            html_url: String::new(),
            pull_request_review_id: None,
            line: None,
            start_line: None,
            side: None,
        }
    }

    /// SCM provider that reports every PR as `status` with a [`fix_request`]
    /// on it, and [`RELEASE_TAG`] as every repo's latest release, holding each
    /// status, comment and release lookup at its gate when `gated`.
    struct MockScm {
        status: PrStatus,
        gated: bool,
        gate: Gate,
        status_checks: AtomicUsize,
        release_checks: AtomicUsize,
    }

    impl MockScm {
        fn reporting(status: PrStatus) -> Self {
            Self {
                status,
                gated: false,
                gate: Gate::default(),
                status_checks: AtomicUsize::new(0),
                release_checks: AtomicUsize::new(0),
            }
        }

        fn gated(status: PrStatus) -> Self {
            Self {
                gated: true,
                ..Self::reporting(status)
            }
        }

        fn status_check_count(&self) -> usize {
            self.status_checks.load(AtomicOrdering::SeqCst)
        }

        fn release_check_count(&self) -> usize {
            self.release_checks.load(AtomicOrdering::SeqCst)
        }

        async fn hold(&self) {
            if self.gated {
                self.gate.pass().await;
            }
        }
    }

    #[async_trait]
    impl ScmProvider for MockScm {
        fn name(&self) -> &str {
            "mock-scm"
        }
        fn is_enabled(&self) -> bool {
            true
        }
        fn review_trigger(&self) -> &str {
            REVIEW_TRIGGER
        }
        async fn get_pr_status(&self, _project: &str, _number: i64) -> Result<PrStatus> {
            self.status_checks.fetch_add(1, AtomicOrdering::SeqCst);
            self.hold().await;
            Ok(self.status)
        }
        async fn get_pr_info(&self, _project: &str, _number: i64) -> Result<PrInfo> {
            Ok(PrInfo {
                head_branch: None,
                base_branch: None,
                title: None,
                author: None,
            })
        }
        async fn get_pr_diff(&self, _project: &str, _number: i64) -> Result<String> {
            Ok(String::new())
        }
        async fn get_reviews(&self, _project: &str, _number: i64) -> Result<Vec<CodeReview>> {
            Ok(Vec::new())
        }
        async fn get_review_comments(
            &self,
            _project: &str,
            _number: i64,
        ) -> Result<Vec<ReviewComment>> {
            Ok(Vec::new())
        }
        async fn get_pr_conversation_comments(
            &self,
            _project: &str,
            _number: i64,
        ) -> Result<Vec<ReviewComment>> {
            self.hold().await;
            Ok(vec![fix_request()])
        }
        async fn list_repos(&self, _org_or_group: &str) -> Result<Vec<RemoteRepo>> {
            Ok(Vec::new())
        }
        async fn get_latest_release(&self, _project: &str) -> Result<Option<ScmRelease>> {
            self.release_checks.fetch_add(1, AtomicOrdering::SeqCst);
            self.hold().await;
            Ok(Some(ScmRelease {
                tag: RELEASE_TAG.to_string(),
                name: None,
                url: String::new(),
                published_at: None,
            }))
        }
    }

    fn crashing_agent() -> Arc<dyn AgentRunner> {
        Arc::new(ScriptedQaAgent {
            calls: Arc::new(AtomicUsize::new(0)),
            answer: QaAnswer::Crash,
        })
    }

    /// Options for a watcher over `source` that follows PRs and their reviews
    /// through `scm`.
    fn scm_watcher_options(
        config: Config,
        source: Arc<dyn IssueSource>,
        tracker: Arc<SqliteTracker>,
        agent: Arc<dyn AgentRunner>,
        scm: Arc<MockScm>,
    ) -> WatcherOptions {
        let provider: Arc<dyn ScmProvider> = scm;
        WatcherOptions {
            config,
            sources: vec![source],
            notifier: Arc::new(MockNotifier::new(true)),
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: Some(Arc::new(ReviewWatcher::with_tracker(
                Arc::clone(&provider),
                tracker,
            ))),
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: None,
            github_client: None,
            scm_provider: Some(provider),
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent,
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }
    }

    fn watcher_with_scm(
        config: Config,
        source: Arc<dyn IssueSource>,
        tracker: Arc<SqliteTracker>,
        agent: Arc<dyn AgentRunner>,
        scm: Arc<MockScm>,
    ) -> Arc<Watcher> {
        Arc::new(Watcher::new(scm_watcher_options(
            config, source, tracker, agent, scm,
        )))
    }

    /// A watcher over `source` that follows open PRs' reviews and finds
    /// [`SCM_REPO`], the repository of [`PR_URL`], checked out at `checkout`,
    /// so review reruns of the PR can start.
    fn watcher_reviewing_pr(
        source: Arc<dyn IssueSource>,
        tracker: Arc<SqliteTracker>,
        checkout: &std::path::Path,
    ) -> Arc<Watcher> {
        Arc::new(Watcher::new(WatcherOptions {
            inferrer: Some(inferrer_indexing(IndexedRepo::new(SCM_REPO, checkout))),
            ..scm_watcher_options(
                test_config(),
                source,
                tracker,
                crashing_agent(),
                Arc::new(MockScm::reporting(PrStatus::Open)),
            )
        }))
    }

    /// Record `issue`'s successful attempt as the author of [`PR_URL`].
    fn record_pr(tracker: &SqliteTracker, issue: &Issue) {
        tracker
            .record_attempt(&issue.source, &issue.id, &issue.short_id)
            .unwrap();
        tracker
            .mark_success(&issue.source, &issue.id, PR_URL)
            .unwrap();
    }

    /// [Record](record_pr) `issue`'s PR, and have `watcher` follow its reviews.
    fn open_pr(watcher: &Watcher, tracker: &SqliteTracker, issue: &Issue) {
        record_pr(tracker, issue);
        watcher
            .review_watcher
            .as_ref()
            .expect("the watcher should follow PR reviews")
            .watch_pr(PrReviewState::new(
                PR_URL,
                SCM_REPO,
                PR_NUMBER,
                &issue.id,
                &issue.source,
            ));
    }

    fn attempt_for(tracker: &SqliteTracker, issue: &Issue) -> FixAttempt {
        tracker
            .get_attempt(&issue.source, &issue.id)
            .unwrap()
            .expect("the issue's attempt should be recorded")
    }

    fn fixed_issue(source: &str) -> Issue {
        Issue::new(
            "review-1",
            "REVIEW-1",
            "Crash on an empty list",
            "https://example.com/issues/review-1",
            source,
        )
    }

    #[tokio::test]
    async fn test_check_reviews_on_a_stopped_watcher_leaves_review_feedback_untouched() {
        let issue = fixed_issue("mock");
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = watcher_with_scm(
            test_config(),
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])),
            Arc::clone(&tracker),
            crashing_agent(),
            Arc::new(MockScm::reporting(PrStatus::Open)),
        );
        open_pr(&watcher, &tracker, &issue);
        watcher.stop();

        watcher.check_reviews().await.unwrap();

        assert_eq!(
            attempt_for(&tracker, &issue).status,
            FixAttemptStatus::Success,
            "a stopped watcher must not start a run for review feedback"
        );
        let state = watcher
            .review_watcher
            .as_ref()
            .and_then(|review_watcher| review_watcher.get_state(PR_URL))
            .expect("the PR should still be followed");
        assert_eq!(
            state.last_issue_comment_id, None,
            "a stopped watcher must not move the review cursor"
        );
        assert!(
            tracker
                .get_unhandled_pr_review_comments(PR_URL)
                .unwrap()
                .is_empty(),
            "a stopped watcher must not fetch review feedback"
        );
    }

    #[tokio::test]
    async fn test_check_reviews_stopped_while_fetching_starts_no_run() {
        let issue = fixed_issue("mock");
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let scm = Arc::new(MockScm::gated(PrStatus::Open));
        let watcher = watcher_with_scm(
            test_config(),
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])),
            Arc::clone(&tracker),
            crashing_agent(),
            Arc::clone(&scm),
        );
        open_pr(&watcher, &tracker, &issue);
        watcher.set_running(true);

        let mut reviews = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.check_reviews().await }
        });
        scm.gate.arrival_during(&mut reviews).await;
        watcher.stop();
        scm.gate.open();
        let checked = reviews.await;

        assert!(
            matches!(checked, Ok(Ok(()))),
            "the review check should finish: {checked:?}"
        );
        assert_eq!(
            attempt_for(&tracker, &issue).status,
            FixAttemptStatus::Success,
            "review feedback fetched after the stop must not start a run"
        );
        assert_eq!(
            tracker
                .get_unhandled_pr_review_comments(PR_URL)
                .unwrap()
                .len(),
            1,
            "the fix request must stay unhandled for the next start"
        );
    }

    #[tokio::test]
    async fn test_review_runs_refused_by_a_stop_do_not_count_toward_giving_up() {
        let issue = fixed_issue(GATED_SOURCE);
        let source = Arc::new(GatedSource {
            issue: Some(issue.clone()),
            ..GatedSource::default()
        });
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let checkout = tempfile::tempdir().unwrap();
        let watcher = watcher_reviewing_pr(
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            checkout.path(),
        );
        open_pr(&watcher, &tracker, &issue);
        watcher
            .lock_processing()
            .insert(format!("{GATED_SOURCE}:{}", issue.id));

        for _ in 0..MAX_REVIEW_COMMENT_ATTEMPTS {
            watcher.set_running(true);
            let mut reviews = tokio::spawn({
                let watcher = Arc::clone(&watcher);
                async move { watcher.check_reviews().await }
            });
            source.gate.arrival_during(&mut reviews).await;
            watcher.stop();
            source.gate.open();
            let checked = reviews.await;
            assert!(
                matches!(checked, Ok(Ok(()))),
                "the review check should finish: {checked:?}"
            );
        }

        assert_eq!(
            tracker
                .get_unhandled_pr_review_comments(PR_URL)
                .unwrap()
                .len(),
            1,
            "review runs refused by a stop must not give up on the fix request"
        );
        assert_eq!(
            review_cycles_used(&tracker, PR_URL),
            0,
            "review runs refused by a stop must not use up the PR's review cycles"
        );
    }

    #[tokio::test]
    async fn test_review_run_stopped_before_it_starts_uses_no_review_cycle() {
        let issue = fixed_issue(GATED_SOURCE);
        let source = Arc::new(GatedSource {
            issue: Some(issue.clone()),
            ..GatedSource::default()
        });
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let checkout = tempfile::tempdir().unwrap();
        let watcher = watcher_reviewing_pr(
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            checkout.path(),
        );
        record_pr(&tracker, &issue);
        watcher.set_running(true);
        let attempt = attempt_for(&tracker, &issue);

        let mut review = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move {
                watcher
                    .process_review_action(&attempt, "Please handle an empty list")
                    .await
            }
        });
        source.gate.arrival_during(&mut review).await;
        source.gate.open();
        source.gate.arrival_during(&mut review).await;
        watcher.stop();
        source.gate.open();
        let reviewed = review.await;

        assert!(
            matches!(reviewed, Ok(Err(_))),
            "the stop should refuse the review run: {reviewed:?}"
        );
        assert_eq!(
            attempt_for(&tracker, &issue).status,
            FixAttemptStatus::Success,
            "a review run refused by a stop must not start"
        );
        assert_eq!(
            review_cycles_used(&tracker, PR_URL),
            0,
            "a review run refused before it starts must not use up a review cycle"
        );
    }

    #[tokio::test]
    async fn test_check_pr_merges_on_a_stopped_watcher_checks_no_pr() {
        let issue = fixed_issue("mock");
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let scm = Arc::new(MockScm::reporting(PrStatus::Merged));
        let watcher = watcher_with_scm(
            test_config(),
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])),
            Arc::clone(&tracker),
            crashing_agent(),
            Arc::clone(&scm),
        );
        open_pr(&watcher, &tracker, &issue);
        watcher.stop();

        watcher.check_pr_merges_and_cascade().await.unwrap();

        assert_eq!(
            scm.status_check_count(),
            0,
            "a stopped watcher must not check its PRs"
        );
        assert_eq!(
            attempt_for(&tracker, &issue).status,
            FixAttemptStatus::Success,
            "a stopped watcher must leave a merged PR for the next start"
        );
        let checks = tracker.get_metrics("pr_status_checks", None, 10).unwrap();
        assert_eq!(
            checks.len(),
            1,
            "a cycle cut short by a stop must still record its metrics"
        );
        assert_eq!(checks[0].metric_value, 0.0);
    }

    #[tokio::test]
    async fn test_check_pr_merges_stopped_while_checking_leaves_the_pr_unmerged() {
        let issue = fixed_issue("mock");
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let scm = Arc::new(MockScm::gated(PrStatus::Merged));
        let watcher = watcher_with_scm(
            test_config(),
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])),
            Arc::clone(&tracker),
            crashing_agent(),
            Arc::clone(&scm),
        );
        open_pr(&watcher, &tracker, &issue);
        watcher.set_running(true);

        let mut merges = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.check_pr_merges_and_cascade().await }
        });
        scm.gate.arrival_during(&mut merges).await;
        watcher.stop();
        scm.gate.open();
        let checked = merges.await;

        assert!(
            matches!(checked, Ok(Ok(()))),
            "the merge check should finish: {checked:?}"
        );
        assert_eq!(
            attempt_for(&tracker, &issue).status,
            FixAttemptStatus::Success,
            "a PR found merged after the stop must be left for the next start"
        );
    }

    #[tokio::test]
    async fn test_check_pr_merges_stopped_while_checking_leaves_the_pr_open() {
        let issue = fixed_issue("mock");
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let scm = Arc::new(MockScm::gated(PrStatus::Closed));
        let watcher = watcher_with_scm(
            test_config(),
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])),
            Arc::clone(&tracker),
            crashing_agent(),
            Arc::clone(&scm),
        );
        open_pr(&watcher, &tracker, &issue);
        watcher.set_running(true);

        let mut merges = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.check_pr_merges_and_cascade().await }
        });
        scm.gate.arrival_during(&mut merges).await;
        watcher.stop();
        scm.gate.open();
        let checked = merges.await;

        assert!(
            matches!(checked, Ok(Ok(()))),
            "the merge check should finish: {checked:?}"
        );
        assert_eq!(
            attempt_for(&tracker, &issue).status,
            FixAttemptStatus::Success,
            "a PR found closed after the stop must be left for the next start"
        );
    }

    /// Wait until `issue`'s attempt is closed, failing instead of hanging when
    /// `task` ends first.
    async fn closed_during<T: std::fmt::Debug>(
        tracker: &SqliteTracker,
        issue: &Issue,
        task: &mut tokio::task::JoinHandle<T>,
    ) {
        let closed = async {
            while attempt_for(tracker, issue).status != FixAttemptStatus::Closed {
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            () = closed => {}
            ended = task => panic!("the task ended before closing the PR: {ended:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_stop_and_drain_waits_for_the_follow_up_to_a_closed_pr() {
        let issue = fixed_issue("mock");
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = watcher_with_scm(
            test_config(),
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])),
            Arc::clone(&tracker),
            crashing_agent(),
            Arc::new(MockScm::reporting(PrStatus::Closed)),
        );
        open_pr(&watcher, &tracker, &issue);
        watcher.set_running(true);
        let feedback = watcher.feedback_analyzer.lock().await;
        let mut merges = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.check_pr_merges_and_cascade().await }
        });
        closed_during(&tracker, &issue, &mut merges).await;

        let drain = watcher.stop_and_drain();
        tokio::pin!(drain);
        let while_following_up =
            tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 3, drain.as_mut()).await;

        assert!(
            while_following_up.is_err(),
            "the drain must wait while the closed PR's follow-up is in flight"
        );
        assert_eq!(
            watcher.in_flight(),
            1,
            "the closed PR's follow-up must count as a run in flight"
        );

        drop(feedback);

        assert!(
            drain.await,
            "the drain must end once the closed PR's follow-up finishes"
        );
        let checked = merges.await;
        assert!(
            matches!(checked, Ok(Ok(()))),
            "the merge check should finish: {checked:?}"
        );
    }

    fn resolved_issue(source: &str) -> Issue {
        let mut issue = fixed_issue(source);
        issue.status = claudear_core::types::IssueStatus::Resolved;
        issue
    }

    #[tokio::test]
    async fn test_auto_close_on_a_stopped_watcher_checks_no_issue() {
        let issue = resolved_issue("mock");
        let source = Arc::new(MockSource::with_issues("mock", vec![issue.clone()]));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = watcher_with_agent(
            test_config(),
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            crashing_agent(),
        );
        record_pr(&tracker, &issue);
        watcher.stop();

        let closed = watcher.check_and_auto_close_prs().await.unwrap();

        assert!(closed.is_empty(), "a stopped watcher must not close PRs");
        assert_eq!(
            source.issue_status_call_count(),
            0,
            "a stopped watcher must not check issue statuses"
        );
        assert_eq!(
            attempt_for(&tracker, &issue).status,
            FixAttemptStatus::Success
        );
    }

    #[tokio::test]
    async fn test_auto_close_stopped_while_checking_leaves_the_pr_open() {
        let issue = resolved_issue(GATED_SOURCE);
        let source = Arc::new(GatedSource {
            issue: Some(issue.clone()),
            ..GatedSource::default()
        });
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = watcher_with_agent(
            test_config(),
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            crashing_agent(),
        );
        record_pr(&tracker, &issue);
        watcher.set_running(true);

        let mut closing = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.check_and_auto_close_prs().await }
        });
        source.gate.arrival_during(&mut closing).await;
        watcher.stop();
        source.gate.open();
        let closed = closing.await;

        assert!(
            matches!(&closed, Ok(Ok(urls)) if urls.is_empty()),
            "no PR must be auto-closed after the stop: {closed:?}"
        );
        assert_eq!(
            attempt_for(&tracker, &issue).status,
            FixAttemptStatus::Success,
            "a PR whose issue is found terminal after the stop must be left for the next start"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_stop_and_drain_waits_for_the_follow_up_to_an_auto_closed_pr() {
        let issue = resolved_issue("mock");
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = watcher_with_agent(
            test_config(),
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])),
            Arc::clone(&tracker),
            crashing_agent(),
        );
        record_pr(&tracker, &issue);
        watcher.set_running(true);
        let feedback = watcher.feedback_analyzer.lock().await;
        let mut closing = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.check_and_auto_close_prs().await }
        });
        closed_during(&tracker, &issue, &mut closing).await;

        let drain = watcher.stop_and_drain();
        tokio::pin!(drain);
        let while_following_up =
            tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 3, drain.as_mut()).await;

        assert!(
            while_following_up.is_err(),
            "the drain must wait while the auto-closed PR's follow-up is in flight"
        );
        assert_eq!(
            watcher.in_flight(),
            1,
            "the auto-closed PR's follow-up must count as a run in flight"
        );

        drop(feedback);

        assert!(
            drain.await,
            "the drain must end once the auto-closed PR's follow-up finishes"
        );
        let closed = closing.await;
        assert!(
            matches!(&closed, Ok(Ok(urls)) if *urls == [PR_URL]),
            "the auto-close should finish closing the PR: {closed:?}"
        );
    }

    /// Agent that holds each fix run and reply at its gate, then fails the run
    /// and replies with [`CUSTOMER_REPLY`].
    struct GatedAgent {
        gate: Arc<Gate>,
    }

    #[async_trait]
    impl AgentRunner for GatedAgent {
        fn name(&self) -> &str {
            "gated-agent"
        }
        fn capabilities(&self) -> claudear_integrations::runner::ProviderCapabilities {
            claudear_integrations::runner::ProviderCapabilities::default()
        }
        fn build_prompt_for_issue(
            &self,
            _issue: &Issue,
            _context: &str,
            _project_dir: &std::path::Path,
        ) -> String {
            String::new()
        }
        async fn execute_with_attempt(
            &self,
            _prompt: &str,
            _issue: Option<&Issue>,
            _attempt_id: Option<i64>,
            _project_dir: &std::path::Path,
        ) -> Result<claudear_core::types::AgentResult> {
            self.gate.pass().await;
            Err(claudear_core::error::Error::runner("gated run failed"))
        }
        async fn generate_reply(
            &self,
            _issue: &Issue,
            _context: &str,
            _guideline: Option<&str>,
            _kind: ReplyKind,
            _project_dir: &std::path::Path,
        ) -> Result<String> {
            self.gate.pass().await;
            Ok(CUSTOMER_REPLY.to_string())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_stop_and_drain_waits_for_the_reply_to_a_merged_fix() {
        let issue = fixed_issue("mock");
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let gate = Arc::new(Gate::default());
        let mut config = test_config();
        config.notifiers.helpscout.enabled = true;
        let watcher = watcher_with_scm(
            config,
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])),
            Arc::clone(&tracker),
            Arc::new(GatedAgent {
                gate: Arc::clone(&gate),
            }),
            Arc::new(MockScm::reporting(PrStatus::Merged)),
        );
        open_pr(&watcher, &tracker, &issue);
        watcher.set_running(true);
        let mut merges = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.check_pr_merges_and_cascade().await }
        });
        gate.arrival_during(&mut merges).await;

        let drain = watcher.stop_and_drain();
        tokio::pin!(drain);
        let while_replying = tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 3, drain.as_mut()).await;

        assert!(
            while_replying.is_err(),
            "the drain must wait while the merged fix's reply is in flight"
        );
        assert_eq!(
            watcher.in_flight(),
            1,
            "the merge follow-up must count as a run in flight"
        );

        gate.open();
        let drained = tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 2, drain).await;

        assert_eq!(
            drained,
            Ok(true),
            "the drain must end once the merge follow-up finishes"
        );
        let checked = merges.await;
        assert!(
            matches!(checked, Ok(Ok(()))),
            "the merge check should finish: {checked:?}"
        );
        assert_eq!(
            attempt_for(&tracker, &issue).status,
            FixAttemptStatus::Merged
        );
        assert!(!gate.was_cancelled(), "draining must not cancel the reply");
    }

    #[tokio::test(start_paused = true)]
    async fn test_stop_and_drain_waits_for_a_review_run_in_flight() {
        let issue = fixed_issue(GATED_SOURCE);
        let source = Arc::new(GatedSource::default());
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let checkout = tempfile::tempdir().unwrap();
        let watcher = watcher_reviewing_pr(
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            checkout.path(),
        );
        open_pr(&watcher, &tracker, &issue);
        watcher.set_running(true);
        let mut reviews = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.check_reviews().await }
        });
        source.gate.arrival_during(&mut reviews).await;

        let drain = watcher.stop_and_drain();
        tokio::pin!(drain);
        let while_reviewing =
            tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 3, drain.as_mut()).await;

        assert!(
            while_reviewing.is_err(),
            "the drain must wait while the review run is still in flight"
        );
        assert_eq!(
            watcher.in_flight(),
            1,
            "the review run must count as a run in flight"
        );

        source.gate.open();
        let drained = tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 2, drain).await;

        assert_eq!(
            drained,
            Ok(true),
            "the drain must end once the review run finishes"
        );
        let checked = reviews.await;
        assert!(
            matches!(checked, Ok(Ok(()))),
            "the review check should finish: {checked:?}"
        );
        assert!(
            !source.gate.was_cancelled(),
            "draining must not cancel the review run"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_stop_and_drain_waits_for_a_retry_in_flight() {
        let source = Arc::new(GatedSource::default());
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;
        let watcher = watcher_with_agent(
            config,
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            crashing_agent(),
        );
        tracker
            .record_attempt(GATED_SOURCE, "retry-1", "GATED-1")
            .unwrap();
        tracker
            .mark_failed(GATED_SOURCE, "retry-1", "initial failure")
            .unwrap();
        watcher.set_running(true);
        let mut retries = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.process_ready_retries().await }
        });
        source.gate.arrival_during(&mut retries).await;
        let retry_state = || {
            let attempt = tracker
                .get_attempt(GATED_SOURCE, "retry-1")
                .unwrap()
                .expect("the retried attempt should be recorded");
            (attempt.status, attempt.retry_count)
        };

        assert_eq!(
            retry_state(),
            (FixAttemptStatus::Failed, 0),
            "the retry must not spend its attempt while loading the issue"
        );

        let drain = watcher.stop_and_drain();
        tokio::pin!(drain);
        let while_retrying = tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 3, drain.as_mut()).await;

        assert!(
            while_retrying.is_err(),
            "the drain must wait while the retry is still in flight"
        );
        assert_eq!(
            watcher.in_flight(),
            1,
            "the retry must count as a run in flight"
        );

        source.gate.open();
        let drained = tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 2, drain).await;

        assert_eq!(
            drained,
            Ok(true),
            "the drain must end once the retry finishes"
        );
        assert_eq!(
            retry_state(),
            (FixAttemptStatus::Failed, 1),
            "the drain must not end before the retry settles its attempt"
        );
        let retried = retries.await;
        assert!(
            matches!(retried, Ok(Ok(()))),
            "the retries should finish: {retried:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_retry_waiting_for_a_slot_does_not_start_once_stopped() {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.max_concurrent = 1;
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;
        let watcher = watcher_with_agent(
            config,
            Arc::new(MockSource::new("mock")),
            Arc::clone(&tracker),
            crashing_agent(),
        );
        tracker.record_attempt("mock", "retry-1", "MOCK-1").unwrap();
        tracker
            .mark_failed("mock", "retry-1", "initial failure")
            .unwrap();
        watcher.set_running(true);
        let busy = watcher
            .claim_processing("mock:busy".to_string(), false)
            .expect("the source's only slot should be free");
        let retries = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.process_ready_retries().await }
        });
        while tracker
            .get_metrics("ready_retries_found", None, 1)
            .unwrap()
            .is_empty()
        {
            tokio::task::yield_now().await;
        }

        watcher.stop();
        drop(busy);
        let retried = retries.await;

        assert!(
            matches!(retried, Ok(Ok(()))),
            "the retries should finish: {retried:?}"
        );
        assert_eq!(
            tracker
                .get_attempt("mock", "retry-1")
                .unwrap()
                .expect("the retry's attempt should be recorded")
                .retry_count,
            0,
            "a retry whose slot frees after the stop must not start"
        );
    }

    fn retried_issue() -> Issue {
        Issue::new(
            "retry-1",
            "RETRY-1",
            "Crash on an empty list",
            "https://example.com/issues/retry-1",
            GATED_SOURCE,
        )
    }

    /// A config that retries a failed attempt at once, and only once.
    fn single_retry_config() -> Config {
        let mut config = test_config();
        config.retry.max_retries = 1;
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;
        config
    }

    fn record_failed_attempt(tracker: &SqliteTracker, issue: &Issue) {
        tracker
            .record_attempt(&issue.source, &issue.id, &issue.short_id)
            .unwrap();
        tracker
            .mark_failed(&issue.source, &issue.id, "initial failure")
            .unwrap();
    }

    /// Every field of `issue`'s attempt, to tell whether a retry changed it.
    fn attempt_snapshot(tracker: &SqliteTracker, issue: &Issue) -> serde_json::Value {
        serde_json::to_value(attempt_for(tracker, issue)).unwrap()
    }

    fn is_ready_to_retry(tracker: &Arc<SqliteTracker>, issue: &Issue) -> bool {
        RetryManager::new(
            single_retry_config().retry,
            Arc::clone(tracker) as Arc<dyn FixAttemptTracker>,
        )
        .get_ready_retries()
        .unwrap()
        .iter()
        .any(|attempt| attempt.issue_id == issue.id)
    }

    /// The decisions `tracker` recorded, most recent first.
    fn decisions(tracker: &SqliteTracker) -> Vec<String> {
        tracker
            .get_recent_activities(100, None)
            .unwrap()
            .into_iter()
            .filter_map(|activity| Some(activity.metadata?["decision"].as_str()?.to_string()))
            .collect()
    }

    fn spawn_ready_retries(watcher: &Arc<Watcher>) -> tokio::task::JoinHandle<Result<()>> {
        let watcher = Arc::clone(watcher);
        tokio::spawn(async move { watcher.process_ready_retries().await })
    }

    #[tokio::test]
    async fn test_retry_loading_its_issue_across_a_stop_keeps_its_attempt() {
        let issue = retried_issue();
        let source = Arc::new(GatedSource {
            issue: Some(issue.clone()),
            ..GatedSource::default()
        });
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let agent_calls = Arc::new(AtomicUsize::new(0));
        let watcher = watcher_with_agent(
            single_retry_config(),
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            Arc::new(ScriptedQaAgent {
                calls: Arc::clone(&agent_calls),
                answer: QaAnswer::Crash,
            }),
        );
        record_failed_attempt(&tracker, &issue);
        let before = attempt_snapshot(&tracker, &issue);
        watcher.set_running(true);
        let mut retries = spawn_ready_retries(&watcher);
        source.gate.arrival_during(&mut retries).await;

        watcher.stop();
        source.gate.open();
        let retried = retries.await;

        assert!(
            matches!(retried, Ok(Ok(()))),
            "the retries should finish: {retried:?}"
        );
        assert_eq!(
            attempt_snapshot(&tracker, &issue),
            before,
            "a retry the stop refused must leave its attempt untouched"
        );
        assert_eq!(
            agent_calls.load(AtomicOrdering::SeqCst),
            0,
            "a retry the stop refused must not run the agent"
        );
        assert!(
            is_ready_to_retry(&tracker, &issue),
            "a retry the stop refused must keep the attempt's only retry"
        );
        assert!(
            tracker
                .get_metrics("ready_retry_failed", None, 10)
                .unwrap()
                .is_empty(),
            "a retry the stop refused must not count as a failed retry"
        );
        assert!(
            !decisions(&tracker).contains(&"ready_retry_trigger_failed".to_string()),
            "a retry the stop refused must not be recorded as a failed trigger"
        );
    }

    #[tokio::test]
    async fn test_retry_spends_its_retry_only_once_its_issue_fails_to_load() {
        let issue = retried_issue();
        let source = Arc::new(GatedSource::default());
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = watcher_with_agent(
            single_retry_config(),
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            crashing_agent(),
        );
        record_failed_attempt(&tracker, &issue);
        let before = attempt_snapshot(&tracker, &issue);
        watcher.set_running(true);
        let mut retries = spawn_ready_retries(&watcher);
        source.gate.arrival_during(&mut retries).await;

        assert_eq!(
            attempt_snapshot(&tracker, &issue),
            before,
            "a retry must not be spent while its issue is loading"
        );

        source.gate.open();
        let retried = retries.await;

        assert!(
            matches!(retried, Ok(Ok(()))),
            "the retries should finish: {retried:?}"
        );
        let attempt = attempt_for(&tracker, &issue);
        assert_eq!(
            (attempt.status, attempt.retry_count),
            (FixAttemptStatus::Failed, 1),
            "an issue that fails to load must spend its retry and stay failed"
        );
        assert!(
            attempt
                .error_message
                .as_deref()
                .is_some_and(|message| message.starts_with(RETRY_TRIGGER_FAILED)),
            "unexpected error message: {:?}",
            attempt.error_message
        );
        assert!(
            !is_ready_to_retry(&tracker, &issue),
            "an issue that keeps failing to load must run out of retries"
        );
        assert_eq!(
            tracker
                .get_metrics("ready_retry_failed", None, 10)
                .unwrap()
                .len(),
            1,
            "an issue that fails to load is a failed retry"
        );
    }

    #[tokio::test]
    async fn test_retry_of_an_issue_already_being_processed_keeps_its_attempt() {
        let issue = retried_issue();
        let source = Arc::new(GatedSource {
            issue: Some(issue.clone()),
            ..GatedSource::default()
        });
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let agent_calls = Arc::new(AtomicUsize::new(0));
        let watcher = watcher_with_agent(
            single_retry_config(),
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            Arc::new(ScriptedQaAgent {
                calls: Arc::clone(&agent_calls),
                answer: QaAnswer::Crash,
            }),
        );
        record_failed_attempt(&tracker, &issue);
        let before = attempt_snapshot(&tracker, &issue);
        watcher.set_running(true);
        let mut retries = spawn_ready_retries(&watcher);
        source.gate.arrival_during(&mut retries).await;

        let processing = watcher
            .claim_processing(format!("{GATED_SOURCE}:{}", issue.id), false)
            .expect("nothing else should be processing the issue yet");
        source.gate.open();
        let retried = retries.await;
        drop(processing);

        assert!(
            matches!(retried, Ok(Ok(()))),
            "the retries should finish: {retried:?}"
        );
        assert_eq!(
            attempt_snapshot(&tracker, &issue),
            before,
            "a retry of an issue already being processed must leave its attempt untouched"
        );
        assert_eq!(
            agent_calls.load(AtomicOrdering::SeqCst),
            0,
            "a retry of an issue already being processed must not run the agent"
        );
        assert!(
            is_ready_to_retry(&tracker, &issue),
            "a retry of an issue already being processed must keep the attempt's only retry"
        );
        assert!(
            decisions(&tracker).contains(&"ready_retry_skipped_inflight".to_string()),
            "the retry should be recorded as skipped for an issue in flight"
        );
    }

    /// Source named [`GATED_SOURCE`] that cannot be reached, so none of its
    /// issues load.
    struct UnreachableSource;

    #[async_trait]
    impl IssueSource for UnreachableSource {
        fn name(&self) -> &str {
            GATED_SOURCE
        }
        fn display_name(&self) -> &str {
            GATED_SOURCE
        }
        async fn fetch_issues(&self) -> Result<Vec<Issue>> {
            Ok(vec![])
        }
        fn matches_criteria(&self, _issue: &Issue) -> MatchResult {
            MatchResult::matched("Unreachable match", MatchPriority::Normal)
        }
        async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
            Ok(format!("Context for {}", issue.short_id))
        }
        async fn get_issue(&self, _id: &str) -> Result<Issue> {
            Err(claudear_core::error::Error::network("connection refused"))
        }
    }

    #[tokio::test]
    async fn test_retry_keeps_its_retry_when_its_source_cannot_be_reached() {
        let issue = retried_issue();
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = watcher_with_agent(
            single_retry_config(),
            Arc::new(UnreachableSource) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            crashing_agent(),
        );
        record_failed_attempt(&tracker, &issue);
        let before = attempt_snapshot(&tracker, &issue);
        watcher.set_running(true);

        watcher.process_ready_retries().await.unwrap();

        assert_eq!(
            attempt_snapshot(&tracker, &issue),
            before,
            "a retry whose source cannot be reached must leave its attempt untouched"
        );
        assert!(
            is_ready_to_retry(&tracker, &issue),
            "a retry whose source cannot be reached must keep the attempt's only retry"
        );
        assert_eq!(
            tracker
                .get_metrics("ready_retry_failed", None, 10)
                .unwrap()
                .len(),
            1,
            "a retry whose source cannot be reached is still a failed retry"
        );
    }

    #[tokio::test]
    async fn test_retry_keeps_its_retry_while_paused_for_a_rate_limit() {
        let issue = Issue::new(
            "paused-1",
            "PAUSED-1",
            "Crash on an empty list",
            "https://example.com/issues/paused-1",
            "mock",
        );
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let agent_calls = Arc::new(AtomicUsize::new(0));
        let watcher = watcher_with_agent(
            single_retry_config(),
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            Arc::new(ScriptedQaAgent {
                calls: Arc::clone(&agent_calls),
                answer: QaAnswer::Crash,
            }),
        );
        record_failed_attempt(&tracker, &issue);
        let before = attempt_snapshot(&tracker, &issue);
        watcher.set_running(true);
        watcher.rate_limit_pause_until.write().await.insert(
            "claude".to_string(),
            Utc::now() + chrono::Duration::hours(1),
        );

        let outcome = watcher
            .retry(&attempt_for(&tracker, &issue), "Retry attempt 1")
            .await;

        assert!(
            matches!(outcome, RetryOutcome::Failed(_)),
            "a retry refused while paused should report why it did not run: {outcome:?}"
        );
        assert_eq!(
            attempt_snapshot(&tracker, &issue),
            before,
            "a retry refused while paused for a rate limit must leave its attempt untouched"
        );
        assert_eq!(
            agent_calls.load(AtomicOrdering::SeqCst),
            0,
            "a retry refused while paused for a rate limit must not run the agent"
        );
    }

    #[tokio::test]
    async fn test_retry_that_ends_on_a_rate_limit_gets_its_retry_back() {
        let mut config = single_retry_config();
        config.agent.default_provider = SCRIPTED_QA_PROVIDER.to_string();
        let harness = DeployQaHarness::answering(
            config,
            QaAnswer::Fail("Claude rate limit hit: some error".to_string()),
        );
        let issue = harness.issue();
        record_failed_attempt(&harness.tracker, &issue);
        harness.watcher.set_running(true);

        harness.watcher.process_ready_retries().await.unwrap();

        assert_eq!(harness.agent_calls(), 1, "the retry should have run");
        let attempt = attempt_for(&harness.tracker, &issue);
        assert_eq!(
            (attempt.status, attempt.retry_count),
            (FixAttemptStatus::Failed, 0),
            "a retry that ends on a rate limit must get its retry back"
        );
    }

    fn triggered_issue() -> Issue {
        Issue::new(
            "trigger-1",
            "TRIGGER-1",
            "Crash on an empty list",
            "https://example.com/issues/trigger-1",
            GATED_SOURCE,
        )
    }

    fn is_stopping_refusal(triggered: &Result<()>) -> bool {
        matches!(triggered, Err(error) if error.to_string() == STOPPING_REFUSAL)
    }

    #[tokio::test]
    async fn test_trigger_on_a_stopped_watcher_refuses_without_loading_the_issue() {
        let issue = triggered_issue();
        let source = Arc::new(GatedSource {
            issue: Some(issue.clone()),
            ..GatedSource::default()
        });
        let watcher = watcher_with_agent(
            test_config(),
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::new(SqliteTracker::in_memory().unwrap()),
            crashing_agent(),
        );
        watcher.stop();

        let mut trigger = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move { watcher.trigger_issue(GATED_SOURCE, &issue.id).await }
        });
        let triggered = tokio::select! {
            () = source.gate.arrival() => panic!("a stopped watcher must not load the issue"),
            triggered = &mut trigger => triggered.expect("the trigger should not panic"),
        };

        assert!(
            is_stopping_refusal(&triggered),
            "a trigger on a stopped watcher must be refused as stopping: {triggered:?}"
        );
    }

    #[tokio::test]
    async fn test_trigger_loading_its_issue_across_a_stop_starts_no_run() {
        let issue = triggered_issue();
        let source = Arc::new(GatedSource {
            issue: Some(issue.clone()),
            ..GatedSource::default()
        });
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let agent_calls = Arc::new(AtomicUsize::new(0));
        let watcher = watcher_with_agent(
            test_config(),
            Arc::clone(&source) as Arc<dyn IssueSource>,
            Arc::clone(&tracker),
            Arc::new(ScriptedQaAgent {
                calls: Arc::clone(&agent_calls),
                answer: QaAnswer::Crash,
            }),
        );
        watcher.set_running(true);
        let mut trigger = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            let issue_id = issue.id.clone();
            async move { watcher.trigger_issue(GATED_SOURCE, &issue_id).await }
        });
        source.gate.arrival_during(&mut trigger).await;

        watcher.stop();
        source.gate.open();
        let triggered = trigger.await.expect("the trigger should not panic");

        assert!(
            is_stopping_refusal(&triggered),
            "a trigger whose issue loads after the stop must be refused as stopping: {triggered:?}"
        );
        assert!(
            !tracker.has_attempted(GATED_SOURCE, &issue.id).unwrap(),
            "a trigger refused by the stop must not record an attempt"
        );
        assert_eq!(
            agent_calls.load(AtomicOrdering::SeqCst),
            0,
            "a trigger refused by the stop must not run the agent"
        );
    }

    const UPSTREAM_REPO: &str = "org/lib";
    const UPSTREAM_PR_URL: &str = "https://github.com/org/lib/pull/3";
    const DOWNSTREAM_REPO: &str = "app";
    const RELEASE_TAG: &str = "v1.0.0";

    fn run_git(directory: &std::path::Path, arguments: &[&str]) {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A clone of [`DOWNSTREAM_REPO`] under `root` whose origin has one commit
    /// on `main`.
    fn downstream_clone(root: &std::path::Path) -> std::path::PathBuf {
        let origin = format!("{DOWNSTREAM_REPO}.git");
        run_git(root, &["init", "--bare", "--initial-branch=main", &origin]);
        run_git(root, &["clone", &origin, DOWNSTREAM_REPO]);
        let clone = root.join(DOWNSTREAM_REPO);
        run_git(
            &clone,
            &[
                "-c",
                "user.name=claudear",
                "-c",
                "user.email=claudear@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "--no-verify",
                "-m",
                "Initial commit",
            ],
        );
        run_git(&clone, &["push", "--no-verify", "origin", "HEAD:main"]);
        clone
    }

    /// A watcher whose merged fix in [`UPSTREAM_REPO`] cascades to a local
    /// clone of [`DOWNSTREAM_REPO`] once `scm` reports a release.
    struct ReleaseHarness {
        watcher: Arc<Watcher>,
        scm: Arc<MockScm>,
        _root: tempfile::TempDir,
    }

    impl ReleaseHarness {
        fn new(scm: MockScm, agent: Arc<dyn AgentRunner>) -> Self {
            let root = tempfile::tempdir().unwrap();
            let mut index = RepoIndex::new();
            index.add_repo(IndexedRepo::new(
                DOWNSTREAM_REPO,
                downstream_clone(root.path()),
            ));
            let mut config = test_config();
            config.workspace = root.path().join("workspace");
            config.cascade.enabled = true;
            config.cascade.rules = vec![CascadeRule {
                upstream: UPSTREAM_REPO.to_string(),
                downstream: DOWNSTREAM_REPO.to_string(),
                trigger: CascadeTrigger::Release,
                target_branch: None,
                version_update: true,
                instructions: None,
            }];
            let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
            tracker.record_attempt("mock", "lib-1", "LIB-1").unwrap();
            tracker
                .mark_success("mock", "lib-1", UPSTREAM_PR_URL)
                .unwrap();
            tracker.mark_merged("mock", "lib-1").unwrap();
            let scm = Arc::new(scm);
            let watcher = Arc::new(Watcher::new(WatcherOptions {
                inferrer: Some(RepoInferrer::new(index)),
                relationships: Some(RepoRelationships::new()),
                ..scm_watcher_options(
                    config,
                    Arc::new(MockSource::new("mock")),
                    tracker,
                    agent,
                    Arc::clone(&scm),
                )
            }));
            Self {
                watcher,
                scm,
                _root: root,
            }
        }
    }

    #[tokio::test]
    async fn test_check_releases_on_a_stopped_watcher_checks_no_release() {
        let harness = ReleaseHarness::new(MockScm::reporting(PrStatus::Open), crashing_agent());
        harness.watcher.stop();

        harness.watcher.check_releases_and_cascade().await.unwrap();

        assert_eq!(
            harness.scm.release_check_count(),
            0,
            "a stopped watcher must not look up releases"
        );
    }

    #[tokio::test]
    async fn test_check_releases_stopped_while_checking_starts_no_cascade() {
        let calls = Arc::new(AtomicUsize::new(0));
        let harness = ReleaseHarness::new(
            MockScm::gated(PrStatus::Open),
            Arc::new(ScriptedQaAgent {
                calls: Arc::clone(&calls),
                answer: QaAnswer::Crash,
            }),
        );
        harness.watcher.set_running(true);
        let mut releases = tokio::spawn({
            let watcher = Arc::clone(&harness.watcher);
            async move { watcher.check_releases_and_cascade().await }
        });
        harness.scm.gate.arrival_during(&mut releases).await;
        harness.watcher.stop();
        harness.scm.gate.open();
        let checked = releases.await;

        assert!(
            matches!(checked, Ok(Ok(()))),
            "the release check should finish: {checked:?}"
        );
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            0,
            "a release found after the stop must not start a cascade run"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_stop_and_drain_waits_for_a_release_cascade_in_flight() {
        let gate = Arc::new(Gate::default());
        let harness = ReleaseHarness::new(
            MockScm::reporting(PrStatus::Open),
            Arc::new(GatedAgent {
                gate: Arc::clone(&gate),
            }),
        );
        harness.watcher.set_running(true);
        let mut releases = tokio::spawn({
            let watcher = Arc::clone(&harness.watcher);
            async move { watcher.check_releases_and_cascade().await }
        });
        gate.arrival_during(&mut releases).await;

        let drain = harness.watcher.stop_and_drain();
        tokio::pin!(drain);
        let while_cascading =
            tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 3, drain.as_mut()).await;

        assert!(
            while_cascading.is_err(),
            "the drain must wait while the release cascade is still in flight"
        );
        assert_eq!(
            harness.watcher.in_flight(),
            1,
            "the release cascade must count as a run in flight"
        );

        gate.open();
        let drained = tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 2, drain).await;

        assert_eq!(
            drained,
            Ok(true),
            "the drain must end once the release cascade finishes"
        );
        let checked = releases.await;
        assert!(
            matches!(checked, Ok(Ok(()))),
            "the release check should finish: {checked:?}"
        );
        assert!(
            !gate.was_cancelled(),
            "draining must not cancel the release cascade"
        );
    }

    #[tokio::test]
    async fn test_watcher_start_leaves_a_run_live_in_another_process_pending() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        tracker.record_attempt("mock", "1", "MOCK-1").unwrap();
        let source = Arc::new(MockSource::new("mock")) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source], false);

        let runner = {
            let watcher = Arc::clone(&watcher);
            tokio::spawn(async move { watcher.start(Some(50)).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !watcher.is_running() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the watcher should finish starting");
        watcher.stop();

        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), runner).await;
        assert!(joined.is_ok(), "watcher start loop did not stop in time");
        assert!(joined.unwrap().expect("task join failed").is_ok());
        assert_eq!(
            tracker.get_attempt("mock", "1").unwrap().unwrap().status,
            FixAttemptStatus::Pending,
            "an attempt that started moments ago may still be running in another process sharing \
             the database, so starting this watcher must not fail it"
        );
    }

    #[test]
    fn test_group_review_feedback_by_pr_batches_same_pr() {
        let review1 = claudear_integrations::scm::CodeReview {
            id: 1,
            state: "CHANGES_REQUESTED".to_string(),
            body: Some("first".to_string()),
            user: claudear_integrations::scm::ReviewUser {
                id: 1,
                login: "r1".to_string(),
                user_type: Some("User".to_string()),
            },
            submitted_at: Some("2024-01-01T00:00:00Z".to_string()),
            html_url: None,
        };
        let review2 = claudear_integrations::scm::CodeReview {
            id: 2,
            state: "COMMENTED".to_string(),
            body: Some("second".to_string()),
            user: claudear_integrations::scm::ReviewUser {
                id: 2,
                login: "r2".to_string(),
                user_type: Some("User".to_string()),
            },
            submitted_at: Some("2024-01-01T00:01:00Z".to_string()),
            html_url: None,
        };

        let events = vec![
            claudear_integrations::scm::ReviewEvent::ReviewSubmitted {
                pr_url: "https://github.com/org/repo/pull/1".to_string(),
                repo: "org/repo".to_string(),
                pr_number: 1,
                review: review1,
                inline_comments: vec![],
            },
            claudear_integrations::scm::ReviewEvent::ReviewSubmitted {
                pr_url: "https://github.com/org/repo/pull/1".to_string(),
                repo: "org/repo".to_string(),
                pr_number: 1,
                review: review2,
                inline_comments: vec![],
            },
            claudear_integrations::scm::ReviewEvent::CommentsAdded {
                pr_url: "https://github.com/org/repo/pull/2".to_string(),
                repo: "org/repo".to_string(),
                pr_number: 2,
                comments: vec![], // requires_action = false, should be ignored
            },
        ];

        let grouped = Watcher::group_review_feedback_by_pr(events);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].0, "https://github.com/org/repo/pull/1");
        assert_eq!(grouped[0].2, 2);
        assert!(grouped[0].1.contains("first"));
        assert!(grouped[0].1.contains("second"));
        assert!(grouped[0].1.contains("---"));
        // Reviews carry no ledger comment ids.
        assert!(grouped[0].3.is_empty());
    }

    #[test]
    fn test_group_review_feedback_collects_comment_ids() {
        // CommentsAdded events contribute their comment ids so acknowledgement can
        // target exactly the batch, not the whole PR.
        let mk = |id: i64| claudear_integrations::scm::ReviewComment {
            id,
            path: String::new(),
            position: None,
            original_position: None,
            body: "@claudear fix".to_string(),
            user: claudear_integrations::scm::ReviewUser {
                id: 1,
                login: "reviewer".to_string(),
                user_type: None,
            },
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
            html_url: format!("h{}", id),
            pull_request_review_id: None,
            start_line: None,
            line: None,
            side: None,
        };
        let events = vec![
            claudear_integrations::scm::ReviewEvent::CommentsAdded {
                pr_url: "https://github.com/org/repo/pull/1".to_string(),
                repo: "org/repo".to_string(),
                pr_number: 1,
                comments: vec![mk(101), mk(102)],
            },
            claudear_integrations::scm::ReviewEvent::CommentsAdded {
                pr_url: "https://github.com/org/repo/pull/1".to_string(),
                repo: "org/repo".to_string(),
                pr_number: 1,
                comments: vec![mk(103)],
            },
        ];

        let grouped = Watcher::group_review_feedback_by_pr(events);
        assert_eq!(grouped.len(), 1);
        let mut refs = grouped[0].3.clone();
        refs.sort();
        // Empty-path comments are conversation-kind; each ref namespaces its id.
        assert_eq!(
            refs,
            vec![
                (101, "conversation"),
                (102, "conversation"),
                (103, "conversation")
            ]
        );
    }

    /// A review watcher watching `pr_url` for issue `1` of the `mock` source,
    /// whose SCM reports nothing new, so each check surfaces only the comments
    /// `tracker` still holds unhandled.
    fn review_watcher_watching(
        tracker: Arc<SqliteTracker>,
        pr_url: &str,
        repo: &str,
        pr_number: i64,
    ) -> Arc<ReviewWatcher> {
        let review_watcher = Arc::new(ReviewWatcher::with_tracker(
            Arc::new(MergedPrs(Vec::new())),
            tracker,
        ));
        review_watcher.watch_pr(PrReviewState::new(pr_url, repo, pr_number, "1", "mock"));
        review_watcher
    }

    /// A live test watcher that acts on the feedback `review_watcher` surfaces
    /// and resolves repositories through `inferrer`.
    fn create_reviewing_test_watcher(
        tracker: Arc<SqliteTracker>,
        source: Arc<dyn IssueSource>,
        inferrer: RepoInferrer,
        review_watcher: Arc<ReviewWatcher>,
    ) -> Arc<Watcher> {
        Arc::new(Watcher::new(WatcherOptions {
            inferrer: Some(inferrer),
            review_watcher: Some(review_watcher),
            ..live_watcher_options(Arc::new(MockNotifier::new(true)), tracker, vec![source])
        }))
    }

    /// A conversation comment on a PR asking claudear for `body`.
    fn review_comment(id: i64, body: &str) -> claudear_integrations::scm::ReviewComment {
        claudear_integrations::scm::ReviewComment {
            id,
            path: String::new(),
            position: None,
            original_position: None,
            body: body.to_string(),
            user: claudear_integrations::scm::ReviewUser {
                login: "reviewer".to_string(),
                id: 1,
                user_type: None,
            },
            created_at: "2026-09-30T00:00:00Z".to_string(),
            updated_at: "2026-09-30T00:00:00Z".to_string(),
            html_url: String::new(),
            pull_request_review_id: None,
            start_line: None,
            line: None,
            side: None,
        }
    }

    /// The repositories an issue's runs resolved, as its timeline records them.
    fn resolved_repositories(tracker: &SqliteTracker, source: &str, issue_id: &str) -> Vec<String> {
        tracker
            .get_activities_for_issue(source, issue_id)
            .unwrap()
            .into_iter()
            .filter(|activity| activity.activity_type == TimelineEventStatus::RepoResolved.as_str())
            .filter_map(|activity| {
                activity.metadata.and_then(|metadata| {
                    metadata
                        .get("repo")
                        .and_then(|repo| repo.as_str())
                        .map(str::to_string)
                })
            })
            .collect()
    }

    /// The details of every decision recorded for an issue to defer its review
    /// rerun.
    fn review_rerun_deferrals(
        tracker: &SqliteTracker,
        source: &str,
        issue_id: &str,
    ) -> Vec<serde_json::Value> {
        tracker
            .get_activities_for_issue(source, issue_id)
            .unwrap()
            .into_iter()
            .filter(|activity| activity.activity_type == "decision")
            .filter_map(|activity| activity.metadata)
            .filter(|metadata| metadata["decision"] == "review_rerun_deferred")
            .map(|metadata| metadata["details"].clone())
            .collect()
    }

    /// The review cycles `pr_url` has used, as stored.
    fn review_cycles_used(tracker: &SqliteTracker, pr_url: &str) -> i32 {
        tracker
            .get_pr(pr_url)
            .unwrap()
            .map_or(0, |record| record.review_cycles)
    }

    /// A source holding one issue whose first fetch waits at `gate`, so
    /// processes that each fetch the issue from their own copy all reach that
    /// fetch before any of them goes past it.
    struct BarrierSource {
        issue: Issue,
        gate: Arc<tokio::sync::Barrier>,
        gated: AtomicBool,
    }

    impl BarrierSource {
        fn new(issue: Issue, gate: Arc<tokio::sync::Barrier>) -> Self {
            Self {
                issue,
                gate,
                gated: AtomicBool::new(true),
            }
        }
    }

    #[async_trait]
    impl IssueSource for BarrierSource {
        fn name(&self) -> &str {
            &self.issue.source
        }
        fn display_name(&self) -> &str {
            &self.issue.source
        }
        async fn fetch_issues(&self) -> Result<Vec<Issue>> {
            Ok(vec![self.issue.clone()])
        }
        fn matches_criteria(&self, _issue: &Issue) -> MatchResult {
            MatchResult::matched("Gated match", MatchPriority::Normal)
        }
        async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
            Ok(format!("Context for {}", issue.short_id))
        }
        async fn get_issue(&self, id: &str) -> Result<Issue> {
            if self.gated.swap(false, AtomicOrdering::SeqCst) {
                self.gate.wait().await;
            }
            if id == self.issue.id {
                Ok(self.issue.clone())
            } else {
                Err(claudear_core::error::Error::source(
                    &self.issue.source,
                    "Issue not found",
                ))
            }
        }
    }

    #[tokio::test]
    async fn test_process_review_action_waits_for_inflight_issue_processing() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        tracker.record_attempt("mock", "1", "MOCK-1").unwrap();
        tracker
            .mark_success("mock", "1", "https://github.com/org/repo/pull/1")
            .unwrap();

        let source = Arc::new(MockSource::with_issues(
            "mock",
            vec![Issue::new(
                "1",
                "MOCK-1",
                "Mock issue",
                "http://example.com/mock/1",
                "mock",
            )],
        )) as Arc<dyn IssueSource>;

        let checkout = tempfile::tempdir().unwrap();
        let watcher = create_test_watcher_with_inferrer(
            notifier,
            tracker.clone(),
            vec![source],
            inferrer_indexing(IndexedRepo::new("org/repo", checkout.path())),
        );
        watcher.is_running.store(true, Ordering::SeqCst);

        watcher.lock_processing().insert("mock:1".to_string());

        let release = Arc::clone(&watcher);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            release.lock_processing().remove("mock:1");
        });

        let attempt = tracker.get_attempt("mock", "1").unwrap().unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            watcher.process_review_action(&attempt, "Please address review feedback"),
        )
        .await;
        assert!(result.is_ok(), "process_review_action timed out");
        assert!(
            result.unwrap().is_ok(),
            "process_review_action returned error"
        );

        let updated_attempt = tracker.get_attempt("mock", "1").unwrap().unwrap();
        assert_eq!(
            updated_attempt.status,
            claudear_core::types::FixAttemptStatus::Failed,
            "review rerun should execute after lock release (fetching the repo fails in test setup, marking failed)"
        );
    }

    #[tokio::test]
    async fn test_review_feedback_stops_triggering_reruns_after_the_cycle_cap() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let pr_url = "https://github.com/org/repo/pull/1";
        // No PR record exists, like a cascade PR that is watched without one
        tracker.record_attempt("mock", "1", "MOCK-1").unwrap();
        tracker.mark_success("mock", "1", pr_url).unwrap();

        let source = Arc::new(MockSource::with_issues(
            "mock",
            vec![Issue::new(
                "1",
                "MOCK-1",
                "Mock issue",
                "http://example.com/mock/1",
                "mock",
            )],
        )) as Arc<dyn IssueSource>;
        let checkout = tempfile::tempdir().unwrap();
        let watcher = create_test_watcher_with_inferrer(
            notifier,
            tracker.clone(),
            vec![source],
            inferrer_indexing(IndexedRepo::new("org/repo", checkout.path())),
        );
        watcher.is_running.store(true, Ordering::SeqCst);

        // Each allowed rerun fails fetching the repo in this setup; reset to the
        // open-PR state before the next round of feedback arrives
        for _ in 0..MAX_REVIEW_CYCLES {
            let attempt = tracker.get_attempt("mock", "1").unwrap().unwrap();
            watcher
                .process_review_action(&attempt, "Please add a test")
                .await
                .unwrap();
            assert_eq!(
                tracker.get_attempt("mock", "1").unwrap().unwrap().status,
                claudear_core::types::FixAttemptStatus::Failed,
                "a rerun under the cap should run"
            );
            tracker.mark_success("mock", "1", pr_url).unwrap();
        }

        let attempt = tracker.get_attempt("mock", "1").unwrap().unwrap();
        watcher
            .process_review_action(&attempt, "Please add a test")
            .await
            .unwrap();
        assert_eq!(
            tracker.get_attempt("mock", "1").unwrap().unwrap().status,
            claudear_core::types::FixAttemptStatus::Success,
            "feedback past the cap must not start another run"
        );
    }

    #[tokio::test]
    async fn test_feedback_after_reruns_that_never_started_still_starts_a_rerun() {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let pr_url = "https://github.com/org/repo/pull/1";
        tracker.record_attempt("mock", "1", "MOCK-1").unwrap();
        tracker.mark_success("mock", "1", pr_url).unwrap();
        let review_watcher = review_watcher_watching(tracker.clone(), pr_url, "org/repo", 1);
        let checkout = tempfile::tempdir().unwrap();
        let indexing_pr_repo = || inferrer_indexing(IndexedRepo::new("org/repo", checkout.path()));
        let attempt = tracker.get_attempt("mock", "1").unwrap().unwrap();

        let issue_unavailable = create_reviewing_test_watcher(
            tracker.clone(),
            Arc::new(MockSource::new("mock")),
            indexing_pr_repo(),
            review_watcher.clone(),
        );
        for _ in 0..5 {
            assert!(
                issue_unavailable
                    .process_review_action(&attempt, "Please add a test")
                    .await
                    .is_err(),
                "a rerun whose issue cannot be fetched must fail so its feedback is retried"
            );
        }

        let issue_available = create_reviewing_test_watcher(
            tracker.clone(),
            Arc::new(MockSource::with_issues(
                "mock",
                vec![Issue::new(
                    "1",
                    "MOCK-1",
                    "Mock issue",
                    "http://example.com/mock/1",
                    "mock",
                )],
            )),
            indexing_pr_repo(),
            review_watcher.clone(),
        );
        issue_available
            .lock_processing()
            .insert("mock:1".to_string());
        for _ in 0..5 {
            assert!(
                issue_available
                    .process_review_action(&attempt, "Please add a test")
                    .await
                    .is_err(),
                "a rerun still waiting on the run that holds its issue when the watcher stops \
                 must fail so its feedback is retried"
            );
        }
        issue_available.lock_processing().remove("mock:1");
        issue_available.is_running.store(true, Ordering::SeqCst);
        issue_available
            .process_review_action(&attempt, "Please add a test")
            .await
            .unwrap();

        assert!(
            review_watcher.get_state(pr_url).is_some(),
            "reruns that never started must not leave the PR to humans"
        );
        assert_eq!(
            tracker.get_attempt("mock", "1").unwrap().unwrap().status,
            FixAttemptStatus::Failed,
            "feedback after reruns that never started must still start a rerun, which fails \
             fetching the repository in this setup"
        );
    }

    #[tokio::test]
    async fn test_refunding_a_review_cycle_keeps_the_cycle_another_process_charged() {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let pr_url = "https://github.com/org/repo/pull/1";
        tracker.record_attempt("mock", "1", "MOCK-1").unwrap();
        tracker.mark_success("mock", "1", pr_url).unwrap();
        let attempt = tracker.get_attempt("mock", "1").unwrap().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let start_process = || {
            let source = MockSource::with_issues(
                "mock",
                vec![Issue::new(
                    "1",
                    "MOCK-1",
                    "Mock issue",
                    "http://example.com/mock/1",
                    "mock",
                )],
            );
            let watcher = create_test_watcher_with_inferrer(
                Arc::new(MockNotifier::new(true)),
                tracker.clone(),
                vec![Arc::new(source) as Arc<dyn IssueSource>],
                inferrer_indexing(IndexedRepo::new("org/repo", checkout.path())),
            );
            watcher.is_running.store(true, Ordering::SeqCst);
            watcher
        };
        let daemon = start_process();
        let webhook = start_process();
        daemon.lock_processing().insert("mock:1".to_string());

        let daemon_review = daemon.process_review_action(&attempt, "Please add a test");
        let webhook_review_while_daemon_waits = async {
            while review_cycles_used(&tracker, pr_url) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let read_before_webhook_charge = tracker.get_pr(pr_url).unwrap().unwrap();
            webhook
                .process_review_action(&attempt, "Please add a test")
                .await
                .unwrap();
            // A read-modify-write of the PR record that read it before the webhook
            // process's charge writes it back now, as interleaved processes can
            tracker.upsert_pr(&read_before_webhook_charge).unwrap();
            daemon.is_running.store(false, Ordering::SeqCst);
        };
        let (daemon_outcome, ()) = tokio::time::timeout(
            Duration::from_secs(30),
            futures::future::join(daemon_review, webhook_review_while_daemon_waits),
        )
        .await
        .expect("the daemon's waiting rerun must give up once its watcher stops");

        assert!(
            daemon_outcome.is_err(),
            "the daemon's rerun never started, so it must fail for its feedback to be retried"
        );
        assert_eq!(
            review_cycles_used(&tracker, pr_url),
            1,
            "the daemon's refund must give back only its own cycle, leaving the webhook \
             process's rerun counted"
        );
    }

    #[tokio::test]
    async fn test_processes_racing_for_a_prs_last_review_cycle_start_only_one_rerun() {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let pr_url = "https://github.com/org/repo/pull/1";
        tracker.record_attempt("mock", "1", "MOCK-1").unwrap();
        tracker.mark_success("mock", "1", pr_url).unwrap();
        let mut record = PrRecord::new(pr_url, "org/repo", 1);
        record.review_cycles = MAX_REVIEW_CYCLES - 1;
        tracker.upsert_pr(&record).unwrap();
        let cycles_before = review_cycles_used(&tracker, pr_url);
        let attempt = tracker.get_attempt("mock", "1").unwrap().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let both_checked_the_cap = Arc::new(tokio::sync::Barrier::new(2));
        let start_process = || {
            let source = BarrierSource::new(
                Issue::new(
                    "1",
                    "MOCK-1",
                    "Mock issue",
                    "http://example.com/mock/1",
                    "mock",
                ),
                Arc::clone(&both_checked_the_cap),
            );
            let watcher = create_test_watcher_with_inferrer(
                Arc::new(MockNotifier::new(true)),
                tracker.clone(),
                vec![Arc::new(source) as Arc<dyn IssueSource>],
                inferrer_indexing(IndexedRepo::new("org/repo", checkout.path())),
            );
            watcher.is_running.store(true, Ordering::SeqCst);
            watcher
        };
        let daemon = start_process();
        let webhook = start_process();

        let (daemon_outcome, webhook_outcome) = tokio::time::timeout(
            Duration::from_secs(30),
            futures::future::join(
                daemon.process_review_action(&attempt, "Please add a test"),
                webhook.process_review_action(&attempt, "Please add a test"),
            ),
        )
        .await
        .expect("both processes must finish handling the feedback");
        daemon_outcome.unwrap();
        webhook_outcome.unwrap();

        assert_eq!(
            resolved_repositories(&tracker, "mock", "1").len(),
            1,
            "processes that both found the PR's last review cycle free must start only one \
             rerun between them"
        );
        assert_eq!(
            review_cycles_used(&tracker, pr_url),
            cycles_before + 1,
            "only the rerun that started may be counted, so the PR never goes past its cap"
        );
    }

    #[tokio::test]
    async fn test_review_rerun_never_moves_to_an_inferred_repo_when_its_pr_repo_is_unindexed() {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let pr_repo = "org/lib";
        let pr_url = "https://github.com/org/lib/pull/7";
        tracker.record_attempt("mock", "1", "MOCK-1").unwrap();
        tracker.mark_success("mock", "1", pr_url).unwrap();

        let mut issue = Issue::new(
            "1",
            "MOCK-1",
            "Mock issue",
            "http://example.com/mock/1",
            "mock",
        );
        issue.description = Some("Crash in src/app.ts".to_string());
        let source = Arc::new(MockSource::with_issues("mock", vec![issue])) as Arc<dyn IssueSource>;
        let checkout = tempfile::tempdir().unwrap();
        let mut inferred_repo = IndexedRepo::new("org/app", checkout.path());
        inferred_repo.files = vec!["src/app.ts".to_string()];
        let agent_calls = Arc::new(AtomicUsize::new(0));
        let watcher = Arc::new(Watcher::new(WatcherOptions {
            inferrer: Some(inferrer_indexing(inferred_repo)),
            agent: Arc::new(ScriptedQaAgent {
                calls: Arc::clone(&agent_calls),
                answer: QaAnswer::Crash,
            }),
            ..live_watcher_options(
                Arc::new(MockNotifier::new(true)),
                tracker.clone(),
                vec![source],
            )
        }));
        watcher.is_running.store(true, Ordering::SeqCst);

        let attempt = tracker.get_attempt("mock", "1").unwrap().unwrap();
        let _ = watcher
            .process_review_action(&attempt, "Please add a test")
            .await;

        assert_eq!(
            agent_calls.load(AtomicOrdering::SeqCst),
            0,
            "no agent may run for a rerun whose PR repository is not indexed"
        );
        let resolved = resolved_repositories(&tracker, "mock", "1");
        assert!(
            resolved.is_empty(),
            "a review rerun must work in its PR's repository, never one inferred from the \
             issue, but it resolved {resolved:?}"
        );
        let deferrals = review_rerun_deferrals(&tracker, "mock", "1");
        assert_eq!(
            deferrals.len(),
            1,
            "the rerun must record why it did not run"
        );
        assert_eq!(
            deferrals[0]["pr_repo"], pr_repo,
            "the rerun must wait for its PR's repository"
        );
        assert!(
            deferrals[0]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains(pr_repo)),
            "the recorded reason must name the repository that is missing: {}",
            deferrals[0]
        );
        let after = tracker.get_attempt("mock", "1").unwrap().unwrap();
        assert_eq!(
            after.status,
            FixAttemptStatus::Success,
            "a rerun that never started must leave the attempt and its open PR as they were"
        );
        assert_eq!(after.pr_url.as_deref(), Some(pr_url));
    }

    #[tokio::test]
    async fn test_review_feedback_outlasts_a_rerun_deferred_until_its_pr_repository_is_indexed() {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let pr_repo = "org/lib";
        let pr_url = "https://github.com/org/lib/pull/7";
        tracker.record_attempt("mock", "1", "MOCK-1").unwrap();
        tracker.mark_success("mock", "1", pr_url).unwrap();
        tracker
            .record_pr_review_comment(pr_url, &review_comment(42, "@claudear please add a test"))
            .unwrap();
        let review_watcher = review_watcher_watching(tracker.clone(), pr_url, pr_repo, 7);
        let source = Arc::new(MockSource::with_issues(
            "mock",
            vec![Issue::new(
                "1",
                "MOCK-1",
                "Mock issue",
                "http://example.com/mock/1",
                "mock",
            )],
        )) as Arc<dyn IssueSource>;
        let checkout = tempfile::tempdir().unwrap();
        let unhandled_comments = || -> Vec<i64> {
            tracker
                .get_unhandled_pr_review_comments(pr_url)
                .unwrap()
                .into_iter()
                .map(|comment| comment.id)
                .collect()
        };

        let before_indexing = create_reviewing_test_watcher(
            tracker.clone(),
            source.clone(),
            inferrer_indexing(IndexedRepo::new("org/app", checkout.path())),
            review_watcher.clone(),
        );
        before_indexing.set_running(true);
        for _ in 0..8 {
            before_indexing.check_reviews().await.unwrap();
        }

        assert_eq!(
            unhandled_comments(),
            vec![42],
            "feedback whose rerun waits for the PR's repository to be indexed must stay \
             outstanding however many cycles it waits, not spend its retries and be given up on"
        );
        assert!(
            resolved_repositories(&tracker, "mock", "1").is_empty(),
            "nothing may run while the PR's repository is not indexed"
        );
        assert!(
            !tracker
                .get_activities_for_issue("mock", "1")
                .unwrap()
                .iter()
                .any(|activity| {
                    activity.activity_type == TimelineEventStatus::ProcessingStarted.as_str()
                }),
            "a rerun that is waiting must not show on the timeline as started"
        );
        assert_eq!(
            review_rerun_deferrals(&tracker, "mock", "1").len(),
            1,
            "the wait must be recorded once for operators, not every cycle"
        );

        let after_indexing = create_reviewing_test_watcher(
            tracker.clone(),
            source,
            inferrer_indexing(IndexedRepo::new(pr_repo, checkout.path())),
            review_watcher,
        );
        after_indexing.set_running(true);
        after_indexing.check_reviews().await.unwrap();

        assert_eq!(
            resolved_repositories(&tracker, "mock", "1"),
            vec![pr_repo.to_string()],
            "once the PR's repository is indexed, the next cycle must rerun in it"
        );
        assert!(
            unhandled_comments().is_empty(),
            "the rerun that started settles the feedback"
        );
    }

    #[tokio::test]
    async fn test_process_review_action_exits_when_watcher_stopping() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        tracker.record_attempt("mock", "1", "MOCK-1").unwrap();
        tracker
            .mark_success("mock", "1", "https://github.com/org/repo/pull/1")
            .unwrap();

        let source = Arc::new(MockSource::with_issues(
            "mock",
            vec![Issue::new(
                "1",
                "MOCK-1",
                "Mock issue",
                "http://example.com/mock/1",
                "mock",
            )],
        )) as Arc<dyn IssueSource>;

        let checkout = tempfile::tempdir().unwrap();
        let watcher = create_test_watcher_with_inferrer(
            notifier,
            tracker.clone(),
            vec![source],
            inferrer_indexing(IndexedRepo::new("org/repo", checkout.path())),
        );

        watcher.lock_processing().insert("mock:1".to_string());
        watcher.is_running.store(false, Ordering::SeqCst);

        let attempt = tracker.get_attempt("mock", "1").unwrap().unwrap();
        let result = watcher
            .process_review_action(&attempt, "Please address review feedback")
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Watcher stopping while waiting"),
            "expected watcher-stopping wait error"
        );
    }

    #[tokio::test]
    async fn test_watcher_poll_source_skips_attempted() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Pre-mark one issue as attempted
        tracker.record_attempt("test", "1", "T-1").unwrap();

        let issues = vec![
            Issue::new(
                "1",
                "T-1",
                "Already Attempted",
                "http://example.com/1",
                "test",
            ),
            Issue::new("2", "T-2", "New Issue", "http://example.com/2", "test"),
        ];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;
        let sources = vec![source.clone()];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, true); // dry run

        // poll_source returns Result<()>
        let result = watcher.poll_source(&source).await;
        assert!(result.is_ok());
        // Only the pre-existing one should be in tracker (dry run doesn't add new ones)
        assert!(tracker.has_attempted("test", "1").unwrap());
        assert!(!tracker.has_attempted("test", "2").unwrap()); // Not recorded in dry run
    }

    #[tokio::test]
    async fn test_watcher_trigger_issue_with_known_source() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![Issue::new(
            "123",
            "T-123",
            "Test Issue",
            "http://example.com/123",
            "mock",
        )];
        let source = Arc::new(MockSource::with_issues("mock", issues)) as Arc<dyn IssueSource>;
        let sources = vec![source];

        let watcher = create_test_watcher(notifier, tracker, sources, true); // dry run

        let result = watcher.trigger_issue("mock", "123").await;
        // Should succeed in dry run (doesn't actually process)
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_watcher_trigger_issue_inflight_returns_error() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![Issue::new(
            "123",
            "T-123",
            "Test Issue",
            "http://example.com/123",
            "mock",
        )];
        let source = Arc::new(MockSource::with_issues("mock", issues)) as Arc<dyn IssueSource>;
        let sources = vec![source];

        let watcher = create_test_watcher(notifier, tracker, sources, true);
        watcher.lock_processing().insert("mock:123".to_string());

        let result = watcher.trigger_issue("mock", "123").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("already being processed"));
    }

    #[test]
    fn test_watcher_processing_set() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        assert!(watcher.lock_processing().is_empty());

        watcher.lock_processing().insert("test:123".to_string());
        assert!(watcher.lock_processing().contains("test:123"));

        watcher.lock_processing().remove("test:123");
        assert!(!watcher.lock_processing().contains("test:123"));
    }

    #[test]
    fn test_watcher_config_values() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let mut config = test_config();
        config.max_issues_per_cycle = 10;
        config.max_concurrent = 3;
        config.processing_delay_ms = 500;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources,
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        assert_eq!(watcher.config.max_issues_per_cycle, 10);
        assert_eq!(watcher.config.max_concurrent, 3);
        assert_eq!(watcher.config.processing_delay_ms, 500);
    }

    #[tokio::test]
    async fn test_watcher_seed_with_multiple_sources() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let source1 = Arc::new(MockSource::with_issues(
            "source1",
            vec![Issue::new(
                "1",
                "S1-1",
                "Issue 1",
                "http://example.com/1",
                "source1",
            )],
        )) as Arc<dyn IssueSource>;

        let source2 = Arc::new(MockSource::with_issues(
            "source2",
            vec![Issue::new(
                "2",
                "S2-1",
                "Issue 2",
                "http://example.com/2",
                "source2",
            )],
        )) as Arc<dyn IssueSource>;

        let sources = vec![source1, source2];
        let watcher = create_test_watcher(notifier, tracker.clone(), sources, false);

        let result = watcher.seed().await.unwrap();
        assert_eq!(result.total, 2);
        assert_eq!(*result.by_source.get("source1").unwrap(), 1);
        assert_eq!(*result.by_source.get("source2").unwrap(), 1);
    }

    #[tokio::test]
    async fn test_watcher_poll_respects_max_issues() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Create more issues than max_issues_per_cycle
        let issues: Vec<Issue> = (1..=10)
            .map(|i| {
                Issue::new(
                    format!("{}", i),
                    format!("T-{}", i),
                    format!("Issue {}", i),
                    format!("http://example.com/{}", i),
                    "test",
                )
            })
            .collect();

        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.max_issues_per_cycle = 5;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source.clone()],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: true,
            llm_engine: None,
        }));

        // Poll should complete successfully
        let result = watcher.poll().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_process_ready_retries_marks_failed_when_trigger_fails() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        tracker
            .record_attempt("mock", "missing-1", "MOCK-1")
            .unwrap();
        tracker
            .mark_failed("mock", "missing-1", "initial failure")
            .unwrap();

        let source = Arc::new(MockSource::new("mock")) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;
        config.processing_delay_ms = 0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));
        watcher.is_running.store(true, Ordering::SeqCst);

        watcher.process_ready_retries().await.unwrap();

        let attempt = tracker.get_attempt("mock", "missing-1").unwrap().unwrap();
        assert_eq!(
            attempt.status,
            claudear_core::types::FixAttemptStatus::Failed
        );
        assert_eq!(attempt.retry_count, 1);
        assert!(attempt
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("Retry trigger failed"));
    }

    #[tokio::test]
    async fn test_poll_source_marks_failed_when_repo_resolution_skips() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let source = Arc::new(MockSource::with_issues(
            "mock",
            vec![Issue::new(
                "issue-1",
                "MOCK-1",
                "Issue without resolvable repo",
                "https://example.com/issue-1",
                "mock",
            )],
        )) as Arc<dyn IssueSource>;

        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source.clone()], false);
        watcher.is_running.store(true, Ordering::SeqCst);

        watcher.poll_source(&source).await.unwrap();
        watcher.drain_spawned_tasks().await;

        let attempt = tracker.get_attempt("mock", "issue-1").unwrap().unwrap();
        assert_eq!(
            attempt.status,
            claudear_core::types::FixAttemptStatus::Failed
        );
        assert!(attempt
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("Repository resolution failed"));
    }

    #[test]
    fn test_mock_notifier_call_tracking() {
        let notifier = MockNotifier::new(true);
        assert_eq!(notifier.get_call_count(), 0);
        assert!(notifier.is_enabled());
        assert_eq!(notifier.name(), "mock");
    }

    #[test]
    fn test_mock_notifier_disabled() {
        let notifier = MockNotifier::new(false);
        assert!(!notifier.is_enabled());
    }

    #[tokio::test]
    async fn test_mock_source_get_issue_not_found() {
        let source = MockSource::new("test");
        let result = source.get_issue("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mock_source_get_issue_found() {
        let issues = vec![Issue::new(
            "123",
            "T-123",
            "Test",
            "http://example.com",
            "test",
        )];
        let source = MockSource::with_issues("test", issues);
        let result = source.get_issue("123").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().id, "123");
    }

    #[tokio::test]
    async fn test_mock_source_build_issue_context() {
        let source = MockSource::new("test");
        let issue = test_issue();
        let context = source.build_issue_context(&issue).await.unwrap();
        assert!(context.contains("TEST-123"));
    }

    #[test]
    fn test_mock_source_matches_criteria() {
        let source = MockSource::new("test");
        let issue = test_issue();
        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Normal);
        assert!(result.reason.contains("Mock"));
    }

    #[test]
    fn test_watcher_reset_attempt_success() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        // Record an attempt
        tracker.record_attempt("test", "123", "T-123").unwrap();
        assert!(tracker.has_attempted("test", "123").unwrap());

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, false);

        // Reset should succeed
        let result = watcher.reset_attempt("test", "123");
        assert!(result.is_ok());
        assert!(!tracker.has_attempted("test", "123").unwrap());
    }

    #[test]
    fn test_watcher_get_stats_empty() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let stats = watcher.get_stats().unwrap();
        assert_eq!(stats.total, 0);
        assert_eq!(stats.success, 0);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.pending, 0);
    }

    #[test]
    fn test_watcher_get_stats_after_attempts() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        // Record some attempts
        tracker.record_attempt("test", "1", "T-1").unwrap();
        tracker.record_attempt("test", "2", "T-2").unwrap();
        tracker
            .mark_success("test", "1", "http://github.com/pr/1")
            .unwrap();
        tracker.mark_failed("test", "2", "Error").unwrap();

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let stats = watcher.get_stats().unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.success, 1);
        assert_eq!(stats.failed, 1);
    }

    #[tokio::test]
    async fn test_cascade_triggers_on_merge() {
        use claudear_analysis::repo::DependencyType;
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        // Setup: Create relationships with an upstream and downstream repo
        let mut relationships = RepoRelationships::new();
        relationships
            .add_dependency(
                "upstream-lib",
                "downstream-app",
                DependencyType::Composer,
                None,
            )
            .unwrap();

        // Create a FixAttempt that simulates a merged upstream PR
        let attempt = FixAttempt {
            id: 1,
            issue_id: "ISSUE-123".to_string(),
            short_id: "ISSUE-123".to_string(),
            source: "linear".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: Some("https://github.com/org/upstream-lib/pull/42".to_string()),
            scm_repo: Some("org/upstream-lib".to_string()),
            scm_pr_number: Some(42),
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        // Verify that get_dependants returns the downstream repo
        let dependants = relationships.get_dependants("upstream-lib");
        assert_eq!(dependants.len(), 1);
        assert_eq!(dependants[0].name, "downstream-app");

        // Verify cascade depth calculation for root attempt
        assert_eq!(attempt.parent_attempt_id, None);

        // Verify repo name normalization (scm_repo "org/upstream-lib" -> "upstream-lib")
        let repo_short_name = attempt
            .scm_repo
            .as_ref()
            .unwrap()
            .split('/')
            .next_back()
            .unwrap();
        assert_eq!(repo_short_name, "upstream-lib");
    }

    #[test]
    fn test_cascade_depth_with_no_parent() {
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        let attempt = FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "linear".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Pending,
            error_message: None,
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        // Root attempt has depth 0
        assert!(attempt.parent_attempt_id.is_none());
    }

    #[test]
    fn test_cascade_config_defaults() {
        use claudear_config::config::CascadeConfig;

        let config = CascadeConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.max_depth, 0);
    }

    #[test]
    fn test_truncate_error_short_message() {
        let error = "short error";
        let result = crate::processing::truncate_error_for_activity(error);
        assert_eq!(result, "short error");
    }

    #[test]
    fn test_truncate_error_exactly_500_chars() {
        let error = "a".repeat(500);
        let result = crate::processing::truncate_error_for_activity(&error);
        assert_eq!(result.len(), 500);
        assert!(!result.ends_with("..."));
    }

    #[test]
    fn test_truncate_error_over_500_chars() {
        let error = "a".repeat(600);
        let result = crate::processing::truncate_error_for_activity(&error);
        assert!(result.len() <= 500);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_error_empty_string() {
        let result = crate::processing::truncate_error_for_activity("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_truncate_error_unicode_boundary() {
        // Build a string that has multibyte chars near the 500-char boundary
        let mut error = "a".repeat(497);
        // Add a 4-byte emoji right at the boundary
        error.push('\u{1F600}'); // emoji: 4 bytes
        error.push_str(&"b".repeat(100));
        let result = crate::processing::truncate_error_for_activity(&error);
        assert!(result.ends_with("..."));
        // Verify it doesn't panic and doesn't split a char
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn test_truncate_error_501_chars() {
        let error = "x".repeat(501);
        let result = crate::processing::truncate_error_for_activity(&error);
        assert!(result.ends_with("..."));
        // Should be at most 500 chars (497 + "...")
        assert!(result.len() <= 500);
    }

    #[test]
    fn test_is_running_accessor() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        assert!(!watcher.is_running());
        watcher.is_running.store(true, Ordering::SeqCst);
        assert!(watcher.is_running());
        watcher.stop();
        assert!(!watcher.is_running());
    }

    #[test]
    fn test_active_count_accessor() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        assert_eq!(watcher.active_count(), 0);
        watcher.active_processing.fetch_add(3, Ordering::SeqCst);
        assert_eq!(watcher.active_count(), 3);
        watcher.active_processing.fetch_sub(1, Ordering::SeqCst);
        assert_eq!(watcher.active_count(), 2);
    }

    #[tokio::test]
    async fn test_active_processing_for_source_empty() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        assert_eq!(watcher.active_processing_for_source("test"), 0);
    }

    #[tokio::test]
    async fn test_active_processing_for_source_counts_only_matching() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        {
            let mut processing = watcher.lock_processing();
            processing.insert("source_a:issue-1".to_string());
            processing.insert("source_a:issue-2".to_string());
            processing.insert("source_b:issue-3".to_string());
            processing.insert("source_a:issue-4".to_string());
        }

        assert_eq!(watcher.active_processing_for_source("source_a"), 3);
        assert_eq!(watcher.active_processing_for_source("source_b"), 1);
        assert_eq!(watcher.active_processing_for_source("source_c"), 0);
    }

    #[tokio::test]
    async fn test_active_processing_for_source_prefix_mismatch() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        {
            let mut processing = watcher.lock_processing();
            // "test_source:" should NOT match "test:" prefix
            processing.insert("test_source:issue-1".to_string());
        }

        assert_eq!(watcher.active_processing_for_source("test"), 0);
        assert_eq!(watcher.active_processing_for_source("test_source"), 1);
    }

    #[tokio::test]
    async fn test_refresh_repos_no_inferrer_returns_zero() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let result = watcher.refresh_repos().await.unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn test_sync_repos_to_db_no_inferrer_returns_zero() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let result = watcher.sync_repos_to_db(true).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn test_sync_repos_to_db_no_inferrer_returns_zero_basic() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let result = watcher.sync_repos_to_db(false).unwrap();
        assert_eq!(result, 0);
    }

    #[tokio::test]
    async fn test_check_reviews_no_watcher_returns_ok() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);
        assert!(watcher.review_watcher.is_none());

        let result = watcher.check_reviews().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_check_pr_merges_no_github_client() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, false);
        assert!(watcher.github_client.is_none());

        // Should succeed and record zero-value metrics
        let result = watcher.check_pr_merges_and_cascade().await;
        assert!(result.is_ok());

        // Verify lifecycle metrics were still recorded
        let checks = tracker.get_metrics("pr_status_checks", None, 10).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].metric_value, 0.0);
    }

    #[tokio::test]
    async fn test_run_housekeeping_cycle_dry_run_skips_retries_and_cascades() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, true);

        let result = watcher.run_housekeeping_cycle().await;
        assert!(result.is_ok());

        // In dry-run mode, no retries or cascade metrics should be recorded
        let retries_found = tracker
            .get_metrics("ready_retries_found", None, 10)
            .unwrap();
        assert!(
            retries_found.is_empty(),
            "dry_run should skip process_ready_retries"
        );
    }

    #[tokio::test]
    async fn test_run_housekeeping_cycle_records_metrics() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, false);
        watcher.is_running.store(true, Ordering::SeqCst);

        let result = watcher.run_housekeeping_cycle().await;
        assert!(result.is_ok());

        // Verify housekeeping metrics
        let duration = tracker
            .get_metrics("housekeeping_cycle_duration_secs", None, 10)
            .unwrap();
        assert_eq!(duration.len(), 1);
        assert!(duration[0].metric_value >= 0.0);

        let active = tracker.get_metrics("active_processing", None, 10).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].metric_value, 0.0);
    }

    #[tokio::test]
    async fn test_check_and_auto_close_prs_no_pending() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let result = watcher.check_and_auto_close_prs().await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_check_and_auto_close_prs_no_matching_source() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Record a PR for a source that does NOT exist in sources list
        tracker
            .record_attempt("nonexistent_source", "1", "NE-1")
            .unwrap();
        tracker
            .mark_success(
                "nonexistent_source",
                "1",
                "https://github.com/org/repo/pull/99",
            )
            .unwrap();

        let source = Arc::new(MockSource::new("different_source")) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker, vec![source], false);

        let result = watcher.check_and_auto_close_prs().await.unwrap();
        // No matching source found, so no auto-close
        assert!(result.is_empty());
    }

    #[test]
    fn test_record_source_decision_does_not_panic() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        // Should not panic even with arbitrary values
        watcher.record_source_decision(
            "test_source",
            "test_decision",
            "Test message",
            json!({"key": "value"}),
        );
    }

    #[test]
    fn test_record_issue_decision_does_not_panic() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let issue = test_issue();
        watcher.record_issue_decision(
            &issue,
            "test_decision",
            "Test message for issue",
            json!({"outcome": "success"}),
        );
    }

    #[test]
    fn test_record_error_pattern_does_not_panic() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        // Should not panic
        crate::processing::record_error_pattern(
            &watcher.tracker,
            "linear",
            "ISSUE-42",
            "build failed: exit code 1",
        );
        crate::processing::record_error_pattern(
            &watcher.tracker,
            "sentry",
            "SENTRY-99",
            "timeout after 300s",
        );
        crate::processing::record_error_pattern(&watcher.tracker, "test", "T-1", "");
    }

    #[test]
    fn test_watcher_new_with_tracker() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config: test_config(),
            sources,
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        assert!(!watcher.dry_run);
    }

    #[tokio::test]
    async fn test_stop_and_drain_immediate_when_no_active() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = Arc::new(create_test_watcher(notifier, tracker, sources, false));
        watcher.is_running.store(true, Ordering::SeqCst);

        // Should complete quickly since there's no active processing
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), watcher.stop_and_drain()).await;
        assert!(result.is_ok(), "stop_and_drain timed out");
        assert!(!watcher.is_running());
    }

    #[tokio::test]
    async fn test_stop_and_drain_waits_for_active() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = Arc::new(create_test_watcher(notifier, tracker, sources, false));
        watcher.is_running.store(true, Ordering::SeqCst);
        watcher.in_flight.fetch_add(1, Ordering::SeqCst);

        // Simulate task finishing after a short delay
        let release = Arc::clone(&watcher);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            release.in_flight.fetch_sub(1, Ordering::SeqCst);
            release.slot_available.notify_waiters();
        });

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), watcher.stop_and_drain()).await;
        assert!(result.is_ok(), "stop_and_drain timed out");
        assert!(!watcher.is_running());
        assert_eq!(watcher.in_flight(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_stop_and_drain_rechecks_a_release_that_sends_no_wake_up() {
        let watcher = create_test_watcher(
            Arc::new(MockNotifier::new(true)),
            Arc::new(SqliteTracker::in_memory().unwrap()),
            vec![],
            false,
        );
        watcher.in_flight.store(1, Ordering::SeqCst);

        let release = Arc::clone(&watcher);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            release.in_flight.fetch_sub(1, Ordering::SeqCst);
        });

        let drained =
            tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 2, watcher.stop_and_drain()).await;

        assert_eq!(
            drained,
            Ok(true),
            "a release whose wake-up is missed must still end the drain"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_housekeeping_run_holds_the_drain_without_counting_as_active_processing() {
        let watcher = create_test_watcher(
            Arc::new(MockNotifier::new(true)),
            Arc::new(SqliteTracker::in_memory().unwrap()),
            vec![],
            false,
        );
        let claim = watcher.claim_run();

        assert_eq!(
            watcher.active_count(),
            0,
            "a housekeeping run processes no issue, so it must not count as active processing"
        );
        assert_eq!(
            watcher.in_flight(),
            1,
            "a housekeeping run must count as a run in flight"
        );

        let drain = watcher.stop_and_drain();
        tokio::pin!(drain);
        let while_claimed = tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 3, drain.as_mut()).await;

        assert!(
            while_claimed.is_err(),
            "the drain must wait for the housekeeping run"
        );

        drop(claim);

        assert!(
            drain.await,
            "the drain must end once the housekeeping run finishes"
        );
    }

    #[test]
    fn test_group_review_feedback_empty_events() {
        let events: Vec<claudear_integrations::scm::ReviewEvent> = vec![];
        let grouped = Watcher::group_review_feedback_by_pr(events);
        assert!(grouped.is_empty());
    }

    #[test]
    fn test_group_review_feedback_only_non_actionable() {
        // CommentsAdded with empty comments does not require action
        let events = vec![claudear_integrations::scm::ReviewEvent::CommentsAdded {
            pr_url: "https://github.com/org/repo/pull/1".to_string(),
            repo: "org/repo".to_string(),
            pr_number: 1,
            comments: vec![],
        }];

        let grouped = Watcher::group_review_feedback_by_pr(events);
        assert!(grouped.is_empty());
    }

    #[test]
    fn test_group_review_feedback_multiple_prs() {
        let make_review =
            |id: i64, state: &str, body: &str| claudear_integrations::scm::CodeReview {
                id,
                state: state.to_string(),
                body: Some(body.to_string()),
                user: claudear_integrations::scm::ReviewUser {
                    id,
                    login: format!("user{}", id),
                    user_type: Some("User".to_string()),
                },
                submitted_at: Some("2024-01-01T00:00:00Z".to_string()),
                html_url: None,
            };

        let events = vec![
            claudear_integrations::scm::ReviewEvent::ReviewSubmitted {
                pr_url: "https://github.com/org/repo/pull/1".to_string(),
                repo: "org/repo".to_string(),
                pr_number: 1,
                review: make_review(1, "CHANGES_REQUESTED", "fix the bug"),
                inline_comments: vec![],
            },
            claudear_integrations::scm::ReviewEvent::ReviewSubmitted {
                pr_url: "https://github.com/org/repo/pull/2".to_string(),
                repo: "org/repo".to_string(),
                pr_number: 2,
                review: make_review(2, "CHANGES_REQUESTED", "needs tests"),
                inline_comments: vec![],
            },
        ];

        let grouped = Watcher::group_review_feedback_by_pr(events);
        assert_eq!(grouped.len(), 2);
        // Verify order is preserved
        assert_eq!(grouped[0].0, "https://github.com/org/repo/pull/1");
        assert_eq!(grouped[1].0, "https://github.com/org/repo/pull/2");
        assert_eq!(grouped[0].2, 1); // 1 review for PR 1
        assert_eq!(grouped[1].2, 1); // 1 review for PR 2
    }

    #[test]
    fn test_is_terminal_all_statuses() {
        let non_terminal = [
            FixAttemptStatus::Pending,
            FixAttemptStatus::Success,
            FixAttemptStatus::Failed,
        ];
        let terminal = [
            FixAttemptStatus::Merged,
            FixAttemptStatus::Closed,
            FixAttemptStatus::CannotFix,
            FixAttemptStatus::Declined,
        ];

        for status in non_terminal {
            assert!(
                !Watcher::is_terminal_attempt_status(status),
                "{:?} should NOT be terminal",
                status
            );
        }
        for status in terminal {
            assert!(
                Watcher::is_terminal_attempt_status(status),
                "{:?} SHOULD be terminal",
                status
            );
        }
    }

    #[test]
    fn test_sort_by_priority_all_same_match_priority_different_issue_priority() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let mut issues = vec![
            (
                test_issue_with_priority("1", IssuePriority::None),
                MatchResult::matched("Same", MatchPriority::Normal),
            ),
            (
                test_issue_with_priority("2", IssuePriority::Critical),
                MatchResult::matched("Same", MatchPriority::Normal),
            ),
            (
                test_issue_with_priority("3", IssuePriority::High),
                MatchResult::matched("Same", MatchPriority::Normal),
            ),
            (
                test_issue_with_priority("4", IssuePriority::Low),
                MatchResult::matched("Same", MatchPriority::Normal),
            ),
            (
                test_issue_with_priority("5", IssuePriority::Medium),
                MatchResult::matched("Same", MatchPriority::Normal),
            ),
        ];

        watcher.sort_by_priority(&mut issues);

        assert_eq!(issues[0].0.priority, IssuePriority::Critical);
        assert_eq!(issues[1].0.priority, IssuePriority::High);
        assert_eq!(issues[2].0.priority, IssuePriority::Medium);
        assert_eq!(issues[3].0.priority, IssuePriority::Low);
        assert_eq!(issues[4].0.priority, IssuePriority::None);
    }

    #[test]
    fn test_sort_by_priority_match_priority_takes_precedence() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        // Issue with Low match priority but Critical issue priority
        // should come AFTER issue with Urgent match priority but None issue priority
        let mut issues = vec![
            (
                test_issue_with_priority("1", IssuePriority::Critical),
                MatchResult::matched("Low match", MatchPriority::Low),
            ),
            (
                test_issue_with_priority("2", IssuePriority::None),
                MatchResult::matched("Urgent match", MatchPriority::Urgent),
            ),
        ];

        watcher.sort_by_priority(&mut issues);

        assert_eq!(issues[0].1.priority, MatchPriority::Urgent);
        assert_eq!(issues[1].1.priority, MatchPriority::Low);
    }

    #[test]
    fn test_sort_by_priority_stability_for_equal_items() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let mut issues = vec![
            (
                {
                    let mut i = test_issue();
                    i.id = "first".to_string();
                    i
                },
                MatchResult::matched("Same", MatchPriority::Normal),
            ),
            (
                {
                    let mut i = test_issue();
                    i.id = "second".to_string();
                    i
                },
                MatchResult::matched("Same", MatchPriority::Normal),
            ),
        ];

        watcher.sort_by_priority(&mut issues);

        // Both have same priority, so the sort should be stable (order preserved)
        assert_eq!(issues[0].0.id, "first");
        assert_eq!(issues[1].0.id, "second");
    }

    #[test]
    fn test_enhance_prompt_no_repo_returns_base() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let base = "Fix the bug in module X";
        let issue = test_issue();
        let result = crate::processing::enhance_prompt_with_learning(
            &watcher.config,
            &watcher.tracker,
            base,
            &issue,
            None,
        );
        assert_eq!(result, base);
    }

    #[test]
    fn test_enhance_prompt_with_repo_no_learning_enabled() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        // Default learning config has everything disabled
        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let base = "Fix the bug in module X";
        let issue = test_issue();
        let result = crate::processing::enhance_prompt_with_learning(
            &watcher.config,
            &watcher.tracker,
            base,
            &issue,
            Some("my-repo"),
        );
        // With no learning enabled and no data, should return base prompt
        assert_eq!(result, base);
    }

    #[tokio::test]
    async fn test_processing_set_concurrent_insertions() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = Arc::new(create_test_watcher(notifier, tracker, sources, false));

        // Concurrently insert 100 items
        let mut handles = vec![];
        for i in 0..100 {
            let w = Arc::clone(&watcher);
            handles.push(tokio::spawn(async move {
                w.lock_processing().insert(format!("test:{}", i));
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(watcher.lock_processing().len(), 100);
    }

    #[tokio::test]
    async fn test_processing_set_insert_and_remove_same_key() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        {
            let mut processing = watcher.lock_processing();
            processing.insert("test:123".to_string());
            assert!(processing.contains("test:123"));
            processing.remove("test:123");
            assert!(!processing.contains("test:123"));
        }

        // Re-insert should work
        watcher.lock_processing().insert("test:123".to_string());

        assert!(watcher.lock_processing().contains("test:123"));
    }

    #[test]
    fn test_watcher_options_config_propagation() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let mut config = test_config();
        config.max_issues_per_cycle = 42;
        config.max_concurrent = 7;
        config.processing_delay_ms = 1500;
        config.poll_interval_ms = 30000;
        config.agent.timeout_secs = 999;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        assert_eq!(watcher.config.max_issues_per_cycle, 42);
        assert_eq!(watcher.config.max_concurrent, 7);
        assert_eq!(watcher.config.processing_delay_ms, 1500);
        assert_eq!(watcher.config.poll_interval_ms, 30000);
        assert_eq!(watcher.config.agent.timeout_secs, 999);
    }

    #[test]
    fn test_watcher_new_multiple_sources() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let sources: Vec<Arc<dyn IssueSource>> = vec![
            Arc::new(MockSource::new("source_a")),
            Arc::new(MockSource::new("source_b")),
            Arc::new(MockSource::new("source_c")),
        ];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        assert_eq!(watcher.sources.len(), 3);
        assert_eq!(watcher.sources[0].name(), "source_a");
        assert_eq!(watcher.sources[1].name(), "source_b");
        assert_eq!(watcher.sources[2].name(), "source_c");
    }

    #[tokio::test]
    async fn test_trigger_cascade_no_relationships_returns_ok() {
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);
        assert!(watcher.relationships.is_none());

        let attempt = FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: Some("https://github.com/org/repo/pull/1".to_string()),
            scm_repo: Some("org/repo".to_string()),
            scm_pr_number: Some(1),
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        let result = watcher
            .trigger_cascade(
                &attempt,
                "https://github.com/org/repo/pull/1",
                claudear_config::config::CascadeTrigger::Merge,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_trigger_cascade_disabled_returns_ok() {
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let mut config = test_config();
        config.cascade.enabled = false;

        let mut relationships = RepoRelationships::new();
        relationships
            .add_dependency(
                "upstream",
                "downstream",
                claudear_analysis::repo::DependencyType::Npm,
                None,
            )
            .unwrap();

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources,
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: Some(relationships),
            github_client: None,
            scm_provider: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let attempt = FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: Some("https://github.com/org/upstream/pull/1".to_string()),
            scm_repo: Some("org/upstream".to_string()),
            scm_pr_number: Some(1),
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        // Even with relationships, cascade disabled returns Ok
        let result = watcher
            .trigger_cascade(
                &attempt,
                "https://github.com/org/upstream/pull/1",
                claudear_config::config::CascadeTrigger::Merge,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_trigger_cascade_no_scm_repo_returns_ok() {
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let mut config = test_config();
        config.cascade.enabled = true;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources,
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: Some(RepoRelationships::new()),
            github_client: None,
            scm_provider: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let attempt = FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: None,
            scm_repo: None, // No scm_repo
            scm_pr_number: None,
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        let result = watcher
            .trigger_cascade(&attempt, "", claudear_config::config::CascadeTrigger::Merge)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_trigger_cascade_no_pr_number_returns_ok() {
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let mut config = test_config();
        config.cascade.enabled = true;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources,
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: Some(RepoRelationships::new()),
            github_client: None,
            scm_provider: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let attempt = FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: Some("https://github.com/org/repo/pull/1".to_string()),
            scm_repo: Some("org/repo".to_string()),
            scm_pr_number: None, // No PR number
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        let result = watcher
            .trigger_cascade(
                &attempt,
                "https://github.com/org/repo/pull/1",
                claudear_config::config::CascadeTrigger::Merge,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_trigger_cascade_no_dependants_returns_ok() {
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let mut config = test_config();
        config.cascade.enabled = true;

        // Empty relationships (no dependants)
        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources,
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: Some(RepoRelationships::new()),
            github_client: None,
            scm_provider: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let attempt = FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: Some("https://github.com/org/my-lib/pull/1".to_string()),
            scm_repo: Some("org/my-lib".to_string()),
            scm_pr_number: Some(1),
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        let result = watcher
            .trigger_cascade(
                &attempt,
                "https://github.com/org/my-lib/pull/1",
                claudear_config::config::CascadeTrigger::Merge,
            )
            .await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_get_cascade_depth_root() {
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let attempt = FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Pending,
            error_message: None,
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        assert_eq!(watcher.get_cascade_depth(&attempt), 0);
    }

    #[test]
    fn test_get_cascade_depth_with_missing_parent() {
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        // Trait default returns None for parent lookups
        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let attempt = FixAttempt {
            id: 2,
            issue_id: "ISSUE-2".to_string(),
            short_id: "ISSUE-2".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Pending,
            error_message: None,
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: Some(999), // Parent doesn't exist
            cascade_repo: None,
        };

        // Should return 1 for the first hop, then break because parent is not found
        assert_eq!(watcher.get_cascade_depth(&attempt), 1);
    }

    #[tokio::test]
    async fn test_poll_source_skips_inflight_issues() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![
            Issue::new(
                "inflight-1",
                "T-IF1",
                "In-flight issue",
                "http://example.com/if1",
                "test",
            ),
            Issue::new(
                "new-1",
                "T-NEW1",
                "New issue",
                "http://example.com/new1",
                "test",
            ),
        ];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;
        let sources = vec![source.clone()];

        let watcher = create_test_watcher(notifier, tracker.clone(), sources, true);

        // Mark one issue as in-flight
        watcher
            .lock_processing()
            .insert("test:inflight-1".to_string());

        watcher.poll_source(&source).await.unwrap();

        // Only the non-inflight issue should be matched
        let matched = tracker.get_metrics("issues_matched", None, 10).unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].metric_value, 1.0); // Only new-1 matched
    }

    #[tokio::test]
    async fn test_poll_dry_run_does_not_record_retries() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Create a failed attempt that would be retried
        tracker
            .record_attempt("mock", "retry-1", "MOCK-R1")
            .unwrap();
        tracker
            .mark_failed("mock", "retry-1", "initial failure")
            .unwrap();

        let source = Arc::new(MockSource::new("mock")) as Arc<dyn IssueSource>;
        let mut config = test_config();
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: true,
            llm_engine: None,
        }));

        watcher.poll().await.unwrap();

        // In dry-run mode, retries should not be processed
        let retries = tracker
            .get_metrics("ready_retries_found", None, 10)
            .unwrap();
        assert!(retries.is_empty(), "dry_run should skip retry processing");
    }

    #[tokio::test]
    async fn test_mock_source_fetch_issues_returns_all() {
        let issues = vec![
            Issue::new("1", "T-1", "Issue 1", "http://example.com/1", "test"),
            Issue::new("2", "T-2", "Issue 2", "http://example.com/2", "test"),
            Issue::new("3", "T-3", "Issue 3", "http://example.com/3", "test"),
        ];
        let source = MockSource::with_issues("test", issues);
        let fetched = source.fetch_issues().await.unwrap();
        assert_eq!(fetched.len(), 3);
    }

    #[test]
    fn test_mock_source_display_name() {
        let source = MockSource::new("my_source");
        assert_eq!(source.display_name(), "my_source");
    }

    #[tokio::test]
    async fn test_mock_source_get_issue_status() {
        let issues = vec![Issue::new("1", "T-1", "Test", "http://example.com", "test")];
        let source = MockSource::with_issues("test", issues);
        let status = source.get_issue_status("1").await.unwrap();
        assert!(status.contains("Open"));
        assert_eq!(source.issue_status_call_count(), 1);
    }

    #[test]
    fn test_mock_source_with_priority() {
        let source = MockSource::with_priority("test", vec![], MatchPriority::Urgent);
        let issue = test_issue();
        let result = source.matches_criteria(&issue);
        assert!(result.matches);
        assert_eq!(result.priority, MatchPriority::Urgent);
    }

    #[tokio::test]
    async fn test_mock_notifier_all_methods_increment_count() {
        let notifier = MockNotifier::new(true);
        let issue = test_issue();

        notifier.notify_start(&issue).await.unwrap();
        assert_eq!(notifier.get_call_count(), 1);

        notifier
            .notify_success(&issue, "http://pr.url")
            .await
            .unwrap();
        assert_eq!(notifier.get_call_count(), 2);

        notifier.notify_completed(&issue).await.unwrap();
        assert_eq!(notifier.get_call_count(), 3);

        notifier.notify_failed(&issue, "error msg").await.unwrap();
        assert_eq!(notifier.get_call_count(), 4);

        notifier.notify_status("status msg").await.unwrap();
        assert_eq!(notifier.get_call_count(), 5);

        notifier
            .notify_urgent_issues(std::slice::from_ref(&issue))
            .await
            .unwrap();
        assert_eq!(notifier.get_call_count(), 6);

        notifier
            .notify_merged(&issue, "http://pr.url")
            .await
            .unwrap();
        assert_eq!(notifier.get_call_count(), 7);
    }

    #[tokio::test]
    async fn test_mock_notifier_with_urgent_failure_fails() {
        let notifier = MockNotifier::with_urgent_failure(true);
        let issue = test_issue();

        let result = notifier.notify_urgent_issues(&[issue]).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_seed_result_by_source_tracking() {
        let mut result = SeedResult {
            total: 10,
            ..Default::default()
        };
        result.by_source.insert("linear".to_string(), 6);
        result.by_source.insert("sentry".to_string(), 4);

        assert_eq!(result.total, 10);
        assert_eq!(*result.by_source.get("linear").unwrap(), 6);
        assert_eq!(*result.by_source.get("sentry").unwrap(), 4);
        assert!(!result.by_source.contains_key("jira"));
    }

    #[test]
    fn test_watcher_new_initializes_all_fields() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config: test_config(),
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        // Verify initial state
        assert!(!watcher.is_running());
        assert_eq!(watcher.active_count(), 0);
        assert!(!watcher.dry_run);
        assert!(watcher.inferrer.is_none());
        assert!(watcher.embedding_client.is_none());
        assert!(watcher.review_watcher.is_none());
        assert!(watcher.issue_embedding_service.is_none());
        assert!(watcher.relationships.is_none());
        assert!(watcher.github_client.is_none());
        assert!(watcher.sources.is_empty());
    }

    #[tokio::test]
    async fn test_trigger_issue_with_feedback_unknown_source() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        let result = watcher
            .trigger_issue_with_feedback(
                "nonexistent",
                "123",
                Some("feedback".to_string()),
                None,
                None,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown source"));
    }

    #[tokio::test]
    async fn test_trigger_issue_with_feedback_issue_not_found() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Source exists but has no issues
        let source = Arc::new(MockSource::new("mock")) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker, vec![source], false);

        let result = watcher
            .trigger_issue_with_feedback("mock", "nonexistent", None, None, None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_poll_source_respects_per_source_max_issues() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues: Vec<Issue> = (1..=10)
            .map(|i| {
                Issue::new(
                    format!("{}", i),
                    format!("T-{}", i),
                    format!("Issue {}", i),
                    format!("http://example.com/{}", i),
                    "test",
                )
            })
            .collect();
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.max_issues_per_cycle = 3;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source.clone()],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: true,
            llm_engine: None,
        }));

        watcher.poll_source(&source).await.unwrap();

        let queued = tracker.get_metrics("issues_queued", None, 10).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].metric_value, 3.0); // Limited to 3
    }

    #[tokio::test]
    async fn test_process_issue_skips_issue_already_processing() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![Issue::new(
            "1",
            "T-1",
            "Test Issue",
            "http://example.com/1",
            "mock",
        )];
        let source = Arc::new(MockSource::with_issues("mock", issues)) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker, vec![source.clone()], false);
        watcher.lock_processing().insert("mock:1".to_string());

        let issue = Issue::new("1", "T-1", "Test Issue", "http://example.com/1", "mock");
        let match_result = MatchResult::matched("Test", MatchPriority::Normal);

        let outcome = watcher
            .process_issue(source, issue, match_result, None, None, None, None)
            .await;
        assert_eq!(
            outcome,
            IssueRun::Busy,
            "process_issue should report an issue already in flight as busy"
        );
    }

    const CUSTOMER_REPLY: &str = "Thanks for reaching out, we are looking into this for you.";
    const SCRIPTED_QA_PROVIDER: &str = "scripted-qa-agent";

    /// How [`ScriptedQaAgent`] answers a QA question.
    enum QaAnswer {
        Report(String),
        Crash,
        Fail(String),
        Panic,
        /// Panics on the first question, then answers with the report.
        PanicOnce(String),
        /// Waits at the gate, then answers with [`CUSTOMER_REPLY`].
        Gated(Arc<Gate>),
    }

    /// Agent that counts every invocation, answers QA as its [`QaAnswer`]
    /// scripts, and never supports the fix pipeline.
    struct ScriptedQaAgent {
        calls: Arc<AtomicUsize>,
        answer: QaAnswer,
    }

    impl ScriptedQaAgent {
        fn record_call(&self) {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    #[async_trait]
    impl AgentRunner for ScriptedQaAgent {
        fn name(&self) -> &str {
            SCRIPTED_QA_PROVIDER
        }
        fn capabilities(&self) -> claudear_integrations::runner::ProviderCapabilities {
            claudear_integrations::runner::ProviderCapabilities::default()
        }
        fn build_prompt_for_issue(
            &self,
            _issue: &Issue,
            _context: &str,
            _project_dir: &std::path::Path,
        ) -> String {
            String::new()
        }
        async fn execute_with_attempt(
            &self,
            _prompt: &str,
            _issue: Option<&Issue>,
            _attempt_id: Option<i64>,
            _project_dir: &std::path::Path,
        ) -> Result<claudear_core::types::AgentResult> {
            self.record_call();
            Err(claudear_core::error::Error::runner(
                "fix pipeline must never run for this agent",
            ))
        }
        async fn answer_question(
            &self,
            _issue: &Issue,
            _context: &str,
            _project_dir: &std::path::Path,
        ) -> Result<String> {
            self.record_call();
            match &self.answer {
                QaAnswer::Report(report) => Ok(report.clone()),
                QaAnswer::Crash => {
                    Err(claudear_core::error::Error::runner("live QA probe crashed"))
                }
                QaAnswer::Fail(message) => Err(claudear_core::error::Error::runner(message)),
                QaAnswer::Panic => panic!("live QA probe panicked"),
                QaAnswer::PanicOnce(report) => {
                    if self.calls.load(AtomicOrdering::SeqCst) == 1 {
                        panic!("first QA probe panicked");
                    }
                    Ok(report.clone())
                }
                QaAnswer::Gated(gate) => {
                    gate.pass().await;
                    Ok(CUSTOMER_REPLY.to_string())
                }
            }
        }
        async fn verify_issue(
            &self,
            _issue: &Issue,
            _context: &str,
            _project_dir: &std::path::Path,
        ) -> Result<claudear_core::types::VerifyResult> {
            self.record_call();
            Err(claudear_core::error::Error::runner(
                "verify must never run for this agent",
            ))
        }
        async fn generate_reply(
            &self,
            _issue: &Issue,
            _context: &str,
            _guideline: Option<&str>,
            _kind: ReplyKind,
            _project_dir: &std::path::Path,
        ) -> Result<String> {
            self.record_call();
            Ok(CUSTOMER_REPLY.to_string())
        }
    }

    const DEPLOY_QA_TRACK: &str = "cloud";
    const DEPLOY_QA_REPO: &str = "appwrite-labs/cloud";

    fn deploy_qa_issue(tip: &DeployQaTip) -> Issue {
        Issue::new(
            tip.issue_id.clone(),
            tip.tag.clone(),
            format!("Deploy QA {}", tip.tag),
            format!("https://github.com/{}/releases/tag/{}", tip.repo, tip.tag),
            DEPLOY_QA_SOURCE,
        )
    }

    fn deploy_qa_report(results: &[&str], verdict: &str) -> String {
        let mut lines = results.to_vec();
        let footer = format!("{VERDICT_PREFIX} {verdict}");
        lines.push(&footer);
        lines.join("\n")
    }

    fn watcher_with_agent(
        config: Config,
        source: Arc<dyn IssueSource>,
        tracker: Arc<SqliteTracker>,
        agent: Arc<dyn AgentRunner>,
    ) -> Arc<Watcher> {
        Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source],
            notifier: Arc::new(MockNotifier::new(true)),
            tracker,
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
            agent,
        }))
    }

    struct DeployQaHarness {
        watcher: Arc<Watcher>,
        source: Arc<dyn IssueSource>,
        tracker: Arc<SqliteTracker>,
        tip: DeployQaTip,
        agent_calls: Arc<AtomicUsize>,
        /// The watcher's `workspace`, where live-QA runs create their private
        /// directories; removed when the harness drops.
        _workspace: tempfile::TempDir,
    }

    impl DeployQaHarness {
        /// A mock `deploy_qa` source whose QA agent crashes.
        fn new(config: Config) -> Self {
            Self::answering(config, QaAnswer::Crash)
        }

        /// A mock `deploy_qa` source whose QA agent answers as `answer`.
        fn answering(config: Config, answer: QaAnswer) -> Self {
            Self::build(config, answer, |_, tip| {
                Arc::new(MockSource::with_issues(
                    DEPLOY_QA_SOURCE,
                    vec![deploy_qa_issue(tip)],
                ))
            })
        }

        /// The real [`DeployQaSource`], whose QA agent answers with `report`.
        fn with_deploy_qa_source(report: String) -> Self {
            Self::with_deploy_qa_config(test_config(), QaAnswer::Report(report))
        }

        /// A running watcher over the real [`DeployQaSource`] whose QA agent
        /// verifies every tip, ready for the `[deploy_qa]` poller to dispatch.
        fn dispatching(config: Config) -> Self {
            Self::dispatching_with(
                config,
                QaAnswer::Report(deploy_qa_report(
                    &["- #1 x LIVE PASS"],
                    VERDICT_ALL_VERIFIED,
                )),
            )
        }

        /// A running watcher over the real [`DeployQaSource`] whose QA agent
        /// answers as `answer`, ready for the `[deploy_qa]` poller to dispatch.
        fn dispatching_with(config: Config, answer: QaAnswer) -> Self {
            let harness = Self::with_deploy_qa_config(config, answer);
            harness.watcher.set_running(true);
            harness
        }

        fn with_deploy_qa_config(config: Config, answer: QaAnswer) -> Self {
            Self::build(config, answer, |tracker, _| {
                let config = DeployQaConfig {
                    tracks: vec![DeployQaTrackConfig {
                        name: DEPLOY_QA_TRACK.to_string(),
                        repo: DEPLOY_QA_REPO.to_string(),
                        tag_filter: Default::default(),
                    }],
                    ..DeployQaConfig::default()
                };
                Arc::new(
                    DeployQaSource::new(config, tracker, None)
                        .expect("the bundled playbook should load"),
                )
            })
        }

        fn build(
            mut config: Config,
            answer: QaAnswer,
            source: impl FnOnce(Arc<dyn FixAttemptTracker>, &DeployQaTip) -> Arc<dyn IssueSource>,
        ) -> Self {
            let workspace = tempfile::tempdir().unwrap();
            config.workspace = workspace.path().to_path_buf();
            let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
            let tip = tracker
                .upsert_deploy_qa_tip(&DeployQaTip::new(DEPLOY_QA_TRACK, DEPLOY_QA_REPO, "1.2.3"))
                .unwrap();
            let source = source(tracker.clone(), &tip);
            let agent_calls = Arc::new(AtomicUsize::new(0));
            let watcher = watcher_with_agent(
                config,
                source.clone(),
                tracker.clone(),
                Arc::new(ScriptedQaAgent {
                    calls: agent_calls.clone(),
                    answer,
                }),
            );
            Self {
                watcher,
                source,
                tracker,
                tip,
                agent_calls,
                _workspace: workspace,
            }
        }

        /// A watcher over the same database, source and QA agent that shares
        /// no in-memory state with [`Self::watcher`], standing in for a second
        /// daemon process.
        fn second_process(&self) -> Arc<Watcher> {
            watcher_with_agent(
                self.watcher.config.clone(),
                self.source.clone(),
                self.tracker.clone(),
                Arc::clone(&self.watcher.agent),
            )
        }

        fn issue(&self) -> Issue {
            deploy_qa_issue(&self.tip)
        }

        fn processing_key(&self) -> String {
            format!("{DEPLOY_QA_SOURCE}:{}", self.tip.issue_id)
        }

        /// Load the seeded tip's issue from the source and process it on
        /// `watcher`, returning whether the watcher processed it.
        async fn process_tip_on(&self, watcher: &Watcher) -> bool {
            let issue = self
                .source
                .get_issue(&self.tip.issue_id)
                .await
                .expect("the seeded tip's issue should load");
            let match_result = self.source.matches_criteria(&issue);
            watcher
                .process_issue(
                    self.source.clone(),
                    issue,
                    match_result,
                    None,
                    None,
                    None,
                    None,
                )
                .await
                == IssueRun::Processed
        }

        fn assert_processing_released(&self, watcher: &Watcher) {
            assert!(
                !watcher.lock_processing().contains(&self.processing_key()),
                "the tip must not stay in the watcher's processing set"
            );
            assert_eq!(
                watcher.active_processing.load(Ordering::SeqCst),
                0,
                "the watcher must give back its processing slot"
            );
        }

        fn stored_status(&self) -> DeployQaTipStatus {
            self.status_of(&self.tip)
        }

        fn status_of(&self, tip: &DeployQaTip) -> DeployQaTipStatus {
            self.tracker
                .get_deploy_qa_tip_by_issue_id(&tip.issue_id)
                .unwrap()
                .expect("deploy_qa tip should still be stored")
                .status
        }

        async fn dispatch_pending_tips(&self) -> Vec<tokio::task::JoinHandle<()>> {
            self.watcher
                .dispatch_pending_deploy_qa_tips()
                .await
                .expect("dispatching pending deploy_qa tips should succeed")
        }

        fn agent_calls(&self) -> usize {
            self.agent_calls.load(AtomicOrdering::SeqCst)
        }

        /// Drive the seeded tip through the watcher: list it from the source,
        /// then process it without a routing intent.
        async fn process_pending_tip(&self) {
            let mut pending = self.source.fetch_issues().await.unwrap();
            assert_eq!(pending.len(), 1, "the seeded tip should be pending");
            let issue = pending.remove(0);
            let match_result = self.source.matches_criteria(&issue);
            self.watcher
                .process_issue(
                    self.source.clone(),
                    issue,
                    match_result,
                    None,
                    None,
                    None,
                    None,
                )
                .await;
        }

        async fn assert_manual_action_refused(&self, action: ActionKind) {
            use crate::processing::ProcessingOutcome;

            let issue = self.issue();
            self.tracker
                .record_attempt(DEPLOY_QA_SOURCE, &issue.id, &issue.short_id)
                .unwrap();

            let outcome = self
                .watcher
                .run_action(action, DEPLOY_QA_SOURCE, &issue.id)
                .await
                .unwrap();

            match outcome {
                ProcessingOutcome::Failed { error } => assert!(
                    error.contains(&action.to_string()),
                    "the refusal should name the {action} action: {error}"
                ),
                _ => panic!("{action} on a deploy_qa issue must be refused"),
            }
            assert_eq!(
                self.agent_calls(),
                0,
                "a refused {action} must not run the agent"
            );
            assert_eq!(
                self.stored_status(),
                DeployQaTipStatus::Pending,
                "a refused {action} must not post a comment that is read as a QA verdict"
            );
            let attempt = self
                .tracker
                .get_attempt(DEPLOY_QA_SOURCE, &issue.id)
                .unwrap()
                .expect("attempt should still be recorded");
            assert_eq!(
                attempt.status,
                FixAttemptStatus::Pending,
                "a refused {action} must leave the attempt untouched"
            );
        }
    }

    #[tokio::test]
    async fn test_failed_deploy_qa_attempt_marks_tip_errored() {
        let harness = DeployQaHarness::new(test_config());

        harness
            .watcher
            .process_issue(
                harness.source.clone(),
                harness.issue(),
                MatchResult::matched("deploy_qa pending tip", MatchPriority::High),
                None,
                None,
                Some(Intent::Question),
                None,
            )
            .await;

        assert_eq!(harness.agent_calls(), 1, "QA agent should be asked once");
        assert_eq!(
            harness.stored_status(),
            DeployQaTipStatus::Errored,
            "an attempt that ends without a verdict must release the tip from running"
        );
        assert!(
            !harness
                .tracker
                .track_has_in_flight_deploy_qa(DEPLOY_QA_TRACK)
                .unwrap(),
            "an errored tip must not block the track"
        );
    }

    async fn run_rate_limited_deploy_qa(failure: String) -> (usize, DateTime<Utc>) {
        let mut config = test_config();
        config.agent.default_provider = SCRIPTED_QA_PROVIDER.to_string();
        let harness = DeployQaHarness::answering(config, QaAnswer::Fail(failure));

        for _ in 0..2 {
            harness
                .watcher
                .process_issue(
                    harness.source.clone(),
                    harness.issue(),
                    MatchResult::matched("deploy_qa pending tip", MatchPriority::High),
                    None,
                    None,
                    Some(Intent::Question),
                    None,
                )
                .await;
        }

        let pause_until = harness
            .tracker
            .get_recent_activities(50, None)
            .unwrap()
            .into_iter()
            .find(|activity| activity.activity_type == "watcher_paused")
            .and_then(|activity| activity.metadata)
            .and_then(|metadata| metadata["pause_until"].as_str().map(str::to_string))
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .expect("the rate limit should be logged as a pause")
            .with_timezone(&Utc);
        (harness.agent_calls(), pause_until)
    }

    async fn assert_deploy_qa_deferred_until(failure: String, reset: DateTime<Utc>) {
        let (agent_calls, pause_until) = run_rate_limited_deploy_qa(failure).await;
        assert_eq!(
            agent_calls, 1,
            "a rate-limited provider must not be asked again before its reset"
        );
        assert!(
            pause_until >= reset,
            "the pause must last until the limit resets at {reset}, got {pause_until}"
        );
    }

    async fn assert_deploy_qa_briefly_deferred(failure: String) {
        let (agent_calls, pause_until) = run_rate_limited_deploy_qa(failure).await;
        assert_eq!(
            agent_calls, 1,
            "a rate-limited provider must stay paused when its reset cannot be trusted"
        );
        assert!(
            pause_until < Utc::now() + chrono::Duration::hours(1),
            "an untrusted reset must not hold the provider past a short pause, got {pause_until}"
        );
    }

    fn usage_limit_failure(reset: DateTime<Utc>) -> String {
        format!("Claude AI usage limit reached|{}", reset.timestamp())
    }

    fn rate_limit_event_failure(resets_at: serde_json::Value) -> String {
        format!(
            "Claude rate limit hit: {}",
            json!({
                "type": "rate_limit_event",
                "rate_limit_info": { "status": "rejected", "resetsAt": resets_at },
            })
        )
    }

    #[tokio::test]
    async fn test_deploy_qa_usage_limit_defers_qa_until_reset() {
        let reset = Utc::now() + chrono::Duration::hours(3);
        assert_deploy_qa_deferred_until(usage_limit_failure(reset), reset).await;
    }

    #[tokio::test]
    async fn test_deploy_qa_rate_limit_event_with_epoch_reset_defers_qa_until_reset() {
        let reset = Utc::now() + chrono::Duration::hours(3);
        assert_deploy_qa_deferred_until(rate_limit_event_failure(json!(reset.timestamp())), reset)
            .await;
    }

    #[tokio::test]
    async fn test_deploy_qa_rate_limit_event_with_rfc3339_reset_defers_qa_until_reset() {
        let reset = Utc::now() + chrono::Duration::hours(3);
        assert_deploy_qa_deferred_until(rate_limit_event_failure(json!(reset.to_rfc3339())), reset)
            .await;
    }

    #[tokio::test]
    async fn test_deploy_qa_rate_limit_event_falls_back_to_top_level_reset() {
        let reset = Utc::now() + chrono::Duration::hours(3);
        let failure = format!(
            "Claude rate limit hit: {}",
            json!({
                "type": "rate_limit_event",
                "rate_limit_info": { "status": "rejected", "resetsAt": "invalid" },
                "resetsAt": reset.to_rfc3339(),
            })
        );
        assert_deploy_qa_deferred_until(failure, reset).await;
    }

    #[tokio::test]
    async fn test_deploy_qa_rate_limit_event_with_invalid_reset_defers_qa_briefly() {
        assert_deploy_qa_briefly_deferred(rate_limit_event_failure(json!("not-a-valid-date")))
            .await;
    }

    #[tokio::test]
    async fn test_deploy_qa_rate_limit_event_with_empty_reset_defers_qa_briefly() {
        assert_deploy_qa_briefly_deferred(rate_limit_event_failure(json!(""))).await;
    }

    #[tokio::test]
    async fn test_deploy_qa_rate_limit_without_reset_defers_qa_briefly() {
        assert_deploy_qa_briefly_deferred("Claude rate limit hit: some error".to_string()).await;
    }

    #[tokio::test]
    async fn test_deploy_qa_usage_limit_with_elapsed_reset_still_defers_qa() {
        let reset = Utc::now() - chrono::Duration::hours(1);
        assert_deploy_qa_briefly_deferred(usage_limit_failure(reset)).await;
    }

    #[tokio::test]
    async fn test_deploy_qa_rate_limit_event_with_elapsed_reset_still_defers_qa() {
        let reset = Utc::now() - chrono::Duration::hours(1);
        assert_deploy_qa_briefly_deferred(rate_limit_event_failure(json!(reset.timestamp()))).await;
    }

    #[tokio::test]
    async fn test_deploy_qa_usage_limit_with_implausibly_distant_reset_defers_qa_briefly() {
        let reset = Utc::now() + chrono::Duration::days(30);
        assert_deploy_qa_briefly_deferred(usage_limit_failure(reset)).await;
    }

    #[tokio::test]
    async fn test_deploy_qa_usage_limit_with_millisecond_reset_defers_qa_briefly() {
        let reset = Utc::now() + chrono::Duration::hours(3);
        assert_deploy_qa_briefly_deferred(format!(
            "Claude AI usage limit reached|{}",
            reset.timestamp_millis()
        ))
        .await;
    }

    #[tokio::test]
    async fn test_deploy_qa_usage_limit_without_reset_defers_qa_briefly() {
        assert_deploy_qa_briefly_deferred("Claude AI usage limit reached|".to_string()).await;
    }

    #[tokio::test]
    async fn test_deploy_qa_trigger_forces_question_path() {
        let harness = DeployQaHarness::new(test_config());

        harness
            .watcher
            .trigger_issue(DEPLOY_QA_SOURCE, &harness.tip.issue_id)
            .await
            .unwrap();

        assert_eq!(
            harness.agent_calls(),
            1,
            "a triggered deploy_qa attempt must take the live QA path, not the fix pipeline"
        );
    }

    #[tokio::test]
    async fn test_deploy_qa_skips_approval_gate() {
        let mut config = test_config();
        config.ask.approval_confidence_threshold = Some("high".to_string());
        let harness = DeployQaHarness::new(config);

        harness
            .watcher
            .process_issue(
                harness.source.clone(),
                harness.issue(),
                MatchResult::matched("deploy_qa pending tip", MatchPriority::High),
                None,
                None,
                Some(Intent::Question),
                None,
            )
            .await;

        assert_eq!(
            harness.agent_calls(),
            1,
            "live-QA deploy_qa attempts must not wait on human approval"
        );
        assert_ne!(
            harness.stored_status(),
            DeployQaTipStatus::Running,
            "the tip must not be left running"
        );
    }

    #[tokio::test]
    async fn test_deploy_qa_refuses_manual_resolve() {
        DeployQaHarness::with_deploy_qa_source(String::new())
            .assert_manual_action_refused(ActionKind::Resolve)
            .await;
    }

    #[tokio::test]
    async fn test_deploy_qa_refuses_manual_reply() {
        DeployQaHarness::with_deploy_qa_source(String::new())
            .assert_manual_action_refused(ActionKind::Reply)
            .await;
    }

    #[tokio::test]
    async fn test_deploy_qa_refuses_manual_verify() {
        DeployQaHarness::with_deploy_qa_source(String::new())
            .assert_manual_action_refused(ActionKind::Verify)
            .await;
    }

    #[tokio::test]
    async fn test_deploy_qa_all_verified_report_marks_tip_verified() {
        let harness = DeployQaHarness::with_deploy_qa_source(deploy_qa_report(
            &["- #1 x LIVE PASS", "- #2 y LIVE PASS"],
            VERDICT_ALL_VERIFIED,
        ));

        harness.process_pending_tip().await;

        assert_eq!(harness.agent_calls(), 1, "QA agent should be asked once");
        assert_eq!(
            harness.stored_status(),
            DeployQaTipStatus::Verified,
            "the source must persist the all-verified verdict and the watcher must not clobber it"
        );
        assert!(!harness
            .tracker
            .track_has_in_flight_deploy_qa(DEPLOY_QA_TRACK)
            .unwrap());
    }

    #[tokio::test]
    async fn test_deploy_qa_live_fail_report_marks_tip_failed() {
        let harness = DeployQaHarness::with_deploy_qa_source(deploy_qa_report(
            &["- #1 x LIVE FAIL", "- #2 y LIVE PASS"],
            VERDICT_FAIL,
        ));

        harness.process_pending_tip().await;

        assert_eq!(harness.agent_calls(), 1, "QA agent should be asked once");
        assert_eq!(
            harness.stored_status(),
            DeployQaTipStatus::Failed,
            "the source must persist the fail verdict and the watcher must not clobber it"
        );
        assert!(!harness
            .tracker
            .track_has_in_flight_deploy_qa(DEPLOY_QA_TRACK)
            .unwrap());
    }

    async fn finish_deploy_qa_runs(runs: Vec<tokio::task::JoinHandle<()>>) {
        for run in join_all(runs).await {
            run.expect("a dispatched deploy_qa run should not panic");
        }
    }

    async fn finish_panicking_deploy_qa_runs(runs: Vec<tokio::task::JoinHandle<()>>) {
        for run in join_all(runs).await {
            assert!(
                run.is_err_and(|error| error.is_panic()),
                "a dispatched deploy_qa run should panic with its QA agent"
            );
        }
    }

    #[tokio::test]
    async fn test_dispatched_pending_deploy_qa_tip_is_verified() {
        let harness = DeployQaHarness::dispatching(test_config());

        let runs = harness.dispatch_pending_tips().await;

        assert_eq!(runs.len(), 1, "the pending tip should be dispatched");
        finish_deploy_qa_runs(runs).await;
        assert_eq!(harness.agent_calls(), 1, "QA agent should be asked once");
        assert_eq!(
            harness.stored_status(),
            DeployQaTipStatus::Verified,
            "a dispatched tip must run QA without the watcher polling sources"
        );
        assert!(
            !harness
                .tracker
                .track_has_in_flight_deploy_qa(DEPLOY_QA_TRACK)
                .unwrap(),
            "a verified tip must not block its track"
        );
    }

    #[tokio::test]
    async fn test_poll_leaves_deploy_qa_tips_to_dispatch() {
        let harness = DeployQaHarness::dispatching(test_config());

        harness.watcher.poll().await.unwrap();
        harness.watcher.drain_spawned_tasks().await;

        assert_eq!(
            harness.agent_calls(),
            0,
            "a poll cycle must leave deploy_qa tips to the dispatcher so a tip never runs twice"
        );
        assert_eq!(harness.stored_status(), DeployQaTipStatus::Pending);

        finish_deploy_qa_runs(harness.dispatch_pending_tips().await).await;

        assert_eq!(harness.agent_calls(), 1, "dispatch should run the tip once");
        assert_eq!(harness.stored_status(), DeployQaTipStatus::Verified);
    }

    #[tokio::test]
    async fn test_finished_deploy_qa_tip_is_not_dispatched() {
        let harness = DeployQaHarness::dispatching(test_config());
        harness
            .tracker
            .update_deploy_qa_tip_status(harness.tip.id, DeployQaTipStatus::Verified, None)
            .unwrap();

        let runs = harness.dispatch_pending_tips().await;

        assert!(runs.is_empty(), "only pending tips may be dispatched");
        assert_eq!(
            harness.agent_calls(),
            0,
            "a finished tip must not run QA again"
        );
        assert_eq!(harness.stored_status(), DeployQaTipStatus::Verified);
    }

    #[tokio::test]
    async fn test_dispatched_deploy_qa_tip_finished_elsewhere_is_not_rerun() {
        let harness = DeployQaHarness::dispatching(test_config());
        for status in [
            DeployQaTipStatus::Running,
            DeployQaTipStatus::Verified,
            DeployQaTipStatus::Errored,
        ] {
            harness
                .tracker
                .update_deploy_qa_tip_status(harness.tip.id, status, None)
                .unwrap();

            harness
                .watcher
                .process_deploy_qa_tip(harness.source.clone(), &harness.tip.issue_id)
                .await;

            assert_eq!(
                harness.agent_calls(),
                0,
                "a tip that left pending after dispatch listed it must not run QA again"
            );
            assert_eq!(
                harness.stored_status(),
                status,
                "a skipped {status} tip must keep its status"
            );
        }
    }

    #[tokio::test]
    async fn test_deploy_qa_trigger_skips_tip_that_is_running_or_has_verdict() {
        let harness = DeployQaHarness::with_deploy_qa_source(deploy_qa_report(
            &["- #1 x LIVE FAIL"],
            VERDICT_FAIL,
        ));
        for status in [
            DeployQaTipStatus::Verified,
            DeployQaTipStatus::Unverified,
            DeployQaTipStatus::Failed,
            DeployQaTipStatus::Running,
        ] {
            harness
                .tracker
                .update_deploy_qa_tip_status(harness.tip.id, status, None)
                .unwrap();

            let triggered = harness
                .watcher
                .trigger_issue(DEPLOY_QA_SOURCE, &harness.tip.issue_id)
                .await;

            assert!(
                triggered.is_err(),
                "a trigger for a {status} tip must report that QA did not start"
            );
            assert_eq!(
                harness.agent_calls(),
                0,
                "a {status} tip must not run QA again or post a second report"
            );
            assert_eq!(
                harness.stored_status(),
                status,
                "a skipped {status} tip must keep its status"
            );
            assert!(
                harness
                    .tracker
                    .get_attempt(DEPLOY_QA_SOURCE, &harness.tip.issue_id)
                    .unwrap()
                    .is_none(),
                "a skipped {status} tip must not record an attempt"
            );
        }
    }

    #[tokio::test]
    async fn test_deploy_qa_trigger_reruns_errored_tip() {
        let harness = DeployQaHarness::with_deploy_qa_source(deploy_qa_report(
            &["- #1 x LIVE FAIL"],
            VERDICT_FAIL,
        ));
        harness
            .tracker
            .update_deploy_qa_tip_status(harness.tip.id, DeployQaTipStatus::Errored, None)
            .unwrap();

        harness
            .watcher
            .trigger_issue(DEPLOY_QA_SOURCE, &harness.tip.issue_id)
            .await
            .unwrap();

        assert_eq!(
            harness.agent_calls(),
            1,
            "an errored tip has no verdict, so a retry must run QA once"
        );
        assert_eq!(
            harness.stored_status(),
            DeployQaTipStatus::Failed,
            "the retried run must record its verdict"
        );
    }

    #[tokio::test]
    async fn test_deploy_qa_tip_runs_once_across_daemon_processes() {
        let harness = DeployQaHarness::with_deploy_qa_source(deploy_qa_report(
            &["- #1 x LIVE PASS"],
            VERDICT_ALL_VERIFIED,
        ));
        let second = harness.second_process();
        let pauses = (
            harness.watcher.rate_limit_pause_until.write().await,
            second.rate_limit_pause_until.write().await,
        );
        // Each run reads its tip before checking for a rate-limit pause, so
        // both have read it as pending once they have been polled.
        let release_pauses = async move {
            tokio::task::yield_now().await;
            drop(pauses);
        };

        let (first_ran, second_ran, ()) = tokio::join!(
            harness.process_tip_on(&harness.watcher),
            harness.process_tip_on(&second),
            release_pauses,
        );

        assert_eq!(
            harness.agent_calls(),
            1,
            "daemons that both read the tip as pending must run QA for it once"
        );
        assert_ne!(
            first_ran, second_ran,
            "exactly one daemon may process the tip"
        );
        assert_eq!(
            harness.stored_status(),
            DeployQaTipStatus::Verified,
            "the daemon that claimed the tip must record its verdict"
        );
        harness.assert_processing_released(&harness.watcher);
        harness.assert_processing_released(&second);
    }

    #[tokio::test]
    async fn test_deploy_qa_tip_claimed_by_another_process_is_left_to_it() {
        let harness = DeployQaHarness::with_deploy_qa_source(deploy_qa_report(
            &["- #1 x LIVE PASS"],
            VERDICT_ALL_VERIFIED,
        ));
        let harness = &harness;
        let pauses = harness.watcher.rate_limit_pause_until.write().await;
        let claim_elsewhere = async move {
            tokio::task::yield_now().await;
            harness
                .tracker
                .update_deploy_qa_tip_status(harness.tip.id, DeployQaTipStatus::Running, None)
                .unwrap();
            drop(pauses);
        };

        let (ran, ()) = tokio::join!(harness.process_tip_on(&harness.watcher), claim_elsewhere);

        assert!(!ran, "a tip another process claimed must not be processed");
        assert_eq!(
            harness.agent_calls(),
            0,
            "a tip another process claimed must not run QA again"
        );
        assert_eq!(
            harness.stored_status(),
            DeployQaTipStatus::Running,
            "the tip must stay running for the process that claimed it"
        );
        assert!(
            harness
                .tracker
                .get_attempt(DEPLOY_QA_SOURCE, &harness.tip.issue_id)
                .unwrap()
                .is_none(),
            "a lost claim must not record an attempt"
        );
        harness.assert_processing_released(&harness.watcher);
    }

    #[tokio::test]
    async fn test_retry_of_failed_deploy_qa_attempt_skips_tip_with_verdict() {
        let mut config = test_config();
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;
        config.processing_delay_ms = 0;
        let harness = DeployQaHarness::dispatching(config);
        let issue = harness.issue();
        harness
            .tracker
            .record_attempt(DEPLOY_QA_SOURCE, &issue.id, &issue.short_id)
            .unwrap();
        harness
            .tracker
            .mark_failed(DEPLOY_QA_SOURCE, &issue.id, "answer delivery failed")
            .unwrap();
        harness
            .tracker
            .update_deploy_qa_tip_status(harness.tip.id, DeployQaTipStatus::Verified, None)
            .unwrap();

        harness.watcher.process_ready_retries().await.unwrap();

        assert_eq!(
            harness.agent_calls(),
            0,
            "retrying a failed attempt must not re-run QA for a tip that already has a verdict"
        );
        assert_eq!(harness.stored_status(), DeployQaTipStatus::Verified);
    }

    #[tokio::test]
    async fn test_seed_leaves_deploy_qa_tips_to_dispatch() {
        let harness = DeployQaHarness::dispatching(test_config());

        let seeded = harness.watcher.seed().await.unwrap();

        assert_eq!(
            seeded.total, 0,
            "a pending deploy_qa tip must not be seeded"
        );
        assert!(!seeded.by_source.contains_key(DEPLOY_QA_SOURCE));
        assert!(
            harness
                .tracker
                .get_attempt(DEPLOY_QA_SOURCE, &harness.tip.issue_id)
                .unwrap()
                .is_none(),
            "seeding must not record a failed attempt that the retry manager would re-run"
        );
        assert_eq!(harness.stored_status(), DeployQaTipStatus::Pending);

        finish_deploy_qa_runs(harness.dispatch_pending_tips().await).await;

        assert_eq!(
            harness.agent_calls(),
            1,
            "dispatch should still run the tip once"
        );
        assert_eq!(harness.stored_status(), DeployQaTipStatus::Verified);
    }

    #[tokio::test]
    async fn test_concurrent_deploy_qa_dispatches_run_tip_once() {
        let mut config = test_config();
        config.qa.max_concurrent = 2;
        let harness = DeployQaHarness::dispatching(config);

        let (first, second) = tokio::join!(
            harness.dispatch_pending_tips(),
            harness.dispatch_pending_tips()
        );
        finish_deploy_qa_runs(first.into_iter().chain(second).collect()).await;
        let after_verdict = harness.dispatch_pending_tips().await;

        assert!(
            after_verdict.is_empty(),
            "a verified tip must not be dispatched again"
        );
        assert_eq!(
            harness.agent_calls(),
            1,
            "concurrent dispatches must run the QA agent once per tip"
        );
        assert_eq!(harness.stored_status(), DeployQaTipStatus::Verified);
    }

    #[tokio::test]
    async fn test_deploy_qa_dispatch_respects_qa_concurrency() {
        let mut config = test_config();
        config.qa.max_concurrent = 1;
        let harness = DeployQaHarness::dispatching(config);
        let next = harness
            .tracker
            .upsert_deploy_qa_tip(&DeployQaTip::new(DEPLOY_QA_TRACK, DEPLOY_QA_REPO, "1.2.4"))
            .unwrap();
        let statuses = || [harness.stored_status(), harness.status_of(&next)];

        let first = harness.dispatch_pending_tips().await;
        let while_busy = harness.dispatch_pending_tips().await;

        assert_eq!(first.len(), 1, "one tip should take the only QA slot");
        assert!(
            while_busy.is_empty(),
            "no tip may start while qa.max_concurrent dispatched runs are in flight"
        );
        finish_deploy_qa_runs(first).await;
        let mut after_first = statuses();
        after_first.sort_by_key(|status| status.to_string());
        assert_eq!(
            after_first,
            [DeployQaTipStatus::Pending, DeployQaTipStatus::Verified],
            "the tip left out must stay pending for a later dispatch"
        );

        let second = harness.dispatch_pending_tips().await;

        assert_eq!(second.len(), 1, "the freed slot should take the other tip");
        finish_deploy_qa_runs(second).await;
        assert_eq!(
            statuses(),
            [DeployQaTipStatus::Verified, DeployQaTipStatus::Verified]
        );
        assert_eq!(harness.agent_calls(), 2);
    }

    #[tokio::test]
    async fn test_panicked_deploy_qa_run_frees_its_dispatch_slot() {
        let mut config = test_config();
        config.qa.max_concurrent = 1;
        let harness = DeployQaHarness::dispatching_with(config, QaAnswer::Panic);
        harness
            .tracker
            .upsert_deploy_qa_tip(&DeployQaTip::new(DEPLOY_QA_TRACK, DEPLOY_QA_REPO, "1.2.4"))
            .unwrap();

        let first = harness.dispatch_pending_tips().await;
        assert_eq!(first.len(), 1, "one tip should take the only QA slot");
        finish_panicking_deploy_qa_runs(first).await;

        let after_panic = harness.dispatch_pending_tips().await;

        assert_eq!(
            after_panic.len(),
            1,
            "a dispatched run that panicked must release its claim so the other pending tip \
             can take the QA slot"
        );
        finish_panicking_deploy_qa_runs(after_panic).await;
        assert_eq!(
            harness.agent_calls(),
            2,
            "each pending tip should reach the QA agent once"
        );
    }

    #[tokio::test]
    async fn test_panicked_process_issue_releases_its_processing_slot() {
        let issue = Issue::new(
            "panic-1",
            "PANIC-1",
            "How do I rotate my API key?",
            "http://example.com/panic/1",
            "mock",
        );
        let source =
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])) as Arc<dyn IssueSource>;
        let agent_calls = Arc::new(AtomicUsize::new(0));
        let watcher = watcher_with_agent(
            test_config(),
            source.clone(),
            Arc::new(SqliteTracker::in_memory().unwrap()),
            Arc::new(ScriptedQaAgent {
                calls: agent_calls.clone(),
                answer: QaAnswer::PanicOnce(CUSTOMER_REPLY.to_string()),
            }),
        );
        let process = |watcher: Arc<Watcher>| {
            let source = source.clone();
            let issue = issue.clone();
            tokio::spawn(async move {
                let match_result = source.matches_criteria(&issue);
                watcher
                    .process_issue(
                        source,
                        issue,
                        match_result,
                        None,
                        None,
                        Some(Intent::Question),
                        None,
                    )
                    .await
            })
        };
        let slot_freed = watcher.slot_available.notified();
        tokio::pin!(slot_freed);
        slot_freed.as_mut().enable();

        let panicked = process(Arc::clone(&watcher)).await;

        assert!(
            panicked.is_err_and(|error| error.is_panic()),
            "the run should panic with its QA agent"
        );
        assert!(
            !watcher.lock_processing().contains("mock:panic-1"),
            "a run that panicked must release its processing key"
        );
        assert_eq!(
            watcher.active_count(),
            0,
            "a run that panicked must give back its processing slot"
        );
        assert_eq!(
            watcher.in_flight(),
            0,
            "a run that panicked must not hold the shutdown drain"
        );
        assert!(
            slot_freed.now_or_never().is_some(),
            "a run that panicked must wake tasks waiting for a free slot"
        );

        let retried = process(Arc::clone(&watcher))
            .await
            .expect("the retried run should not panic");

        assert_eq!(
            retried,
            IssueRun::Processed,
            "the issue must not be refused as already being processed"
        );
        assert_eq!(
            agent_calls.load(AtomicOrdering::SeqCst),
            2,
            "the retried run should reach the QA agent"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_stop_and_drain_waits_for_an_in_flight_qa_run() {
        let issue = Issue::new(
            "drain-1",
            "DRAIN-1",
            "How do I rotate my API key?",
            "http://example.com/drain/1",
            "mock",
        );
        let source =
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])) as Arc<dyn IssueSource>;
        let gate = Arc::new(Gate::default());
        let watcher = watcher_with_agent(
            test_config(),
            source.clone(),
            Arc::new(SqliteTracker::in_memory().unwrap()),
            Arc::new(ScriptedQaAgent {
                calls: Arc::new(AtomicUsize::new(0)),
                answer: QaAnswer::Gated(Arc::clone(&gate)),
            }),
        );
        watcher.set_running(true);
        let run = tokio::spawn({
            let watcher = Arc::clone(&watcher);
            async move {
                let match_result = source.matches_criteria(&issue);
                watcher
                    .process_issue(
                        source,
                        issue,
                        match_result,
                        None,
                        None,
                        Some(Intent::Question),
                        None,
                    )
                    .await
            }
        });
        gate.arrival().await;

        let drain = watcher.stop_and_drain();
        tokio::pin!(drain);
        let while_running = tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 3, drain.as_mut()).await;

        assert!(
            while_running.is_err(),
            "the drain must wait while the QA run is still in flight"
        );
        assert_eq!(
            watcher.active_count(),
            1,
            "the QA run must hold its processing slot until it finishes"
        );

        gate.open();
        let drained = tokio::time::timeout(DRAIN_RECHECK_INTERVAL * 2, drain).await;

        assert_eq!(
            drained,
            Ok(true),
            "the drain must end once the QA run finishes"
        );
        assert_eq!(
            run.await.expect("the QA run should not panic"),
            IssueRun::Processed,
            "the QA run should have been processed"
        );
        assert!(!gate.was_cancelled(), "draining must not cancel the QA run");
    }

    #[tokio::test]
    async fn test_run_dispatched_before_a_stop_does_not_start_after_it() {
        let issue = Issue::new(
            "dispatch-1",
            "DISPATCH-1",
            "How do I rotate my API key?",
            "http://example.com/dispatch/1",
            "mock",
        );
        let source =
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])) as Arc<dyn IssueSource>;
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let agent_calls = Arc::new(AtomicUsize::new(0));
        let watcher = watcher_with_agent(
            test_config(),
            source.clone(),
            Arc::clone(&tracker),
            Arc::new(ScriptedQaAgent {
                calls: Arc::clone(&agent_calls),
                answer: QaAnswer::Crash,
            }),
        );
        watcher.set_running(true);
        let match_result = source.matches_criteria(&issue);
        watcher
            .dispatch_lane(
                &source,
                vec![(issue.clone(), match_result, Some(Intent::Question))],
                1,
                true,
            )
            .await;
        assert!(
            !tracker.has_attempted("mock", &issue.id).unwrap(),
            "the dispatched run must not have started before the stop"
        );

        watcher.stop();
        watcher.drain_spawned_tasks().await;

        assert!(
            !tracker.has_attempted("mock", &issue.id).unwrap(),
            "a run dispatched before the stop must not start after it"
        );
        assert_eq!(
            agent_calls.load(AtomicOrdering::SeqCst),
            0,
            "a run dispatched before the stop must not reach the agent"
        );
    }

    #[tokio::test]
    async fn test_deploy_qa_tips_stay_pending_while_watcher_cannot_process() {
        let harness = DeployQaHarness::dispatching(test_config());

        harness.watcher.set_running(false);
        assert!(
            harness.dispatch_pending_tips().await.is_empty(),
            "a watcher that is not running must not start QA runs"
        );

        harness.watcher.set_running(true);
        harness.watcher.rate_limit_pause_until.write().await.insert(
            harness.watcher.config.agent.default_provider.clone(),
            Utc::now() + chrono::Duration::hours(1),
        );
        assert!(
            harness.dispatch_pending_tips().await.is_empty(),
            "a rate-limited watcher must not start QA runs"
        );

        assert_eq!(harness.agent_calls(), 0);
        assert_eq!(
            harness.stored_status(),
            DeployQaTipStatus::Pending,
            "an undispatched tip must stay pending for a later dispatch"
        );
    }

    #[tokio::test]
    async fn test_deploy_qa_tip_loaded_across_a_stop_stays_pending() {
        let mut gated = None;
        let harness = DeployQaHarness::build(test_config(), QaAnswer::Crash, |_, tip| {
            let source = Arc::new(GatedSource {
                name: DEPLOY_QA_SOURCE,
                issue: Some(deploy_qa_issue(tip)),
                ..GatedSource::default()
            });
            gated = Some(Arc::clone(&source));
            source
        });
        let source = gated.expect("the harness should build its source");
        harness.watcher.set_running(true);
        let mut runs = harness.dispatch_pending_tips().await;
        assert_eq!(runs.len(), 1, "the pending tip should be dispatched");
        source.gate.arrival_during(&mut runs[0]).await;

        harness.watcher.stop();
        source.gate.open();
        for run in runs {
            run.await.expect("the dispatched run should not panic");
        }

        assert_eq!(
            harness.agent_calls(),
            0,
            "a tip loaded after the stop must not reach the QA agent"
        );
        assert_eq!(
            harness.stored_status(),
            DeployQaTipStatus::Pending,
            "a tip loaded after the stop must stay pending for the next start"
        );
    }

    #[tokio::test]
    async fn test_retry_processing_its_issue_counts_once_as_active_processing() {
        let gate = Arc::new(Gate::default());
        let mut config = test_config();
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;
        let harness = DeployQaHarness::answering(config, QaAnswer::Gated(Arc::clone(&gate)));
        harness
            .tracker
            .record_attempt(DEPLOY_QA_SOURCE, &harness.tip.issue_id, &harness.tip.tag)
            .unwrap();
        harness
            .tracker
            .mark_failed(DEPLOY_QA_SOURCE, &harness.tip.issue_id, "initial failure")
            .unwrap();
        harness.watcher.set_running(true);
        let mut retries = tokio::spawn({
            let watcher = Arc::clone(&harness.watcher);
            async move { watcher.process_ready_retries().await }
        });
        gate.arrival_during(&mut retries).await;

        assert_eq!(
            harness.watcher.active_count(),
            1,
            "a retry processing its issue must count once as active processing"
        );
        assert_eq!(
            harness.watcher.in_flight(),
            2,
            "the drain must wait for both the retry and the issue it processes"
        );

        gate.open();
        let retried = retries.await;

        assert!(
            matches!(retried, Ok(Ok(()))),
            "the retries should finish: {retried:?}"
        );
        assert_eq!(
            harness.agent_calls(),
            1,
            "the retry should reach the QA agent"
        );
    }

    #[tokio::test]
    async fn test_deploy_qa_dispatch_requires_deploy_qa_source() {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher =
            create_test_watcher(Arc::new(MockNotifier::new(true)), tracker, vec![], false);
        watcher.set_running(true);

        assert!(
            watcher.dispatch_pending_deploy_qa_tips().await.is_err(),
            "dispatching without a deploy_qa source must be reported"
        );
    }

    #[tokio::test]
    async fn test_trigger_cascade_depth_limit() {
        let notifier = Arc::new(MockNotifier::new(true));
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let mut config = test_config();
        config.cascade.enabled = true;
        config.cascade.max_depth = 1;

        let mut relationships = RepoRelationships::new();
        relationships
            .add_dependency(
                "upstream",
                "downstream",
                claudear_analysis::repo::DependencyType::Npm,
                None,
            )
            .unwrap();

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources,
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: Some(relationships),
            github_client: None,
            scm_provider: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        // Record a root attempt and a child attempt so depth = 1
        sqlite
            .record_attempt("test", "root-issue", "ROOT-1")
            .unwrap();
        let root = sqlite.get_attempt("test", "root-issue").unwrap().unwrap();

        // Create child attempt with parent_attempt_id set to root
        sqlite
            .record_cascade_attempt("test", "child-issue", "CHILD-1", root.id, "org/upstream")
            .unwrap();
        let child = sqlite.get_attempt("test", "child-issue").unwrap().unwrap();
        assert_eq!(child.parent_attempt_id, Some(root.id));

        // The child is at depth 1 already, and max_depth is 1
        // So trigger_cascade should bail out early
        let result = watcher
            .trigger_cascade(
                &child,
                "https://github.com/org/upstream/pull/1",
                claudear_config::config::CascadeTrigger::Merge,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_start_clamps_low_interval() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> =
            vec![Arc::new(MockSource::new("mock")) as Arc<dyn IssueSource>];

        let watcher = Arc::new(create_test_watcher(notifier, tracker, sources, true));

        // Start with very small interval (should be clamped to 1000ms)
        let runner = {
            let w = Arc::clone(&watcher);
            tokio::spawn(async move { w.start(Some(50)).await })
        };

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        watcher.stop();

        let joined = tokio::time::timeout(std::time::Duration::from_secs(10), runner).await;
        assert!(joined.is_ok(), "watcher start did not stop in time");
        assert!(joined.unwrap().expect("task join failed").is_ok());
    }

    #[test]
    fn test_reset_attempt_nonexistent_succeeds() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let sources: Vec<Arc<dyn IssueSource>> = vec![];

        let watcher = create_test_watcher(notifier, tracker, sources, false);

        // Resetting an attempt that was never recorded should succeed silently
        let result = watcher.reset_attempt("test", "nonexistent");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_poll_source_applies_suppression_when_prioritisation_disabled() {
        use claudear_config::config::PrioritisationConfig;
        use claudear_core::types::{SuppressionField, SuppressionMatchMode, SuppressionRule};

        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![Issue::new(
            "1",
            "T-1",
            "Suppress me please",
            "http://example.com/1",
            "test",
        )];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.prioritisation = PrioritisationConfig {
            enabled: false,
            suppression_rules: vec![SuppressionRule {
                name: "suppress-all".to_string(),
                pattern: ".*".to_string(),
                field: SuppressionField::Title,
                match_mode: SuppressionMatchMode::Regex,
                sources: vec![],
                reason: "test suppression".to_string(),
            }],
            ..Default::default()
        };

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source.clone()],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: true,
            llm_engine: None,
        }));

        watcher.poll_source(&source).await.unwrap();

        // Issue should be suppressed
        let matched = tracker.get_metrics("issues_matched", None, 10).unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].metric_value, 0.0);
    }

    #[test]
    fn test_enhance_prompt_with_learning_repo_knowledge_enabled_but_empty() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let mut config = test_config();
        config.learning.repo_knowledge = true;
        config.learning.qa_promotion = true;
        config.learning.strategy_fingerprinting = true;
        config.learning.cluster_detection = true;
        config.learning.cross_repo_correlation = true;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let base = "Fix the authentication bug";
        let issue = test_issue();
        // With learning enabled but no data in DB, should return base prompt unchanged
        let result = crate::processing::enhance_prompt_with_learning(
            &watcher.config,
            &watcher.tracker,
            base,
            &issue,
            Some("org/my-repo"),
        );
        assert_eq!(result, base);
    }

    #[test]
    fn test_enhance_prompt_with_empty_repo_name() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        let base = "Fix the bug";
        let issue = test_issue();
        // Empty string repo name should still attempt learning but find nothing
        let result = crate::processing::enhance_prompt_with_learning(
            &watcher.config,
            &watcher.tracker,
            base,
            &issue,
            Some(""),
        );
        assert_eq!(result, base);
    }

    #[tokio::test]
    async fn test_notify_failed_with_escalation_non_hard_error() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier.clone(), tracker, vec![], false);

        let issue = test_issue();
        let result = crate::processing::notify_failed_with_escalation(
            &watcher.notifier,
            &watcher.tracker,
            &issue,
            "simple build error",
        )
        .await;
        assert!(result.is_ok());
        // Should have called notify_failed once
        assert_eq!(notifier.get_call_count(), 1);
    }

    #[tokio::test]
    async fn test_notify_failed_with_escalation_hard_error_rate_limit() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier.clone(), tracker, vec![], false);

        let issue = test_issue();
        // "rate limit" triggers hard error escalation
        let result = crate::processing::notify_failed_with_escalation(
            &watcher.notifier,
            &watcher.tracker,
            &issue,
            "rate limit exceeded, try again later",
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(notifier.get_call_count(), 1);
    }

    #[tokio::test]
    async fn test_notify_failed_with_escalation_hard_error_timeout() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier.clone(), tracker, vec![], false);

        let issue = test_issue();
        let result = crate::processing::notify_failed_with_escalation(
            &watcher.notifier,
            &watcher.tracker,
            &issue,
            "process timed out after 300s",
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(notifier.get_call_count(), 1);
    }

    #[tokio::test]
    async fn test_notify_failed_with_escalation_hard_error_spawn_failure() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier.clone(), tracker, vec![], false);

        let mut issue = test_issue();
        issue.metadata.insert(
            "resolved_user".to_string(),
            serde_json::Value::String("alice".to_string()),
        );

        // Hard error should remove resolved_user (escalate to global)
        let result = crate::processing::notify_failed_with_escalation(
            &watcher.notifier,
            &watcher.tracker,
            &issue,
            "failed to spawn claude",
        )
        .await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_record_error_pattern_various_error_types() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        // Test multiple error types
        crate::processing::record_error_pattern(
            &watcher.tracker,
            "test",
            "1",
            "rate limit exceeded",
        );
        crate::processing::record_error_pattern(
            &watcher.tracker,
            "test",
            "2",
            "process timed out after 300s",
        );
        crate::processing::record_error_pattern(
            &watcher.tracker,
            "test",
            "3",
            "No PR URL found in output",
        );
        crate::processing::record_error_pattern(
            &watcher.tracker,
            "test",
            "4",
            "Repository resolution failed: no match",
        );
        crate::processing::record_error_pattern(
            &watcher.tracker,
            "test",
            "5",
            "Failed to create worktree: git error",
        );
    }

    #[tokio::test]
    async fn test_record_feedback_outcome_from_attempt_no_sqlite() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        let attempt = claudear_core::types::FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Failed,
            error_message: None,
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        // Should not panic with default trait impl
        watcher
            .record_feedback_outcome_from_attempt(&attempt, Outcome::Failed)
            .await;
    }

    #[tokio::test]
    async fn test_record_feedback_outcome_from_attempt_with_sqlite() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config: test_config(),
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        // Record an attempt so we can reconstruct it
        sqlite.record_attempt("test", "ISSUE-1", "ISSUE-1").unwrap();
        let attempt = sqlite.get_attempt("test", "ISSUE-1").unwrap().unwrap();

        // Should not panic
        watcher
            .record_feedback_outcome_from_attempt(&attempt, Outcome::Merged)
            .await;
    }

    #[tokio::test]
    async fn test_record_feedback_outcome_stores_to_tracker() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config: test_config(),
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        sqlite.record_attempt("test", "1", "T-1").unwrap();
        let attempt = sqlite.get_attempt("test", "1").unwrap().unwrap();
        let issue = test_issue();

        crate::processing::record_feedback_outcome(
            &watcher.tracker,
            watcher.embedding_client.as_deref(),
            watcher.issue_embedding_service.as_deref(),
            &watcher.feedback_analyzer,
            &attempt.source,
            &issue,
            Outcome::Failed,
        )
        .await;

        // Verify outcome was stored
        let outcome = sqlite.get_feedback_outcome_by_attempt(attempt.id);
        assert!(outcome.is_ok());
    }

    #[tokio::test]
    async fn test_run_periodic_learning_all_disabled() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let mut config = test_config();
        config.learning.qa_promotion = false;
        config.learning.cluster_detection = false;
        config.learning.cross_repo_correlation = false;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        // Should complete without panicking
        watcher.run_periodic_learning().await;
    }

    #[tokio::test]
    async fn test_run_periodic_learning_with_cluster_detection_enabled() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let source = Arc::new(MockSource::new("test")) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.learning.qa_promotion = false;
        config.learning.cluster_detection = true;
        config.learning.cross_repo_correlation = false;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        // Should complete without panicking even with no data
        watcher.run_periodic_learning().await;
    }

    #[tokio::test]
    async fn test_run_periodic_learning_with_cross_repo_enabled() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let mut config = test_config();
        config.learning.qa_promotion = false;
        config.learning.cluster_detection = false;
        config.learning.cross_repo_correlation = true;
        config.learning.cross_repo_window_hours = 24;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        watcher.run_periodic_learning().await;
    }

    #[tokio::test]
    async fn test_run_periodic_learning_with_qa_promotion_enabled() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let mut config = test_config();
        config.learning.qa_promotion = true;
        config.learning.qa_promotion_threshold = 3;
        config.learning.cluster_detection = false;
        config.learning.cross_repo_correlation = false;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        watcher.run_periodic_learning().await;
    }

    #[tokio::test]
    async fn test_run_post_merge_learning_all_disabled() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let mut config = test_config();
        config.learning.auto_extract_learnings = false;
        config.learning.diff_analysis = false;
        config.learning.quality_scoring = false;
        config.learning.auto_agent_md = false;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let attempt = claudear_core::types::FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: Some("https://github.com/org/repo/pull/1".to_string()),
            scm_repo: Some("org/repo".to_string()),
            scm_pr_number: Some(1),
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        // Should complete without panicking
        watcher.run_post_merge_learning(&attempt).await;
    }

    #[tokio::test]
    async fn test_run_post_merge_learning_auto_extract_enabled_no_sqlite() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let mut config = test_config();
        config.learning.auto_extract_learnings = true;
        config.learning.diff_analysis = false;
        config.learning.quality_scoring = false;
        config.learning.auto_agent_md = false;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let attempt = claudear_core::types::FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        // Should skip extraction path when no executions exist
        watcher.run_post_merge_learning(&attempt).await;
    }

    #[tokio::test]
    async fn test_run_post_merge_learning_diff_analysis_no_github_client() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let mut config = test_config();
        config.learning.auto_extract_learnings = false;
        config.learning.diff_analysis = true;
        config.learning.quality_scoring = false;
        config.learning.auto_agent_md = false;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: None,
            github_client: None, // No GitHub client
            scm_provider: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let attempt = claudear_core::types::FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: Some("https://github.com/org/repo/pull/1".to_string()),
            scm_repo: Some("org/repo".to_string()),
            scm_pr_number: Some(1),
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        // Should skip diff analysis because github_client is None
        watcher.run_post_merge_learning(&attempt).await;
    }

    #[tokio::test]
    async fn test_run_post_merge_learning_quality_scoring_no_pr_url() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let mut config = test_config();
        config.learning.auto_extract_learnings = false;
        config.learning.diff_analysis = false;
        config.learning.quality_scoring = true;
        config.learning.auto_agent_md = false;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let attempt = claudear_core::types::FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: None, // No PR URL
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        // Should skip quality scoring because pr_url is None
        watcher.run_post_merge_learning(&attempt).await;
    }

    #[tokio::test]
    async fn test_run_post_merge_learning_auto_agent_md_no_scm_repo() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let mut config = test_config();
        config.learning.auto_extract_learnings = false;
        config.learning.diff_analysis = false;
        config.learning.quality_scoring = false;
        config.learning.auto_agent_md = true;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let attempt = claudear_core::types::FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: None,
            scm_repo: None, // No scm_repo
            scm_pr_number: None,
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        // Should skip auto_agent_md because scm_repo is None
        watcher.run_post_merge_learning(&attempt).await;
    }

    #[test]
    fn test_get_cascade_depth_with_chain() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config: test_config(),
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        // Create a chain: root -> child -> grandchild
        sqlite.record_attempt("test", "root", "ROOT").unwrap();
        let root = sqlite.get_attempt("test", "root").unwrap().unwrap();

        sqlite
            .record_cascade_attempt("test", "child", "CHILD", root.id, "org/repo")
            .unwrap();
        let child = sqlite.get_attempt("test", "child").unwrap().unwrap();

        sqlite
            .record_cascade_attempt("test", "grandchild", "GRANDCHILD", child.id, "org/repo2")
            .unwrap();
        let grandchild = sqlite.get_attempt("test", "grandchild").unwrap().unwrap();

        assert_eq!(watcher.get_cascade_depth(&root), 0);
        assert_eq!(watcher.get_cascade_depth(&child), 1);
        assert_eq!(watcher.get_cascade_depth(&grandchild), 2);
    }

    #[tokio::test]
    async fn test_trigger_cascade_unlimited_depth() {
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let mut config = test_config();
        config.cascade.enabled = true;
        config.cascade.max_depth = 0; // Unlimited

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: Some(RepoRelationships::new()),
            github_client: None,
            scm_provider: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        sqlite.record_attempt("test", "root", "ROOT").unwrap();
        let root = sqlite.get_attempt("test", "root").unwrap().unwrap();

        sqlite
            .record_cascade_attempt("test", "deep-child", "DEEP", root.id, "org/repo")
            .unwrap();
        let deep_child = sqlite.get_attempt("test", "deep-child").unwrap().unwrap();

        let attempt = FixAttempt {
            id: deep_child.id,
            issue_id: deep_child.issue_id,
            short_id: deep_child.short_id,
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: Some("https://github.com/org/repo/pull/1".to_string()),
            scm_repo: Some("org/repo".to_string()),
            scm_pr_number: Some(1),
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: Some(root.id),
            cascade_repo: None,
        };

        // With max_depth=0, cascade should NOT be blocked by depth
        // It will still return Ok because there are no dependants
        let result = watcher
            .trigger_cascade(
                &attempt,
                "https://github.com/org/repo/pull/1",
                claudear_config::config::CascadeTrigger::Merge,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_check_and_auto_close_prs_with_terminal_issue() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Create a source whose issues have resolved status
        let mut issue = Issue::new(
            "resolved-1",
            "R-1",
            "A resolved issue",
            "http://example.com/resolved/1",
            "mock",
        );
        issue.status = claudear_core::types::IssueStatus::Resolved;

        let source = Arc::new(MockSource::with_issues("mock", vec![issue])) as Arc<dyn IssueSource>;

        // Record a successful attempt with PR
        tracker.record_attempt("mock", "resolved-1", "R-1").unwrap();
        tracker
            .mark_success("mock", "resolved-1", "https://github.com/org/repo/pull/42")
            .unwrap();

        let watcher = create_test_watcher(notifier.clone(), tracker.clone(), vec![source], false);

        let auto_closed = watcher.check_and_auto_close_prs().await.unwrap();

        // The issue status is "Resolved" which is terminal, so PR should be auto-closed
        assert_eq!(auto_closed.len(), 1);
        assert_eq!(auto_closed[0], "https://github.com/org/repo/pull/42");

        // Verify attempt was marked as closed
        let attempt = tracker.get_attempt("mock", "resolved-1").unwrap().unwrap();
        assert_eq!(attempt.status, FixAttemptStatus::Closed);
    }

    #[tokio::test]
    async fn test_run_housekeeping_cycle_with_active_processing() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![], false);
        watcher.is_running.store(true, Ordering::SeqCst);
        watcher.active_processing.fetch_add(5, Ordering::SeqCst);

        let result = watcher.run_housekeeping_cycle().await;
        assert!(result.is_ok());

        let active = tracker.get_metrics("active_processing", None, 10).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].metric_value, 5.0);
    }

    #[tokio::test]
    async fn test_poll_source_with_prioritisation_enabled() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![
            Issue::new("1", "T-1", "Issue 1", "http://example.com/1", "test"),
            Issue::new("2", "T-2", "Issue 2", "http://example.com/2", "test"),
        ];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.prioritisation.enabled = true;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source.clone()],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: true,
            llm_engine: None,
        }));

        let result = watcher.poll_source(&source).await;
        assert!(result.is_ok());

        // Verify metrics were recorded
        let queued = tracker.get_metrics("issues_queued", None, 10).unwrap();
        assert_eq!(queued.len(), 1);
        assert!(queued[0].metric_value >= 0.0);
    }

    // Additional coverage: process_issue with repo resolution skip (already
    // tested but verify cleanup)
    #[tokio::test]
    async fn test_process_issue_cleans_up_on_repo_skip() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issue = Issue::new(
            "cleanup-1",
            "CLEAN-1",
            "Test cleanup",
            "http://example.com/cleanup/1",
            "mock",
        );
        let source =
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source.clone()], false);

        let match_result = MatchResult::matched("Test", MatchPriority::Normal);
        let outcome = watcher
            .process_issue(source, issue, match_result, None, None, None, None)
            .await;
        assert_eq!(
            outcome,
            IssueRun::Processed,
            "a run that fails after starting still counts as processed"
        );
        assert!(!watcher.lock_processing().contains("mock:cleanup-1"));
        assert_eq!(watcher.active_count(), 0);
    }

    #[tokio::test]
    async fn test_seed_preserves_issue_labels() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let mut issue = Issue::new(
            "labeled-1",
            "L-1",
            "Labeled issue",
            "http://example.com/labeled/1",
            "mock",
        );
        issue.set_metadata("labels", vec!["bug".to_string(), "critical".to_string()]);

        let source = Arc::new(MockSource::with_issues("mock", vec![issue])) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source], false);

        let result = watcher.seed().await.unwrap();
        assert_eq!(result.total, 1);

        // Verify the issue was recorded
        assert!(tracker.has_attempted("mock", "labeled-1").unwrap());
    }

    #[test]
    fn test_truncate_error_exactly_at_boundary() {
        // Test with exactly 497 chars (no truncation needed for exactly 500 total)
        let error = "b".repeat(497);
        let result = crate::processing::truncate_error_for_activity(&error);
        assert_eq!(result.len(), 497);
        assert!(!result.ends_with("..."));
    }

    #[test]
    fn test_truncate_error_single_char() {
        let result = crate::processing::truncate_error_for_activity("x");
        assert_eq!(result, "x");
    }

    #[test]
    fn test_truncate_error_all_unicode() {
        // A string of 200 4-byte emojis (800 bytes, 200 chars)
        let error: String = std::iter::repeat_n('\u{1F600}', 200).collect();
        let result = crate::processing::truncate_error_for_activity(&error);
        // Should not panic and should end with "..."
        assert!(result.ends_with("..."));
        assert!(result.is_char_boundary(result.len()));
    }

    #[tokio::test(start_paused = true)]
    async fn test_stop_and_drain_does_not_hang_forever() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = Arc::new(create_test_watcher(notifier, tracker, vec![], false));
        watcher.is_running.store(true, Ordering::SeqCst);
        watcher.in_flight.store(1, Ordering::SeqCst);

        let started = tokio::time::Instant::now();
        let drained = tokio::time::timeout(DRAIN_TIMEOUT * 2, watcher.stop_and_drain()).await;

        assert_eq!(
            drained,
            Ok(false),
            "a drain that runs out of time must give up and report it"
        );
        assert_eq!(
            started.elapsed(),
            DRAIN_TIMEOUT,
            "the drain must give up exactly when its budget runs out"
        );
        assert!(!watcher.is_running());
    }

    #[tokio::test]
    async fn test_poll_source_all_issues_already_attempted() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Pre-mark all issues as attempted
        tracker.record_attempt("test", "1", "T-1").unwrap();
        tracker.record_attempt("test", "2", "T-2").unwrap();

        let issues = vec![
            Issue::new("1", "T-1", "Issue 1", "http://example.com/1", "test"),
            Issue::new("2", "T-2", "Issue 2", "http://example.com/2", "test"),
        ];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source.clone()], false);

        watcher.poll_source(&source).await.unwrap();

        // All issues were already attempted, so none should be queued
        let queued = tracker.get_metrics("issues_queued", None, 10).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].metric_value, 0.0);
    }

    #[tokio::test]
    async fn test_poll_source_stops_when_not_running() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![
            Issue::new("1", "T-1", "Issue 1", "http://example.com/1", "test"),
            Issue::new("2", "T-2", "Issue 2", "http://example.com/2", "test"),
        ];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;

        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source.clone()], false);
        // Deliberately NOT setting is_running to true
        // The poll_source should still work but process_issue checks won't queue

        let result = watcher.poll_source(&source).await;
        assert!(result.is_ok());
    }

    /// SCM stub that reports the given PR numbers as merged and the rest as open.
    struct MergedPrs(Vec<i64>);

    #[async_trait::async_trait]
    impl claudear_integrations::scm::ScmProvider for MergedPrs {
        fn name(&self) -> &str {
            "github"
        }
        fn is_enabled(&self) -> bool {
            true
        }
        fn review_trigger(&self) -> &str {
            "@claudear"
        }
        async fn get_pr_status(
            &self,
            _project: &str,
            number: i64,
        ) -> claudear_core::error::Result<PrStatus> {
            Ok(if self.0.contains(&number) {
                PrStatus::Merged
            } else {
                PrStatus::Open
            })
        }
        async fn get_pr_info(
            &self,
            _project: &str,
            _number: i64,
        ) -> claudear_core::error::Result<claudear_integrations::scm::PrInfo> {
            Ok(claudear_integrations::scm::PrInfo {
                head_branch: None,
                base_branch: None,
                title: None,
                author: None,
            })
        }
        async fn get_pr_diff(
            &self,
            _project: &str,
            _number: i64,
        ) -> claudear_core::error::Result<String> {
            Ok(String::new())
        }
        async fn get_reviews(
            &self,
            _project: &str,
            _number: i64,
        ) -> claudear_core::error::Result<Vec<claudear_integrations::scm::CodeReview>> {
            Ok(Vec::new())
        }
        async fn get_review_comments(
            &self,
            _project: &str,
            _number: i64,
        ) -> claudear_core::error::Result<Vec<claudear_integrations::scm::ReviewComment>> {
            Ok(Vec::new())
        }
        async fn list_repos(
            &self,
            _org: &str,
        ) -> claudear_core::error::Result<Vec<claudear_integrations::scm::RemoteRepo>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn test_merged_cascade_pr_is_handled_once_and_leaves_the_parent_alone() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        tracker.record_attempt("discord", "1", "D-1").unwrap();
        tracker
            .mark_success("discord", "1", "https://github.com/org/app/pull/1")
            .unwrap();
        let parent = tracker.get_attempt("discord", "1").unwrap().unwrap();
        let child = tracker
            .record_cascade_attempt(
                "discord",
                "1",
                "D-1",
                parent.id,
                "git@github.com:org/lib.git",
            )
            .unwrap();
        tracker
            .update_attempt_pr(child, "https://github.com/org/lib/pull/120", "org/lib", 120)
            .unwrap();

        let mut config = test_config();
        config.cascade.enabled = false;
        let watcher = Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: None,
            github_client: None,
            scm_provider: Some(Arc::new(MergedPrs(vec![120]))),
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        });
        watcher.set_running(true);

        watcher.check_pr_merges_and_cascade().await.unwrap();
        watcher.check_pr_merges_and_cascade().await.unwrap();

        // Before, the parent was marked merged and the cascade row stayed pending,
        // so every poll saw the merge again
        let merged_per_poll: Vec<f64> = tracker
            .get_metrics("pr_status_merged", None, 10)
            .unwrap()
            .into_iter()
            .map(|m| m.metric_value)
            .collect();
        assert_eq!(merged_per_poll.iter().sum::<f64>(), 1.0);
        let parent = tracker.get_attempt("discord", "1").unwrap().unwrap();
        assert_eq!(
            parent.status,
            claudear_core::types::FixAttemptStatus::Success
        );
    }

    #[tokio::test]
    async fn test_check_pr_merges_records_all_lifecycle_metrics() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![], false);

        watcher.check_pr_merges_and_cascade().await.unwrap();

        let metric_names = [
            "pr_status_checks",
            "pr_status_merged",
            "pr_status_closed",
            "pr_status_errors",
            "regression_watches_created",
            "auto_resolved_on_merge",
            "cascade_triggered",
            "cascade_failed",
        ];

        for name in &metric_names {
            let metrics = tracker.get_metrics(name, None, 10).unwrap();
            assert_eq!(
                metrics.len(),
                1,
                "Expected exactly 1 metric for {}, got {}",
                name,
                metrics.len()
            );
            assert_eq!(
                metrics[0].metric_value, 0.0,
                "Expected 0.0 for metric {}, got {}",
                name, metrics[0].metric_value
            );
        }
    }

    #[test]
    fn test_watcher_new_with_all_optional_fields() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let relationships = RepoRelationships::new();

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config: test_config(),
            sources: vec![
                Arc::new(MockSource::new("s1")) as Arc<dyn IssueSource>,
                Arc::new(MockSource::new("s2")) as Arc<dyn IssueSource>,
            ],
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: Some(relationships),
            github_client: None,
            scm_provider: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: true,
            llm_engine: None,
        }));

        assert!(watcher.dry_run);
        assert!(watcher.relationships.is_some());
        assert_eq!(watcher.sources.len(), 2);
        assert!(!watcher.is_running());
        assert_eq!(watcher.active_count(), 0);
    }

    #[tokio::test]
    async fn test_process_ready_retries_skips_inflight() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Create a failed attempt that would be retried
        tracker
            .record_attempt("mock", "inflight-retry", "MOCK-IR")
            .unwrap();
        tracker
            .mark_failed("mock", "inflight-retry", "initial failure")
            .unwrap();

        let source = Arc::new(MockSource::new("mock")) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;
        config.processing_delay_ms = 0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));
        watcher.is_running.store(true, Ordering::SeqCst);

        // Mark the issue as currently processing
        watcher
            .lock_processing()
            .insert("mock:inflight-retry".to_string());

        let result = watcher.process_ready_retries().await;
        assert!(result.is_ok());

        // Attempt should still be failed (retry was skipped because inflight)
        let attempt = tracker
            .get_attempt("mock", "inflight-retry")
            .unwrap()
            .unwrap();
        // The retry was skipped, so retry_count should remain 0
        assert_eq!(attempt.retry_count, 0);
    }

    #[tokio::test]
    async fn test_process_ready_retries_stops_when_not_running() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        tracker
            .record_attempt("mock", "stop-retry", "MOCK-SR")
            .unwrap();
        tracker
            .mark_failed("mock", "stop-retry", "initial failure")
            .unwrap();

        let source = Arc::new(MockSource::with_issues(
            "mock",
            vec![Issue::new(
                "stop-retry",
                "MOCK-SR",
                "Stop retry",
                "http://example.com",
                "mock",
            )],
        )) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));
        // NOT setting is_running - should break the retry loop early
        watcher.is_running.store(false, Ordering::SeqCst);

        let result = watcher.process_ready_retries().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_poll_records_stats_metrics() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Add some attempts to get non-zero stats
        tracker.record_attempt("test", "1", "T-1").unwrap();
        tracker.record_attempt("test", "2", "T-2").unwrap();
        tracker.mark_failed("test", "2", "error").unwrap();

        let watcher = create_test_watcher(notifier, tracker.clone(), vec![], false);
        watcher.poll().await.unwrap();

        let pending = tracker.get_metrics("pending_attempts", None, 10).unwrap();
        assert_eq!(pending.len(), 1);

        let total = tracker.get_metrics("total_attempts", None, 10).unwrap();
        assert_eq!(total.len(), 1);
        assert_eq!(total[0].metric_value, 2.0);
    }

    #[test]
    fn test_group_review_feedback_preserves_insertion_order() {
        let make_review = |id: i64, body: &str| claudear_integrations::scm::CodeReview {
            id,
            state: "CHANGES_REQUESTED".to_string(),
            body: Some(body.to_string()),
            user: claudear_integrations::scm::ReviewUser {
                id,
                login: format!("user{}", id),
                user_type: Some("User".to_string()),
            },
            submitted_at: Some("2024-01-01T00:00:00Z".to_string()),
            html_url: None,
        };

        let events = vec![
            claudear_integrations::scm::ReviewEvent::ReviewSubmitted {
                pr_url: "https://github.com/org/repo/pull/3".to_string(),
                repo: "org/repo".to_string(),
                pr_number: 3,
                review: make_review(1, "third PR first"),
                inline_comments: vec![],
            },
            claudear_integrations::scm::ReviewEvent::ReviewSubmitted {
                pr_url: "https://github.com/org/repo/pull/1".to_string(),
                repo: "org/repo".to_string(),
                pr_number: 1,
                review: make_review(2, "first PR"),
                inline_comments: vec![],
            },
            claudear_integrations::scm::ReviewEvent::ReviewSubmitted {
                pr_url: "https://github.com/org/repo/pull/3".to_string(),
                repo: "org/repo".to_string(),
                pr_number: 3,
                review: make_review(3, "third PR second"),
                inline_comments: vec![],
            },
        ];

        let grouped = Watcher::group_review_feedback_by_pr(events);
        assert_eq!(grouped.len(), 2);
        // PR 3 appeared first so it should be first
        assert_eq!(grouped[0].0, "https://github.com/org/repo/pull/3");
        assert_eq!(grouped[0].2, 2); // 2 reviews for PR 3
        assert_eq!(grouped[1].0, "https://github.com/org/repo/pull/1");
        assert_eq!(grouped[1].2, 1); // 1 review for PR 1
    }

    #[tokio::test]
    async fn test_trigger_cascade_uses_short_name_fallback() {
        use claudear_analysis::repo::DependencyType;
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let mut config = test_config();
        config.cascade.enabled = true;
        config.cascade.max_depth = 0;

        // Add dependency using short name (no org prefix)
        let mut relationships = RepoRelationships::new();
        relationships
            .add_dependency("upstream-lib", "downstream-app", DependencyType::Npm, None)
            .unwrap();

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: Some(relationships),
            github_client: None,
            scm_provider: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let attempt = FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            // scm_repo is "org/upstream-lib" but dependency graph has "upstream-lib"
            pr_url: Some("https://github.com/org/upstream-lib/pull/1".to_string()),
            scm_repo: Some("org/upstream-lib".to_string()),
            scm_pr_number: Some(1),
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        // This exercises the short_name fallback path in trigger_cascade
        // It will find dependants via the short name "upstream-lib"
        // but cascade_to_repo will fail because no inferrer is configured
        let result = watcher
            .trigger_cascade(
                &attempt,
                "https://github.com/org/upstream-lib/pull/1",
                claudear_config::config::CascadeTrigger::Merge,
            )
            .await;
        // Should still return Ok even if individual cascade_to_repo fails
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_process_issue_records_attempt_early() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issue = Issue::new(
            "early-record",
            "ER-1",
            "Early record test",
            "http://example.com/er/1",
            "mock",
        );
        let source =
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source.clone()], false);

        let match_result = MatchResult::matched("Test", MatchPriority::Normal);
        watcher
            .process_issue(source, issue, match_result, None, None, None, None)
            .await;

        // Verify the attempt was recorded
        assert!(tracker.has_attempted("mock", "early-record").unwrap());
    }

    #[tokio::test]
    async fn test_poll_with_sources_records_source_count() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let sources: Vec<Arc<dyn IssueSource>> = vec![
            Arc::new(MockSource::new("s1")),
            Arc::new(MockSource::new("s2")),
            Arc::new(MockSource::new("s3")),
        ];
        let watcher = create_test_watcher(notifier, tracker.clone(), sources, false);
        watcher.poll().await.unwrap();

        let source_count = tracker.get_metrics("poll_sources", None, 10).unwrap();
        assert_eq!(source_count.len(), 1);
        assert_eq!(source_count[0].metric_value, 3.0);
    }

    #[tokio::test]
    async fn test_check_and_auto_close_prs_issue_status_error() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Record a successful attempt with a PR for issue that doesn't exist in the source
        tracker
            .record_attempt("mock", "nonexistent", "NE-1")
            .unwrap();
        tracker
            .mark_success("mock", "nonexistent", "https://github.com/org/repo/pull/1")
            .unwrap();

        // MockSource with no issues - get_issue_status will fail
        let source = Arc::new(MockSource::new("mock")) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source], false);

        let result = watcher.check_and_auto_close_prs().await.unwrap();
        // Should not auto-close because get_issue_status returned error
        assert!(result.is_empty());

        // Attempt status should remain unchanged
        let attempt = tracker.get_attempt("mock", "nonexistent").unwrap().unwrap();
        assert_eq!(attempt.status, FixAttemptStatus::Success);
    }

    #[tokio::test]
    async fn test_seed_records_labels_from_metadata() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let mut issue = Issue::new(
            "1",
            "T-1",
            "Bug with labels",
            "http://example.com/1",
            "mock",
        );
        issue.set_metadata(
            "labels",
            vec!["bug".to_string(), "high-priority".to_string()],
        );

        let source = Arc::new(MockSource::with_issues("mock", vec![issue])) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source], false);

        let result = watcher.seed().await.unwrap();
        assert_eq!(result.total, 1);

        // Verify the issue was marked with labels
        let attempt = tracker.get_attempt("mock", "1").unwrap().unwrap();
        assert!(attempt.issue_labels.contains(&"bug".to_string()));
        assert!(attempt.issue_labels.contains(&"high-priority".to_string()));
    }

    #[tokio::test]
    async fn test_poll_source_uses_per_source_max_issues() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues: Vec<Issue> = (1..=10)
            .map(|i| {
                Issue::new(
                    format!("{}", i),
                    format!("T-{}", i),
                    format!("Issue {}", i),
                    format!("http://example.com/{}", i),
                    "test",
                )
            })
            .collect();
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;

        let mut config = test_config();
        // Global limit is 10 but we want to verify it applies
        config.max_issues_per_cycle = 2;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source.clone()],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: true,
            llm_engine: None,
        }));

        watcher.poll_source(&source).await.unwrap();

        let queued = tracker.get_metrics("issues_queued", None, 10).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].metric_value, 2.0);
    }

    #[tokio::test]
    async fn test_process_ready_retries_empty() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![], false);
        watcher.is_running.store(true, Ordering::SeqCst);

        let result = watcher.process_ready_retries().await;
        assert!(result.is_ok());

        // Should record zero-value metrics
        let retries_found = tracker
            .get_metrics("ready_retries_found", None, 10)
            .unwrap();
        assert_eq!(retries_found.len(), 1);
        assert_eq!(retries_found[0].metric_value, 0.0);

        let executed = tracker
            .get_metrics("ready_retries_executed_total", None, 10)
            .unwrap();
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0].metric_value, 0.0);
    }

    #[tokio::test]
    async fn test_trigger_cascade_full_name_match() {
        use claudear_analysis::repo::DependencyType;
        use claudear_core::types::{FixAttempt, FixAttemptStatus};

        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let mut config = test_config();
        config.cascade.enabled = true;
        config.cascade.max_depth = 0;

        // Add dependency using full org/repo name
        let mut relationships = RepoRelationships::new();
        relationships
            .add_dependency(
                "org/upstream-lib",
                "org/downstream-app",
                DependencyType::Npm,
                None,
            )
            .unwrap();

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: Some(relationships),
            github_client: None,
            scm_provider: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let attempt = FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: Some("https://github.com/org/upstream-lib/pull/1".to_string()),
            scm_repo: Some("org/upstream-lib".to_string()),
            scm_pr_number: Some(1),
            status: FixAttemptStatus::Merged,
            error_message: None,
            merged_at: Some(chrono::Utc::now()),
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        // This exercises the full_name match path (not the short_name fallback)
        let result = watcher
            .trigger_cascade(
                &attempt,
                "https://github.com/org/upstream-lib/pull/1",
                claudear_config::config::CascadeTrigger::Merge,
            )
            .await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_enhance_prompt_with_learning_cluster_detection_enabled() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let mut config = test_config();
        config.learning.cluster_detection = true;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let base = "Fix the auth bug";
        let issue = test_issue();
        let result = crate::processing::enhance_prompt_with_learning(
            &watcher.config,
            &watcher.tracker,
            base,
            &issue,
            Some("org/my-repo"),
        );
        // With no clusters stored, should return base prompt
        assert_eq!(result, base);
    }

    #[tokio::test]
    async fn test_poll_source_fetched_metric_reflects_total_issues() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![
            Issue::new("1", "T-1", "Issue 1", "http://example.com/1", "test"),
            Issue::new("2", "T-2", "Issue 2", "http://example.com/2", "test"),
            Issue::new("3", "T-3", "Issue 3", "http://example.com/3", "test"),
        ];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source.clone()], true);

        watcher.poll_source(&source).await.unwrap();

        let fetched = tracker.get_metrics("issues_fetched", None, 10).unwrap();
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].metric_value, 3.0);
    }

    #[tokio::test]
    async fn test_poll_source_records_batch_processed_metric() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![Issue::new(
            "1",
            "T-1",
            "Issue 1",
            "http://example.com/1",
            "test",
        )];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source.clone()], false);
        watcher.is_running.store(true, Ordering::SeqCst);

        watcher.poll_source(&source).await.unwrap();

        let batch = tracker.get_metrics("batch_processed", None, 10).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].metric_value, 1.0);
    }

    #[test]
    fn test_seed_result_default_all_fields() {
        let result = SeedResult::default();
        assert_eq!(result.total, 0);
        assert!(result.by_source.is_empty());
        assert_eq!(result.by_source.len(), 0);
    }

    #[test]
    fn test_seed_result_multiple_sources() {
        let mut result = SeedResult {
            total: 15,
            ..Default::default()
        };
        result.by_source.insert("sentry".to_string(), 7);
        result.by_source.insert("linear".to_string(), 5);
        result.by_source.insert("jira".to_string(), 3);

        assert_eq!(result.by_source.len(), 3);
        assert_eq!(*result.by_source.get("sentry").unwrap(), 7);
        assert_eq!(*result.by_source.get("linear").unwrap(), 5);
        assert_eq!(*result.by_source.get("jira").unwrap(), 3);
    }

    #[test]
    fn test_watcher_new_feedback_analyzer_with_sqlite() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        // This tests the branch where tracker is a real SqliteTracker
        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config: test_config(),
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        // Just verify the watcher was created successfully with feedback_analyzer initialized
        assert!(!watcher.is_running());
    }

    #[test]
    fn test_watcher_new_feedback_analyzer_without_sqlite() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // This tests the branch where tracker has default impl
        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config: test_config(),
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        assert!(!watcher.is_running());
    }

    // Additional coverage: record_source_decision / record_issue_decision
    // with various values
    #[test]
    fn test_record_source_decision_with_complex_details() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        watcher.record_source_decision(
            "linear",
            "poll_filtering_summary",
            "Summary of poll filtering for linear",
            json!({
                "fetched": 100,
                "matched": 50,
                "queued": 10,
                "deferred": 40,
                "skipped": {
                    "duplicate": 5,
                    "already_attempted": 30,
                    "inflight": 3,
                    "unmatched": 12,
                },
            }),
        );
    }

    #[test]
    fn test_record_issue_decision_with_metadata() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        let mut issue = test_issue();
        issue.set_metadata("resolved_user", "alice");

        watcher.record_issue_decision(
            &issue,
            "claude_run_succeeded_with_pr",
            format!("Claude produced PR for {}", issue.short_id),
            json!({
                "pr_url": "https://github.com/org/repo/pull/1",
                "attempt_id": 42,
                "used_qa_ids": [1, 2, 3],
            }),
        );
    }

    #[tokio::test]
    async fn test_mock_notifier_notify_closed_uses_default_impl() {
        let notifier = MockNotifier::new(true);
        let issue = test_issue();

        // notify_closed uses the default trait impl which calls notify_status
        let result = notifier
            .notify_closed(&issue, "https://github.com/org/repo/pull/1")
            .await;
        assert!(result.is_ok());
        // Default impl calls notify_status which increments call count
        assert_eq!(notifier.get_call_count(), 1);
    }

    #[tokio::test]
    async fn test_active_processing_for_source_empty_string() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        watcher.lock_processing().insert(":issue1".to_string());

        // Empty source name prefix ":" should match ":issue1"
        assert_eq!(watcher.active_processing_for_source(""), 1);
    }

    #[tokio::test]
    async fn test_poll_source_dry_run_does_not_record_batch_processed() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![Issue::new(
            "1",
            "T-1",
            "Issue 1",
            "http://example.com/1",
            "test",
        )];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source.clone()], true);

        watcher.poll_source(&source).await.unwrap();

        // Dry run returns early before recording batch_processed
        let batch = tracker.get_metrics("batch_processed", None, 10).unwrap();
        assert!(batch.is_empty());
    }

    #[tokio::test]
    async fn test_check_and_auto_close_prs_non_terminal_issue() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Issue is still Open (non-terminal)
        let issue = Issue::new(
            "open-1",
            "O-1",
            "Still open issue",
            "http://example.com/open/1",
            "mock",
        );

        let source = Arc::new(MockSource::with_issues("mock", vec![issue])) as Arc<dyn IssueSource>;

        tracker.record_attempt("mock", "open-1", "O-1").unwrap();
        tracker
            .mark_success("mock", "open-1", "https://github.com/org/repo/pull/1")
            .unwrap();

        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source], false);

        let result = watcher.check_and_auto_close_prs().await.unwrap();
        // Issue is still open, so no auto-close
        assert!(result.is_empty());

        // Attempt should still be Success
        let attempt = tracker.get_attempt("mock", "open-1").unwrap().unwrap();
        assert_eq!(attempt.status, FixAttemptStatus::Success);
    }

    #[tokio::test]
    async fn test_process_ready_retries_with_delay_between_items() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Create two failed attempts
        tracker.record_attempt("mock", "r1", "MOCK-R1").unwrap();
        tracker.mark_failed("mock", "r1", "failure 1").unwrap();
        tracker.record_attempt("mock", "r2", "MOCK-R2").unwrap();
        tracker.mark_failed("mock", "r2", "failure 2").unwrap();

        let source = Arc::new(MockSource::new("mock")) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;
        config.processing_delay_ms = 50; // Small delay

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));
        watcher.is_running.store(true, Ordering::SeqCst);

        let result = watcher.process_ready_retries().await;
        assert!(result.is_ok());

        // Both should have been retried (and failed because issue not in mock source)
        let a1 = tracker.get_attempt("mock", "r1").unwrap().unwrap();
        let a2 = tracker.get_attempt("mock", "r2").unwrap().unwrap();
        assert_eq!(a1.retry_count, 1);
        assert_eq!(a2.retry_count, 1);
    }

    #[tokio::test]
    async fn test_run_post_merge_learning_strategy_fingerprinting_enabled() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = sqlite.clone() as Arc<dyn FixAttemptTracker>;

        let mut config = test_config();
        config.learning.auto_extract_learnings = true;
        config.learning.diff_analysis = false;
        config.learning.quality_scoring = false;
        config.learning.auto_agent_md = false;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        sqlite.record_attempt("test", "1", "T-1").unwrap();
        let attempt = sqlite.get_attempt("test", "1").unwrap().unwrap();

        // Should not panic even with no executions in DB
        watcher.run_post_merge_learning(&attempt).await;
    }

    #[tokio::test]
    async fn test_poll_source_metric_consistency() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![
            Issue::new("1", "T-1", "Issue 1", "http://example.com/1", "test"),
            Issue::new("2", "T-2", "Issue 2", "http://example.com/2", "test"),
            Issue::new("3", "T-3", "Issue 3", "http://example.com/3", "test"),
        ];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source.clone()], true);

        watcher.poll_source(&source).await.unwrap();

        let fetched = tracker.get_metrics("issues_fetched", None, 10).unwrap();
        let matched = tracker.get_metrics("issues_matched", None, 10).unwrap();
        let queued = tracker.get_metrics("issues_queued", None, 10).unwrap();

        // All 3 issues fetched
        assert_eq!(fetched[0].metric_value, 3.0);
        // MockSource always matches, so all 3 matched
        assert_eq!(matched[0].metric_value, 3.0);
        // max_issues_per_cycle is 5 (default), so all 3 queued
        assert_eq!(queued[0].metric_value, 3.0);
    }

    fn create_test_watcher_with_sqlite(
        notifier: Arc<dyn Notifier>,
        tracker: Arc<SqliteTracker>,
        sources: Vec<Arc<dyn IssueSource>>,
    ) -> Arc<Watcher> {
        Arc::new(Watcher::new(WatcherOptions {
            config: test_config(),
            sources,
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }))
    }

    #[tokio::test]
    async fn test_active_processing_for_source() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        // Manually populate the processing set
        {
            let mut processing = watcher.lock_processing();
            processing.insert("source1:issue1".to_string());
            processing.insert("source1:issue2".to_string());
            processing.insert("source2:issue3".to_string());
        }

        assert_eq!(watcher.active_processing_for_source("source1"), 2);
        assert_eq!(watcher.active_processing_for_source("source2"), 1);
        assert_eq!(watcher.active_processing_for_source("source3"), 0);
    }

    #[test]
    fn test_record_source_decision() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let watcher = create_test_watcher_with_sqlite(notifier, sqlite.clone(), vec![]);

        watcher.record_source_decision(
            "test_source",
            "poll_complete",
            "Completed polling for test_source",
            json!({"fetched": 5, "matched": 3}),
        );

        // Verify activity was recorded to the tracker
        let activities = sqlite.get_recent_activities(10, None).unwrap();
        assert!(!activities.is_empty());
        let latest = &activities[0];
        assert_eq!(latest.activity_type, "decision");
        assert!(latest.message.contains("test_source"));
    }

    #[test]
    fn test_record_issue_decision() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let watcher = create_test_watcher_with_sqlite(notifier, sqlite.clone(), vec![]);

        let issue = test_issue();
        watcher.record_issue_decision(
            &issue,
            "issue_queued",
            "Issue TEST-123 queued for processing",
            json!({"priority": "normal", "match_reason": "label match"}),
        );

        let activities = sqlite.get_recent_activities(10, None).unwrap();
        assert!(!activities.is_empty());
        let latest = &activities[0];
        assert_eq!(latest.activity_type, "decision");
        assert!(latest.message.contains("TEST-123"));
        assert_eq!(latest.source.as_deref(), Some("test"));
        assert_eq!(latest.issue_id.as_deref(), Some("123"));
    }

    #[test]
    fn test_record_error_pattern() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let watcher = create_test_watcher_with_sqlite(notifier, sqlite.clone(), vec![]);

        crate::processing::record_error_pattern(
            &watcher.tracker,
            "linear",
            "ISSUE-42",
            "build failed: exit code 1",
        );
        crate::processing::record_error_pattern(
            &watcher.tracker,
            "sentry",
            "SENTRY-99",
            "timeout after 300s",
        );
        crate::processing::record_error_pattern(
            &watcher.tracker,
            "test",
            "T-1",
            "rate limit exceeded",
        );

        // Verify error patterns were stored
        let patterns = sqlite.get_error_patterns(10).unwrap();
        assert!(
            patterns.len() >= 2,
            "Expected at least 2 distinct error patterns, got {}",
            patterns.len()
        );
    }

    #[test]
    fn test_truncate_error_boundary_cases() {
        // Exactly 500 chars: no truncation
        let exactly_500 = "a".repeat(500);
        let result = crate::processing::truncate_error_for_activity(&exactly_500);
        assert_eq!(result.len(), 500);
        assert!(!result.ends_with("..."));

        // 501 chars: should truncate
        let chars_501 = "b".repeat(501);
        let result = crate::processing::truncate_error_for_activity(&chars_501);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 500);

        // Empty string
        let result = crate::processing::truncate_error_for_activity("");
        assert_eq!(result, "");

        // Multi-byte UTF-8 near boundary: 495 ASCII chars + some 4-byte emojis
        let mut multi_byte = "x".repeat(495);
        multi_byte.push_str("\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}");
        let result = crate::processing::truncate_error_for_activity(&multi_byte);
        assert!(result.ends_with("..."));
        assert!(result.is_char_boundary(result.len()));
        // Verify no panic, no split codepoint
        for ch in result.chars() {
            assert!(ch.len_utf8() >= 1);
        }

        // Very long string: 10000 chars
        let very_long = "z".repeat(10000);
        let result = crate::processing::truncate_error_for_activity(&very_long);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 500);
    }

    #[tokio::test]
    async fn test_notify_failed_with_escalation_hard_error() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let watcher = create_test_watcher_with_sqlite(notifier.clone(), sqlite.clone(), vec![]);

        let mut issue = test_issue();
        issue
            .metadata
            .insert("resolved_user".to_string(), json!("alice"));

        // "rate limit" is a hard error keyword
        let result = crate::processing::notify_failed_with_escalation(
            &watcher.notifier,
            &watcher.tracker,
            &issue,
            "rate limit exceeded: please slow down",
        )
        .await;
        assert!(result.is_ok());

        // Notifier should have been called once (via notify_failed)
        assert_eq!(notifier.get_call_count(), 1);

        // Verify the decision activity was recorded
        let activities = sqlite.get_recent_activities(10, None).unwrap();
        let escalation = activities
            .iter()
            .find(|a| a.activity_type == "decision" || a.activity_type == "error");
        assert!(
            escalation.is_some(),
            "Expected an escalation activity to be recorded"
        );
    }

    #[tokio::test]
    async fn test_notify_failed_with_escalation_soft_error() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let watcher = create_test_watcher_with_sqlite(notifier.clone(), sqlite.clone(), vec![]);

        let issue = test_issue();

        // A normal error message that is NOT a hard error
        let result: claudear_core::error::Result<()> =
            crate::processing::notify_failed_with_escalation(
                &watcher.notifier,
                &watcher.tracker,
                &issue,
                "compilation failed: missing semicolon",
            )
            .await;
        assert!(result.is_ok());

        // Notifier should have been called once
        assert_eq!(notifier.get_call_count(), 1);

        // No escalation activity should exist (soft errors skip the escalation path)
        let activities = sqlite.get_recent_activities(10, None).unwrap();
        let has_escalation = activities
            .iter()
            .any(|a| a.message.contains("Escalating hard error"));
        assert!(
            !has_escalation,
            "Soft error should not trigger escalation activity"
        );
    }

    #[test]
    fn test_watcher_new_with_tracker_coverage() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let watcher = create_test_watcher_with_sqlite(notifier, sqlite.clone(), vec![]);

        // Verify the Some(st) branch in the constructor was taken
        assert!(!watcher.dry_run);
        assert!(!watcher.is_running());
        assert_eq!(watcher.active_count(), 0);

        // Verify feedback_analyzer was initialized with sqlite
        // (it won't panic when used, which it would if incorrectly initialized)
        assert_eq!(watcher.sources.len(), 0);
    }

    #[test]
    fn test_sync_repos_to_db_no_inferrer() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let watcher = create_test_watcher_with_sqlite(notifier, sqlite, vec![]);

        // inferrer is None, should return 0
        let result = watcher.sync_repos_to_db(true).unwrap();
        assert_eq!(result, 0);

        let result = watcher.sync_repos_to_db(false).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn test_sync_repos_to_db_no_sqlite() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        // No inferrer, so sync returns 0
        let result = watcher.sync_repos_to_db(false).unwrap();
        assert_eq!(result, 0);
    }

    #[tokio::test]
    async fn test_refresh_repos_no_inferrer() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let watcher = create_test_watcher_with_sqlite(notifier, sqlite, vec![]);

        // No inferrer and no embedding_client => returns 0
        assert!(watcher.inferrer.is_none());
        assert!(watcher.embedding_client.is_none());
        let result = watcher.refresh_repos().await.unwrap();
        assert_eq!(result, 0);
    }

    #[tokio::test]
    async fn test_build_inferrer_no_known_orgs() {
        let mut config = test_config();
        config.known_orgs = vec![];

        let result = Watcher::build_inferrer(&config, None, None).await.unwrap();
        assert!(result.is_none(), "Expected None when known_orgs is empty");
    }

    #[tokio::test]
    async fn test_build_inferrer_no_discovery_method() {
        let mut config = test_config();
        config.known_orgs = vec!["some-org".to_string()];
        config.auto_discover_paths = vec![];
        // No github client passed

        let result = Watcher::build_inferrer(&config, None, None).await.unwrap();
        assert!(
            result.is_none(),
            "Expected None when no auto_discover_paths and no GitHub client"
        );
    }

    #[test]
    fn test_get_cascade_depth_no_parent() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let watcher = create_test_watcher_with_sqlite(notifier, sqlite, vec![]);

        let attempt = claudear_core::types::FixAttempt {
            id: 1,
            issue_id: "ISSUE-1".to_string(),
            short_id: "ISSUE-1".to_string(),
            source: "test".to_string(),
            attempted_at: chrono::Utc::now(),
            pr_url: None,
            scm_repo: None,
            scm_pr_number: None,
            status: FixAttemptStatus::Pending,
            error_message: None,
            merged_at: None,
            resolved_at: None,
            retry_count: 0,
            last_retry_at: None,
            issue_labels: vec![],
            parent_attempt_id: None,
            cascade_repo: None,
        };

        assert_eq!(watcher.get_cascade_depth(&attempt), 0);
    }

    #[tokio::test]
    async fn test_record_feedback_outcome_from_attempt() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let watcher = create_test_watcher_with_sqlite(notifier, sqlite.clone(), vec![]);

        // Record an attempt so we have one in the DB
        sqlite.record_attempt("test", "ISSUE-42", "T-42").unwrap();
        let attempt = sqlite.get_attempt("test", "ISSUE-42").unwrap().unwrap();

        // Should not panic and should create a minimal Issue internally
        watcher
            .record_feedback_outcome_from_attempt(&attempt, Outcome::Failed)
            .await;

        // Verify the feedback outcome was stored
        let outcome = sqlite.get_feedback_outcome_by_attempt(attempt.id);
        assert!(outcome.is_ok());
    }

    #[tokio::test]
    async fn test_run_periodic_learning_disabled() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));

        let mut config = test_config();
        config.learning.qa_promotion = false;
        config.learning.cluster_detection = false;
        config.learning.cross_repo_correlation = false;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: sqlite.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                sqlite.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        // Should complete instantly with all learning disabled
        watcher.run_periodic_learning().await;
        // No panic = success
    }

    #[tokio::test]
    async fn test_seed_empty_sources() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));
        let watcher = create_test_watcher_with_sqlite(notifier, sqlite, vec![]);

        let result = watcher.seed().await.unwrap();
        assert_eq!(result.total, 0);
        assert!(result.by_source.is_empty());
    }

    #[tokio::test]
    async fn test_seed_with_issues() {
        let sqlite = Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(MockNotifier::new(true));

        let issues = vec![
            Issue::new(
                "10",
                "S-10",
                "Seed Issue 1",
                "http://example.com/10",
                "seed_src",
            ),
            Issue::new(
                "11",
                "S-11",
                "Seed Issue 2",
                "http://example.com/11",
                "seed_src",
            ),
            Issue::new(
                "12",
                "S-12",
                "Seed Issue 3",
                "http://example.com/12",
                "seed_src",
            ),
        ];
        let source = Arc::new(MockSource::with_issues("seed_src", issues)) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher_with_sqlite(notifier, sqlite.clone(), vec![source]);

        let result = watcher.seed().await.unwrap();
        assert_eq!(result.total, 3);
        assert_eq!(*result.by_source.get("seed_src").unwrap(), 3);

        // Verify issues are tracked in the DB
        assert!(sqlite.has_attempted("seed_src", "10").unwrap());
        assert!(sqlite.has_attempted("seed_src", "11").unwrap());
        assert!(sqlite.has_attempted("seed_src", "12").unwrap());
    }

    #[test]
    fn test_watcher_accepts_non_claude_agent() {
        use claudear_integrations::runner::AgentRunner;

        struct MockAgent;

        #[async_trait]
        impl AgentRunner for MockAgent {
            fn name(&self) -> &str {
                "mock-agent"
            }
            fn capabilities(&self) -> claudear_integrations::runner::ProviderCapabilities {
                claudear_integrations::runner::ProviderCapabilities::default()
            }
            fn build_prompt_for_issue(
                &self,
                _issue: &Issue,
                _context: &str,
                _project_dir: &std::path::Path,
            ) -> String {
                "mock prompt".to_string()
            }
            async fn execute_with_attempt(
                &self,
                _prompt: &str,
                _issue: Option<&Issue>,
                _attempt_id: Option<i64>,
                _project_dir: &std::path::Path,
            ) -> claudear_core::error::Result<claudear_core::types::AgentResult> {
                Ok(claudear_core::types::AgentResult {
                    success: true,
                    output: "mock output".to_string(),
                    pr_url: None,
                    changelog: None,
                    error: None,
                    blocking_question: None,
                    used_qa_ids: Vec::new(),
                    confidence: 0,
                    confidence_reasoning: None,
                    wrong_repo: None,
                })
            }
        }

        // Create a Watcher with the mock agent to verify trait abstraction works
        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let mock_agent: Arc<dyn AgentRunner> = Arc::new(MockAgent);

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config: test_config(),
            sources: vec![],
            notifier: Arc::new(claudear_integrations::notifier::ConsoleNotifier),
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: true,
            llm_engine: None,
            agent: mock_agent,
        }));

        // Verify the watcher was created successfully with a non-Claude agent
        assert!(watcher.config.workspace.to_str().is_some());
    }

    #[test]
    fn test_watcher_with_orchestrator_agent() {
        use claudear_integrations::runner::{
            AgentOrchestrator, AgentRunner, SelectionStrategy, WeightedProvider,
        };

        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());

        // Create an orchestrator with a simple mock as the agent
        struct SimpleRunner;

        #[async_trait]
        impl AgentRunner for SimpleRunner {
            fn name(&self) -> &str {
                "simple"
            }
            fn capabilities(&self) -> claudear_integrations::runner::ProviderCapabilities {
                claudear_integrations::runner::ProviderCapabilities::default()
            }
            fn build_prompt_for_issue(&self, _: &Issue, _: &str, _: &std::path::Path) -> String {
                "simple prompt".to_string()
            }
            async fn execute_with_attempt(
                &self,
                _: &str,
                _: Option<&Issue>,
                _: Option<i64>,
                _: &std::path::Path,
            ) -> claudear_core::error::Result<claudear_core::types::AgentResult> {
                Ok(claudear_core::types::AgentResult {
                    success: true,
                    output: String::new(),
                    pr_url: None,
                    changelog: None,
                    error: None,
                    blocking_question: None,
                    used_qa_ids: Vec::new(),
                    confidence: 0,
                    confidence_reasoning: None,
                    wrong_repo: None,
                })
            }
        }

        let orchestrator = AgentOrchestrator::new(
            vec![WeightedProvider {
                provider: Arc::new(SimpleRunner),
                weight: 1.0,
            }],
            SelectionStrategy::Primary,
            Some("test-experiment".to_string()),
        );

        let agent: Arc<dyn AgentRunner> = Arc::new(orchestrator);

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config: test_config(),
            sources: vec![],
            notifier: Arc::new(claudear_integrations::notifier::ConsoleNotifier),
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: true,
            llm_engine: None,
            agent,
        }));

        assert!(watcher.config.workspace.to_str().is_some());
    }

    #[test]
    fn test_source_from_processing_key_with_colon() {
        assert_eq!(source_from_processing_key("sentry:ISSUE-42"), "sentry");
    }

    #[test]
    fn test_source_from_processing_key_without_colon() {
        // When there's no colon, the whole key is the source
        assert_eq!(source_from_processing_key("no_colon_here"), "no_colon_here");
    }

    #[test]
    fn test_source_from_processing_key_empty() {
        assert_eq!(source_from_processing_key(""), "");
    }

    #[test]
    fn test_source_from_processing_key_colon_at_start() {
        assert_eq!(source_from_processing_key(":issue-1"), "");
    }

    #[test]
    fn test_source_from_processing_key_multiple_colons() {
        // split_once only splits on the first colon
        assert_eq!(source_from_processing_key("a:b:c"), "a");
    }

    #[test]
    fn test_source_from_processing_key_colon_at_end() {
        assert_eq!(source_from_processing_key("source:"), "source");
    }

    #[test]
    fn test_processing_state_new() {
        let state = ProcessingState::new();
        assert!(state.is_empty());
        assert_eq!(state.len(), 0);
    }

    #[test]
    fn test_processing_state_insert_returns_true_for_new() {
        let mut state = ProcessingState::new();
        assert!(state.insert("sentry:123".to_string()));
    }

    #[test]
    fn test_processing_state_insert_returns_false_for_duplicate() {
        let mut state = ProcessingState::new();
        assert!(state.insert("sentry:123".to_string()));
        assert!(!state.insert("sentry:123".to_string()));
    }

    #[test]
    fn test_processing_state_len_and_is_empty() {
        let mut state = ProcessingState::new();
        assert!(state.is_empty());
        assert_eq!(state.len(), 0);

        state.insert("a:1".to_string());
        assert!(!state.is_empty());
        assert_eq!(state.len(), 1);

        state.insert("a:2".to_string());
        assert_eq!(state.len(), 2);

        state.insert("b:1".to_string());
        assert_eq!(state.len(), 3);
    }

    #[test]
    fn test_processing_state_contains() {
        let mut state = ProcessingState::new();
        assert!(!state.contains("x:1"));
        state.insert("x:1".to_string());
        assert!(state.contains("x:1"));
        assert!(!state.contains("x:2"));
    }

    #[test]
    fn test_processing_state_source_count() {
        let mut state = ProcessingState::new();
        assert_eq!(state.source_count("sentry"), 0);

        state.insert("sentry:1".to_string());
        assert_eq!(state.source_count("sentry"), 1);

        state.insert("sentry:2".to_string());
        assert_eq!(state.source_count("sentry"), 2);

        state.insert("linear:1".to_string());
        assert_eq!(state.source_count("sentry"), 2);
        assert_eq!(state.source_count("linear"), 1);
    }

    #[test]
    fn test_processing_state_insert_qa_counts_only_qa_lane() {
        // A QA insert increments the QA lane only, leaving the fix lane at zero.
        let mut state = ProcessingState::new();
        assert!(state.insert_qa("discord:1".to_string()));
        assert_eq!(state.qa_source_count("discord"), 1);
        assert_eq!(state.source_count("discord"), 0);
        assert!(state.contains("discord:1"));
    }

    #[test]
    fn test_processing_state_lanes_are_independent() {
        // Fixes and questions for the same source count in separate lanes.
        let mut state = ProcessingState::new();
        state.insert("discord:fix1".to_string());
        state.insert("discord:fix2".to_string());
        state.insert_qa("discord:q1".to_string());

        assert_eq!(state.source_count("discord"), 2);
        assert_eq!(state.qa_source_count("discord"), 1);
    }

    #[test]
    fn test_processing_state_remove_routes_to_correct_lane() {
        let mut state = ProcessingState::new();
        state.insert("discord:fix1".to_string());
        state.insert_qa("discord:q1".to_string());
        assert_eq!(state.source_count("discord"), 1);
        assert_eq!(state.qa_source_count("discord"), 1);

        // Removing the QA key decrements only the QA lane.
        assert!(state.remove("discord:q1"));
        assert_eq!(state.qa_source_count("discord"), 0);
        assert_eq!(state.source_count("discord"), 1);

        // Removing the fix key decrements only the fix lane.
        assert!(state.remove("discord:fix1"));
        assert_eq!(state.source_count("discord"), 0);
        assert_eq!(state.qa_source_count("discord"), 0);
    }

    #[test]
    fn test_processing_state_insert_qa_duplicate_returns_false() {
        let mut state = ProcessingState::new();
        assert!(state.insert_qa("discord:1".to_string()));
        assert!(!state.insert_qa("discord:1".to_string()));
        assert_eq!(state.qa_source_count("discord"), 1);
    }

    #[test]
    fn test_processing_state_remove_returns_true_when_present() {
        let mut state = ProcessingState::new();
        state.insert("sentry:1".to_string());
        assert!(state.remove("sentry:1"));
    }

    #[test]
    fn test_processing_state_remove_returns_false_when_absent() {
        let mut state = ProcessingState::new();
        assert!(!state.remove("nonexistent:1"));
    }

    #[test]
    fn test_processing_state_remove_decrements_source_count() {
        let mut state = ProcessingState::new();
        state.insert("sentry:1".to_string());
        state.insert("sentry:2".to_string());
        assert_eq!(state.source_count("sentry"), 2);

        state.remove("sentry:1");
        assert_eq!(state.source_count("sentry"), 1);

        state.remove("sentry:2");
        assert_eq!(state.source_count("sentry"), 0);
    }

    #[test]
    fn test_processing_state_remove_cleans_up_zero_count() {
        let mut state = ProcessingState::new();
        state.insert("src:1".to_string());
        state.remove("src:1");
        // After removing the last key for a source, source_count returns 0
        assert_eq!(state.source_count("src"), 0);
        assert!(state.is_empty());
    }

    #[test]
    fn test_processing_state_insert_remove_reinsert() {
        let mut state = ProcessingState::new();
        state.insert("a:1".to_string());
        state.remove("a:1");
        assert_eq!(state.source_count("a"), 0);

        // Re-insert should work
        assert!(state.insert("a:1".to_string()));
        assert_eq!(state.source_count("a"), 1);
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn test_processing_state_key_without_colon() {
        let mut state = ProcessingState::new();
        state.insert("nocolon".to_string());
        // The entire key is treated as the source name
        assert_eq!(state.source_count("nocolon"), 1);
        assert!(state.contains("nocolon"));
    }

    #[test]
    fn test_is_dry_run_true() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], true);
        assert!(watcher.is_dry_run());
    }

    #[test]
    fn test_is_dry_run_false() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);
        assert!(!watcher.is_dry_run());
    }

    #[test]
    fn test_set_running_true() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);
        assert!(!watcher.is_running());
        watcher.set_running(true);
        assert!(watcher.is_running());
    }

    #[test]
    fn test_set_running_false() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);
        watcher.set_running(true);
        watcher.set_running(false);
        assert!(!watcher.is_running());
    }

    #[test]
    fn test_reindex_interval_disabled() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.code_index.enabled = false;
        config.code_index.reindex_interval_hours = 6.0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        assert!(watcher.reindex_interval().is_none());
    }

    #[test]
    fn test_reindex_interval_zero_hours() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.code_index.enabled = true;
        config.code_index.reindex_interval_hours = 0.0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        assert!(watcher.reindex_interval().is_none());
    }

    #[test]
    fn test_reindex_interval_negative_hours() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.code_index.enabled = true;
        config.code_index.reindex_interval_hours = -1.0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        assert!(watcher.reindex_interval().is_none());
    }

    #[test]
    fn test_reindex_interval_enabled_with_hours() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.code_index.enabled = true;
        config.code_index.reindex_interval_hours = 2.0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let interval = watcher.reindex_interval().unwrap();
        assert_eq!(interval, std::time::Duration::from_secs(7200));
    }

    #[test]
    fn test_reindex_interval_fractional_hours() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.code_index.enabled = true;
        config.code_index.reindex_interval_hours = 0.5; // 30 minutes

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let interval = watcher.reindex_interval().unwrap();
        assert_eq!(interval, std::time::Duration::from_secs(1800));
    }

    #[test]
    fn test_extract_rate_limit_reset_from_banner_utc_pm() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "You've hit your limit · resets 3pm (UTC)";
        let parsed = Watcher::extract_rate_limit_reset_from_banner_utc(msg, now).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-02-23T15:00:00+00:00");
    }

    #[test]
    fn test_extract_rate_limit_reset_from_banner_utc_12am() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "You've hit your limit · resets 12am (UTC)";
        let parsed = Watcher::extract_rate_limit_reset_from_banner_utc(msg, now).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-02-24T00:00:00+00:00");
    }

    #[test]
    fn test_extract_rate_limit_reset_from_banner_utc_12pm() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "You've hit your limit · resets 12pm (UTC)";
        let parsed = Watcher::extract_rate_limit_reset_from_banner_utc(msg, now).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-02-23T12:00:00+00:00");
    }

    #[test]
    fn test_extract_rate_limit_reset_from_banner_utc_with_minutes() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "You've hit your limit · resets 6:30am (UTC)";
        let parsed = Watcher::extract_rate_limit_reset_from_banner_utc(msg, now).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-02-23T06:30:00+00:00");
    }

    #[test]
    fn test_extract_rate_limit_reset_from_banner_utc_missing_utc() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // No (UTC) at end
        let msg = "You've hit your limit · resets 6am";
        let parsed = Watcher::extract_rate_limit_reset_from_banner_utc(msg, now);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_extract_rate_limit_reset_from_banner_utc_invalid_hour() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // Hour 0 is invalid in 12-hour format
        let msg = "You've hit your limit · resets 0am (UTC)";
        let parsed = Watcher::extract_rate_limit_reset_from_banner_utc(msg, now);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_extract_rate_limit_reset_from_banner_utc_hour_13() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "You've hit your limit · resets 13am (UTC)";
        let parsed = Watcher::extract_rate_limit_reset_from_banner_utc(msg, now);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_extract_rate_limit_reset_from_banner_utc_minute_60() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "You've hit your limit · resets 6:60am (UTC)";
        let parsed = Watcher::extract_rate_limit_reset_from_banner_utc(msg, now);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_extract_rate_limit_reset_from_banner_utc_no_resets_keyword() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "You've hit your limit at 6am (UTC)";
        let parsed = Watcher::extract_rate_limit_reset_from_banner_utc(msg, now);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_extract_rate_limit_reset_from_retry_after() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "429 Too Many Requests. Retry-After: 120";
        let parsed = Watcher::extract_rate_limit_reset_from_retry_after(msg, now).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-02-23T10:02:00+00:00");
    }

    #[test]
    fn test_extract_rate_limit_reset_from_retry_after_zero_seconds() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // retry-after 0 is invalid (seconds <= 0)
        let msg = "Retry-After: 0";
        let parsed = Watcher::extract_rate_limit_reset_from_retry_after(msg, now);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_extract_rate_limit_reset_from_retry_after_no_digits() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "Retry-After: soon";
        let parsed = Watcher::extract_rate_limit_reset_from_retry_after(msg, now);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_extract_rate_limit_reset_from_retry_after_missing_keyword() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "Wait 120 seconds";
        let parsed = Watcher::extract_rate_limit_reset_from_retry_after(msg, now);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_extract_rate_limit_reset_time_prefers_resets_at() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // Has both resetsAt JSON and retry-after header; should prefer resetsAt
        let msg = r#"{"resetsAt": "2026-02-23T12:00:00Z"} Retry-After: 120"#;
        let parsed = Watcher::extract_rate_limit_reset_time(msg, now).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-02-23T12:00:00+00:00");
    }

    #[test]
    fn test_extract_rate_limit_reset_time_falls_back_to_banner() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "You've hit your limit · resets 6am (UTC)";
        let parsed = Watcher::extract_rate_limit_reset_time(msg, now).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-02-23T06:00:00+00:00");
    }

    #[test]
    fn test_extract_rate_limit_reset_time_falls_back_to_retry_after() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "Rate limited. Retry-After: 60";
        let parsed = Watcher::extract_rate_limit_reset_time(msg, now).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-02-23T10:01:00+00:00");
    }

    #[test]
    fn test_extract_rate_limit_reset_time_none_when_no_match() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-23T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = "Some random error with no rate limit info";
        let parsed = Watcher::extract_rate_limit_reset_time(msg, now);
        assert!(parsed.is_none());
    }

    #[tokio::test]
    async fn test_is_rate_limit_paused_when_not_paused() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);
        assert!(!watcher.is_rate_limit_paused().await);
    }

    #[tokio::test]
    async fn test_is_rate_limit_paused_when_paused_in_future() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        // Set pause until far in the future
        {
            let mut pauses = watcher.rate_limit_pause_until.write().await;
            pauses.insert(
                "claude".to_string(),
                Utc::now() + chrono::Duration::hours(1),
            );
        }

        assert!(watcher.is_rate_limit_paused().await);
    }

    #[tokio::test]
    async fn test_is_rate_limit_paused_clears_expired() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        // Set pause until the past
        {
            let mut pauses = watcher.rate_limit_pause_until.write().await;
            pauses.insert(
                "claude".to_string(),
                Utc::now() - chrono::Duration::seconds(10),
            );
        }

        // Should return false and clear the expired pause
        assert!(!watcher.is_rate_limit_paused().await);

        // Verify it was cleared
        let pauses = watcher.rate_limit_pause_until.read().await;
        assert!(pauses.is_empty());
    }

    #[tokio::test]
    async fn test_clear_rate_limit_pause_clears_value() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        {
            let mut pauses = watcher.rate_limit_pause_until.write().await;
            pauses.insert(
                "claude".to_string(),
                Utc::now() + chrono::Duration::hours(1),
            );
        }

        watcher.clear_rate_limit_pause().await;

        let pauses = watcher.rate_limit_pause_until.read().await;
        assert!(pauses.is_empty());
    }

    #[tokio::test]
    async fn test_clear_rate_limit_pause_noop_when_not_set() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        // Should not panic when nothing to clear
        watcher.clear_rate_limit_pause().await;

        let pauses = watcher.rate_limit_pause_until.read().await;
        assert!(pauses.is_empty());
    }

    #[tokio::test]
    async fn test_pause_until_rate_limit_reset_sets_pause() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        let issue = test_issue();
        let error = r#"{"resetsAt": "2026-12-31T23:59:59Z"}"#;

        let result = watcher.pause_until_rate_limit_reset(&issue, error).await;
        assert!(result.is_some());

        let pauses = watcher.rate_limit_pause_until.read().await;
        assert!(pauses.contains_key("claude"));
    }

    #[tokio::test]
    async fn test_pause_until_rate_limit_reset_fallback_when_no_parse() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        let issue = test_issue();
        // Error with no parseable time info
        let error = "rate limit hit, no timing info";

        let result = watcher.pause_until_rate_limit_reset(&issue, error).await;
        assert!(result.is_some());

        // Fallback is 15 minutes + 1 minute buffer
        let pauses = watcher.rate_limit_pause_until.read().await;
        assert!(pauses.contains_key("claude"));
    }

    #[tokio::test]
    async fn test_pause_until_rate_limit_reset_does_not_lower_existing() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        // Set a pause until far in the future
        let far_future = Utc::now() + chrono::Duration::hours(24);
        {
            let mut pauses = watcher.rate_limit_pause_until.write().await;
            pauses.insert("claude".to_string(), far_future);
        }

        let issue = test_issue();
        // This would parse to a time much sooner
        let error = "Retry-After: 60";
        watcher.pause_until_rate_limit_reset(&issue, error).await;

        // The pause should NOT be lowered below the existing far_future value
        let pauses = watcher.rate_limit_pause_until.read().await;
        assert!(*pauses.get("claude").unwrap() >= far_future);
    }

    #[tokio::test]
    async fn test_check_releases_and_cascade_disabled() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.cascade.enabled = false;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let result = watcher.check_releases_and_cascade().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_check_releases_and_cascade_no_scm() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.cascade.enabled = true;
        config.cascade.rules = vec![claudear_config::config::CascadeRule {
            upstream: "org/lib".to_string(),
            downstream: "org/app".to_string(),
            trigger: claudear_config::config::CascadeTrigger::Release,
            version_update: true,
            target_branch: None,
            instructions: None,
        }];

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None,
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: None,
            github_client: None, // No GitHub client
            scm_provider: None,  // No SCM provider
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        let result = watcher.check_releases_and_cascade().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_discover_dependencies_no_inferrer() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);
        assert!(watcher.inferrer.is_none());
        // Should return early without panicking
        watcher.discover_dependencies().await;
    }

    #[tokio::test]
    async fn test_pull_and_reindex_all_repos_no_inferrer() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);
        assert!(watcher.inferrer.is_none());
        // Should return early without panicking
        watcher.pull_and_reindex_all_repos().await;
    }

    #[tokio::test]
    async fn test_reindex_repo_disabled() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.code_index.enabled = false;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        // Should return early without panicking
        watcher
            .reindex_repo("test-repo", std::path::Path::new("/tmp/test"))
            .await;
    }

    #[tokio::test]
    async fn test_reindex_repo_no_embedding_client() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut config = test_config();
        config.code_index.enabled = true;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
            inferrer: None,
            embedding_client: None, // No embedding client
            review_watcher: None,
            issue_embedding_service: None,
            code_search_service: None,
            discord_search_service: None,
            discord_index_orchestrator: None,
            relationships: None,
            github_client: None,
            scm_provider: None,
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));

        // Should return early without panicking
        watcher
            .reindex_repo("test-repo", std::path::Path::new("/tmp/test"))
            .await;
    }

    #[tokio::test]
    async fn test_build_inferrer_with_embeddings_no_known_orgs() {
        let mut config = test_config();
        config.known_orgs = vec![];

        let result = Watcher::build_inferrer_with_embeddings(&config, None, None)
            .await
            .unwrap();
        assert!(result.0.is_none());
        assert!(result.1.is_none());
    }

    #[tokio::test]
    async fn test_build_inferrer_with_embeddings_no_discovery() {
        let mut config = test_config();
        config.known_orgs = vec!["org".to_string()];
        config.auto_discover_paths = vec![];

        let result = Watcher::build_inferrer_with_embeddings(&config, None, None)
            .await
            .unwrap();
        assert!(result.0.is_none());
        assert!(result.1.is_none());
    }

    #[tokio::test]
    async fn test_poll_returns_early_when_rate_limited() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![], false);

        // Set pause until the future
        {
            let mut pauses = watcher.rate_limit_pause_until.write().await;
            pauses.insert(
                "claude".to_string(),
                Utc::now() + chrono::Duration::hours(1),
            );
        }

        let result = watcher.poll().await;
        assert!(result.is_ok());

        // No metrics should be recorded since we returned early
        let poll_cycle = tracker
            .get_metrics("poll_cycle_duration_secs", None, 10)
            .unwrap();
        assert!(poll_cycle.is_empty());
    }

    #[tokio::test]
    async fn test_run_housekeeping_cycle_runs_even_when_provider_rate_limited() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker.clone(), vec![], false);

        {
            let mut pauses = watcher.rate_limit_pause_until.write().await;
            pauses.insert(
                "claude".to_string(),
                Utc::now() + chrono::Duration::hours(1),
            );
        }

        let result = watcher.run_housekeeping_cycle().await;
        assert!(result.is_ok());

        // Housekeeping metrics SHOULD be recorded (housekeeping is not blocked by provider rate limits)
        let duration = tracker
            .get_metrics("housekeeping_cycle_duration_secs", None, 10)
            .unwrap();
        assert!(!duration.is_empty());
    }

    #[tokio::test]
    async fn test_poll_source_returns_early_when_rate_limited() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issues = vec![Issue::new(
            "1",
            "T-1",
            "Issue 1",
            "http://example.com/1",
            "test",
        )];
        let source = Arc::new(MockSource::with_issues("test", issues)) as Arc<dyn IssueSource>;

        let watcher = create_test_watcher(notifier, tracker.clone(), vec![source.clone()], false);

        {
            let mut pauses = watcher.rate_limit_pause_until.write().await;
            pauses.insert(
                "claude".to_string(),
                Utc::now() + chrono::Duration::hours(1),
            );
        }

        let result = watcher.poll_source(&source).await;
        assert!(result.is_ok());

        // No metrics recorded since we returned early
        let fetched = tracker.get_metrics("issues_fetched", None, 10).unwrap();
        assert!(fetched.is_empty());
    }

    #[tokio::test]
    async fn test_process_issue_is_paused_while_rate_limited() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        let issue = Issue::new("1", "T-1", "Test", "http://example.com", "mock");
        let source =
            Arc::new(MockSource::with_issues("mock", vec![issue.clone()])) as Arc<dyn IssueSource>;
        let watcher = create_test_watcher(notifier, tracker, vec![source.clone()], false);

        {
            let mut pauses = watcher.rate_limit_pause_until.write().await;
            pauses.insert(
                "claude".to_string(),
                Utc::now() + chrono::Duration::hours(1),
            );
        }

        let match_result = MatchResult::matched("Test", MatchPriority::Normal);
        let outcome = watcher
            .process_issue(source, issue, match_result, None, None, None, None)
            .await;
        assert_eq!(
            outcome,
            IssueRun::Paused,
            "process_issue should report an issue skipped while rate limited as paused"
        );
    }

    #[tokio::test]
    async fn test_process_ready_retries_skips_human_closed_pr() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());

        // Create a closed attempt that would be retried
        tracker
            .record_attempt("mock", "closed-1", "MOCK-C1")
            .unwrap();
        tracker
            .mark_success("mock", "closed-1", "https://github.com/org/repo/pull/1")
            .unwrap();
        tracker.mark_closed("mock", "closed-1").unwrap();

        let source = Arc::new(MockSource::new("mock")) as Arc<dyn IssueSource>;

        let mut config = test_config();
        config.retry.base_delay_ms = 0;
        config.retry.max_delay_ms = 0;
        config.processing_delay_ms = 0;

        let watcher = Arc::new(Watcher::new(WatcherOptions {
            config,
            sources: vec![source],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        }));
        watcher.is_running.store(true, Ordering::SeqCst);

        let result = watcher.process_ready_retries().await;
        assert!(result.is_ok());

        // A human closing the PR is a verdict on the fix, not a failure to retry
        let attempt = tracker.get_attempt("mock", "closed-1").unwrap().unwrap();
        assert_eq!(attempt.retry_count, 0);
        assert_eq!(
            attempt.status,
            claudear_core::types::FixAttemptStatus::Closed
        );
    }

    #[test]
    fn test_is_terminal_attempt_status_exhaustive() {
        let all_statuses = [
            FixAttemptStatus::Pending,
            FixAttemptStatus::Success,
            FixAttemptStatus::Failed,
            FixAttemptStatus::Merged,
            FixAttemptStatus::Closed,
            FixAttemptStatus::CannotFix,
            FixAttemptStatus::Answered,
            FixAttemptStatus::Declined,
        ];

        let terminal_count = all_statuses
            .iter()
            .filter(|s| Watcher::is_terminal_attempt_status(**s))
            .count();
        assert_eq!(terminal_count, 4, "Expected exactly 4 terminal statuses");

        let non_terminal_count = all_statuses
            .iter()
            .filter(|s| !Watcher::is_terminal_attempt_status(**s))
            .count();
        assert_eq!(
            non_terminal_count, 4,
            "Expected exactly 4 non-terminal statuses"
        );
    }

    #[tokio::test]
    async fn test_refresh_repos_no_embedding_client() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        // inferrer is None so it returns 0 before checking embedding_client
        let watcher = create_test_watcher(notifier, tracker, vec![], false);
        let result = watcher.refresh_repos().await.unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn test_processing_state_multiple_sources() {
        let mut state = ProcessingState::new();

        for i in 0..10 {
            state.insert(format!("sentry:{}", i));
        }
        for i in 0..5 {
            state.insert(format!("linear:{}", i));
        }
        for i in 0..3 {
            state.insert(format!("jira:{}", i));
        }

        assert_eq!(state.len(), 18);
        assert_eq!(state.source_count("sentry"), 10);
        assert_eq!(state.source_count("linear"), 5);
        assert_eq!(state.source_count("jira"), 3);
        assert_eq!(state.source_count("unknown"), 0);

        // Remove some from sentry
        for i in 0..5 {
            state.remove(&format!("sentry:{}", i));
        }

        assert_eq!(state.source_count("sentry"), 5);
        assert_eq!(state.len(), 13);
    }

    #[test]
    fn test_sort_by_priority_mixed_match_and_issue_priority() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_test_watcher(notifier, tracker, vec![], false);

        let mut issues = vec![
            (
                test_issue_with_priority("1", IssuePriority::Critical),
                MatchResult::matched("Normal match critical issue", MatchPriority::Normal),
            ),
            (
                test_issue_with_priority("2", IssuePriority::Low),
                MatchResult::matched("Urgent match low issue", MatchPriority::Urgent),
            ),
            (
                test_issue_with_priority("3", IssuePriority::High),
                MatchResult::matched("Normal match high issue", MatchPriority::Normal),
            ),
        ];

        watcher.sort_by_priority(&mut issues);

        // Urgent match comes first regardless of issue priority
        assert_eq!(issues[0].0.id, "2");
        assert_eq!(issues[0].1.priority, MatchPriority::Urgent);

        // Among Normal match, Critical comes before High
        assert_eq!(issues[1].0.id, "1");
        assert_eq!(issues[1].0.priority, IssuePriority::Critical);
        assert_eq!(issues[2].0.id, "3");
        assert_eq!(issues[2].0.priority, IssuePriority::High);
    }

    #[test]
    fn test_parse_approval_reply_multiple_punctuation() {
        assert_eq!(parse_approval_reply("yes!!!"), ApprovalDecision::Approved);
        assert_eq!(parse_approval_reply("no..."), ApprovalDecision::Denied);
        assert_eq!(
            parse_approval_reply("approve!!"),
            ApprovalDecision::Approved
        );
        assert_eq!(parse_approval_reply("reject??"), ApprovalDecision::Denied);
    }

    #[test]
    fn test_parse_approval_reply_mixed_case_with_punctuation() {
        assert_eq!(
            parse_approval_reply("Go Ahead!"),
            ApprovalDecision::Approved
        );
        assert_eq!(parse_approval_reply("PROCEED."), ApprovalDecision::Approved);
        assert_eq!(parse_approval_reply("NOPE!"), ApprovalDecision::Denied);
        assert_eq!(parse_approval_reply("Pass."), ApprovalDecision::Denied);
    }

    #[test]
    fn test_parse_approval_reply_only_whitespace() {
        assert_eq!(parse_approval_reply("   "), ApprovalDecision::Unrecognized);
        assert_eq!(parse_approval_reply("\t"), ApprovalDecision::Unrecognized);
        assert_eq!(parse_approval_reply("\n"), ApprovalDecision::Unrecognized);
    }

    #[test]
    fn test_parse_approval_reply_only_punctuation() {
        assert_eq!(parse_approval_reply("!!!"), ApprovalDecision::Unrecognized);
        assert_eq!(parse_approval_reply("..."), ApprovalDecision::Unrecognized);
        assert_eq!(parse_approval_reply("?"), ApprovalDecision::Unrecognized);
    }

    #[test]
    fn test_parse_approval_reply_partial_match_not_accepted() {
        // "yesss" is not "yes", "noo" is not "no"
        assert_eq!(
            parse_approval_reply("yesss"),
            ApprovalDecision::Unrecognized
        );
        assert_eq!(parse_approval_reply("noo"), ApprovalDecision::Unrecognized);
        assert_eq!(
            parse_approval_reply("approved"),
            ApprovalDecision::Unrecognized
        );
        assert_eq!(
            parse_approval_reply("rejected"),
            ApprovalDecision::Unrecognized
        );
        assert_eq!(
            parse_approval_reply("skipping"),
            ApprovalDecision::Unrecognized
        );
        assert_eq!(
            parse_approval_reply("okayy"),
            ApprovalDecision::Unrecognized
        );
    }

    #[test]
    fn test_parse_approval_reply_with_newlines() {
        // Newline after the word — trim handles leading/trailing whitespace
        assert_eq!(parse_approval_reply("yes\n"), ApprovalDecision::Approved);
        assert_eq!(parse_approval_reply("\nno\n"), ApprovalDecision::Denied);
    }

    #[test]
    fn test_parse_approval_reply_multiword_with_extra_spaces() {
        assert_eq!(
            parse_approval_reply("  go ahead  "),
            ApprovalDecision::Approved
        );
        // Extra internal spacing should NOT match
        assert_eq!(
            parse_approval_reply("go  ahead"),
            ApprovalDecision::Unrecognized
        );
    }

    #[test]
    fn test_parse_approval_reply_yes_variants() {
        for word in &[
            "yes", "y", "approve", "ok", "sure", "go ahead", "lgtm", "yep", "yeah", "proceed",
        ] {
            assert_eq!(
                parse_approval_reply(word),
                ApprovalDecision::Approved,
                "Expected Approved for {:?}",
                word
            );
        }
    }

    #[test]
    fn test_parse_approval_reply_no_variants() {
        for word in &[
            "no", "n", "skip", "deny", "reject", "nope", "nah", "stop", "pass",
        ] {
            assert_eq!(
                parse_approval_reply(word),
                ApprovalDecision::Denied,
                "Expected Denied for {:?}",
                word
            );
        }
    }

    #[test]
    fn test_parse_approval_reply_case_insensitive() {
        assert_eq!(parse_approval_reply("YES"), ApprovalDecision::Approved);
        assert_eq!(parse_approval_reply("Yes"), ApprovalDecision::Approved);
        assert_eq!(parse_approval_reply("LGTM"), ApprovalDecision::Approved);
        assert_eq!(parse_approval_reply("NO"), ApprovalDecision::Denied);
        assert_eq!(parse_approval_reply("Skip"), ApprovalDecision::Denied);
    }

    #[test]
    fn test_parse_approval_reply_with_whitespace_and_punctuation() {
        assert_eq!(parse_approval_reply("  yes  "), ApprovalDecision::Approved);
        assert_eq!(parse_approval_reply("no!"), ApprovalDecision::Denied);
        assert_eq!(
            parse_approval_reply("  approve. "),
            ApprovalDecision::Approved
        );
        assert_eq!(parse_approval_reply("reject!"), ApprovalDecision::Denied);
    }

    #[test]
    fn test_parse_approval_reply_unrecognized() {
        assert_eq!(
            parse_approval_reply("maybe"),
            ApprovalDecision::Unrecognized
        );
        assert_eq!(parse_approval_reply(""), ApprovalDecision::Unrecognized);
        assert_eq!(
            parse_approval_reply("I think so"),
            ApprovalDecision::Unrecognized
        );
        assert_eq!(
            parse_approval_reply("not sure"),
            ApprovalDecision::Unrecognized
        );
    }

    #[test]
    fn test_parse_approval_reply_redirect_use() {
        assert_eq!(
            parse_approval_reply("use org/other-repo"),
            ApprovalDecision::Redirect {
                repo_name: "org/other-repo".to_string()
            }
        );
    }

    #[test]
    fn test_parse_approval_reply_redirect_try() {
        assert_eq!(
            parse_approval_reply("try org/other-repo"),
            ApprovalDecision::Redirect {
                repo_name: "org/other-repo".to_string()
            }
        );
    }

    #[test]
    fn test_parse_approval_reply_redirect_to() {
        assert_eq!(
            parse_approval_reply("redirect to org/other-repo"),
            ApprovalDecision::Redirect {
                repo_name: "org/other-repo".to_string()
            }
        );
    }

    #[test]
    fn test_parse_approval_reply_redirect_case_insensitive() {
        assert_eq!(
            parse_approval_reply("Use Org/Repo"),
            ApprovalDecision::Redirect {
                repo_name: "org/repo".to_string()
            }
        );
    }

    #[test]
    fn test_parse_approval_reply_redirect_empty_repo_is_unrecognized() {
        // "use " with nothing after is unrecognized, not a redirect
        assert_eq!(parse_approval_reply("use "), ApprovalDecision::Unrecognized);
        assert_eq!(
            parse_approval_reply("use  "),
            ApprovalDecision::Unrecognized
        );
    }

    #[test]
    fn test_parse_approval_reply_bare_repo_is_unrecognized() {
        // A bare repo name without a prefix should NOT be treated as redirect
        assert_eq!(
            parse_approval_reply("org/repo"),
            ApprovalDecision::Unrecognized
        );
    }

    #[test]
    fn test_should_request_approval_require_approval_true() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier, tracker, true, None);
        let resolution = RepoResolution::Resolved {
            project_dir: std::path::PathBuf::from("/tmp/repo"),
            repo_name: "org/repo".to_string(),
            repo_id: None,
            scm_url: "https://github.com/org/repo".to_string(),
            default_branch: "main".to_string(),
            confidence: Some(Confidence::High),
        };
        assert!(watcher.should_request_approval(&resolution));
    }

    #[test]
    fn test_should_request_approval_threshold_triggers() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut watcher = create_approval_watcher(notifier, tracker, false, None);
        watcher.config.ask.approval_confidence_threshold = Some("low".to_string());

        // Low confidence should trigger (Low <= Low)
        let resolution = RepoResolution::Resolved {
            project_dir: std::path::PathBuf::from("/tmp/repo"),
            repo_name: "org/repo".to_string(),
            repo_id: None,
            scm_url: "https://github.com/org/repo".to_string(),
            default_branch: "main".to_string(),
            confidence: Some(Confidence::Low),
        };
        assert!(watcher.should_request_approval(&resolution));
    }

    #[test]
    fn test_should_request_approval_threshold_skips_high_confidence() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut watcher = create_approval_watcher(notifier, tracker, false, None);
        watcher.config.ask.approval_confidence_threshold = Some("low".to_string());

        // High confidence should NOT trigger (High > Low)
        let resolution = RepoResolution::Resolved {
            project_dir: std::path::PathBuf::from("/tmp/repo"),
            repo_name: "org/repo".to_string(),
            repo_id: None,
            scm_url: "https://github.com/org/repo".to_string(),
            default_branch: "main".to_string(),
            confidence: Some(Confidence::High),
        };
        assert!(!watcher.should_request_approval(&resolution));
    }

    #[test]
    fn test_should_request_approval_no_threshold_no_require() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier, tracker, false, None);
        let resolution = RepoResolution::Resolved {
            project_dir: std::path::PathBuf::from("/tmp/repo"),
            repo_name: "org/repo".to_string(),
            repo_id: None,
            scm_url: "https://github.com/org/repo".to_string(),
            default_branch: "main".to_string(),
            confidence: Some(Confidence::High),
        };
        assert!(!watcher.should_request_approval(&resolution));
    }

    #[test]
    fn test_should_request_approval_none_confidence_below_threshold() {
        let notifier = Arc::new(MockNotifier::new(true));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut watcher = create_approval_watcher(notifier, tracker, false, None);
        watcher.config.ask.approval_confidence_threshold = Some("low".to_string());

        // None confidence (direct lookup) should trigger (None <= Low)
        let resolution = RepoResolution::Resolved {
            project_dir: std::path::PathBuf::from("/tmp/repo"),
            repo_name: "org/repo".to_string(),
            repo_id: None,
            scm_url: "https://github.com/org/repo".to_string(),
            default_branch: "main".to_string(),
            confidence: None,
        };
        assert!(watcher.should_request_approval(&resolution));
    }

    #[test]
    fn test_confidence_ordering() {
        assert!(Confidence::None < Confidence::Low);
        assert!(Confidence::Low < Confidence::Medium);
        assert!(Confidence::Medium < Confidence::High);
    }

    #[test]
    fn test_confidence_from_str() {
        assert_eq!("high".parse::<Confidence>(), Ok(Confidence::High));
        assert_eq!("medium".parse::<Confidence>(), Ok(Confidence::Medium));
        assert_eq!("low".parse::<Confidence>(), Ok(Confidence::Low));
        assert_eq!("none".parse::<Confidence>(), Ok(Confidence::None));
        assert_eq!("HIGH".parse::<Confidence>(), Ok(Confidence::High));
        assert!("invalid".parse::<Confidence>().is_err());
    }

    use claudear_core::types::{AskDelivery, AskReply};
    use std::sync::Mutex;

    /// A mock notifier that supports replies and returns pre-configured answers.
    struct ApprovalMockNotifier {
        /// Pre-configured reply to return on poll, or None for timeout simulation.
        reply: Mutex<Option<String>>,
        /// Track how many times ask_question was called.
        ask_count: AtomicUsize,
    }

    impl ApprovalMockNotifier {
        fn with_reply(answer: &str) -> Self {
            Self {
                reply: Mutex::new(Some(answer.to_string())),
                ask_count: AtomicUsize::new(0),
            }
        }

        fn with_no_reply() -> Self {
            Self {
                reply: Mutex::new(None),
                ask_count: AtomicUsize::new(0),
            }
        }

        fn ask_count(&self) -> usize {
            self.ask_count.load(AtomicOrdering::SeqCst)
        }
    }

    #[async_trait]
    impl Notifier for ApprovalMockNotifier {
        fn name(&self) -> &str {
            "approval_mock"
        }
        fn is_enabled(&self) -> bool {
            true
        }
        async fn notify_start(&self, _issue: &Issue) -> Result<()> {
            Ok(())
        }
        async fn notify_success(&self, _issue: &Issue, _pr_url: &str) -> Result<()> {
            Ok(())
        }
        async fn notify_completed(&self, _issue: &Issue) -> Result<()> {
            Ok(())
        }
        async fn notify_failed(&self, _issue: &Issue, _error: &str) -> Result<()> {
            Ok(())
        }
        async fn notify_status(&self, _message: &str) -> Result<()> {
            Ok(())
        }
        async fn notify_urgent_issues(&self, _issues: &[Issue]) -> Result<()> {
            Ok(())
        }
        async fn ask_question(
            &self,
            _issue: &Issue,
            _request: &AskRequest,
        ) -> Result<Option<AskDelivery>> {
            self.ask_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(Some(AskDelivery {
                channel: "approval_mock".to_string(),
                target: None,
                message_id: Some("msg-1".to_string()),
            }))
        }

        async fn poll_question_replies(
            &self,
            request: &AskRequest,
            _since: DateTime<Utc>,
        ) -> Result<Vec<AskReply>> {
            let reply = self.reply.lock().unwrap();
            match reply.as_ref() {
                Some(answer) => Ok(vec![AskReply {
                    correlation_id: request.correlation_id.clone(),
                    channel: "approval_mock".to_string(),
                    responder: Some("test-user".to_string()),
                    answer: answer.clone(),
                    replied_at: Utc::now(),
                }]),
                None => Ok(vec![]),
            }
        }

        fn supports_replies(&self) -> bool {
            true
        }
    }

    fn create_approval_watcher(
        notifier: Arc<dyn Notifier>,
        tracker: Arc<SqliteTracker>,
        require_approval: bool,
        approval_timeout_secs: Option<u64>,
    ) -> Watcher {
        let mut config = test_config();
        config.ask.require_approval = require_approval;
        config.ask.approval_timeout_secs = approval_timeout_secs;
        // Use short timeouts for tests
        config.ask.wait_timeout_secs = 2;
        config.ask.poll_interval_secs = 1;
        Watcher::new(WatcherOptions {
            config,
            sources: vec![],
            notifier,
            tracker: tracker.clone(),
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
            user_registry: UserRegistry::new(std::collections::HashMap::new()),
            agent: Arc::new(claudear_integrations::runner::ClaudeAgentRunner::new(
                claudear_integrations::runner::ClaudeRunnerConfig::default(),
                tracker.clone(),
            )),
            classification_agent: None,
            repo_classification_agent: None,
            qa_agent: None,
            dry_run: false,
            llm_engine: None,
        })
    }

    fn test_resolution() -> RepoResolution {
        RepoResolution::Resolved {
            project_dir: std::path::PathBuf::from("/tmp/repo"),
            repo_name: "org/repo".to_string(),
            repo_id: None,
            scm_url: "https://github.com/org/repo".to_string(),
            default_branch: "main".to_string(),
            confidence: Some(Confidence::Medium),
        }
    }

    #[tokio::test]
    async fn test_request_approval_yes_reply() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("yes"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier.clone(), tracker.clone(), true, None);

        let issue = test_issue();
        let resolution = test_resolution();
        let decision = watcher.request_approval("test", &issue, &resolution).await;

        assert_eq!(decision, ApprovalDecision::Approved);
        assert_eq!(notifier.ask_count(), 1);

        // Verify activity was logged
        let activities = tracker.get_recent_activities(10, None).unwrap();
        let decisions: Vec<_> = activities
            .iter()
            .filter(|a| a.activity_type == "decision")
            .collect();
        assert!(
            decisions.iter().any(|a| a.message.contains("granted")),
            "Expected approval_granted decision in activities"
        );
    }

    #[tokio::test]
    async fn test_request_approval_no_reply() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("no"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier.clone(), tracker.clone(), true, None);

        let issue = test_issue();
        let resolution = test_resolution();
        let decision = watcher.request_approval("test", &issue, &resolution).await;

        assert_eq!(decision, ApprovalDecision::Denied);
        assert_eq!(notifier.ask_count(), 1);

        let activities = tracker.get_recent_activities(10, None).unwrap();
        let decisions: Vec<_> = activities
            .iter()
            .filter(|a| a.activity_type == "decision")
            .collect();
        assert!(
            decisions.iter().any(|a| a.message.contains("denied")),
            "Expected approval_denied decision in activities"
        );
    }

    #[tokio::test]
    async fn test_request_approval_approve_variant() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("approve"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier.clone(), tracker.clone(), true, None);

        let issue = test_issue();
        let resolution = test_resolution();
        assert_eq!(
            watcher.request_approval("test", &issue, &resolution).await,
            ApprovalDecision::Approved
        );
    }

    #[tokio::test]
    async fn test_request_approval_skip_variant() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("skip"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier.clone(), tracker.clone(), true, None);

        let issue = test_issue();
        let resolution = test_resolution();
        assert_eq!(
            watcher.request_approval("test", &issue, &resolution).await,
            ApprovalDecision::Denied
        );
    }

    #[tokio::test]
    async fn test_request_approval_unrecognized_reply_denies() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("maybe later"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier.clone(), tracker.clone(), true, None);

        let issue = test_issue();
        let resolution = test_resolution();
        let decision = watcher.request_approval("test", &issue, &resolution).await;

        assert_eq!(
            decision,
            ApprovalDecision::Unrecognized,
            "Unrecognized reply should return Unrecognized"
        );
    }

    #[tokio::test]
    async fn test_request_approval_timeout_is_unanswered() {
        let notifier = Arc::new(ApprovalMockNotifier::with_no_reply());
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier.clone(), tracker.clone(), true, Some(1));

        let issue = test_issue();
        let resolution = test_resolution();
        let decision = watcher.request_approval("test", &issue, &resolution).await;

        assert_eq!(
            decision,
            ApprovalDecision::Unanswered,
            "a timeout means no one answered, which is not a refusal"
        );
    }

    #[tokio::test]
    async fn test_request_approval_uses_custom_timeout() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("yes"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier.clone(), tracker.clone(), true, Some(60));

        let issue = test_issue();
        let resolution = test_resolution();
        // Should still approve since mock replies immediately
        assert_eq!(
            watcher.request_approval("test", &issue, &resolution).await,
            ApprovalDecision::Approved
        );
    }

    #[tokio::test]
    async fn test_request_approval_logs_approval_requested_activity() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("yes"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier.clone(), tracker.clone(), true, None);

        let issue = test_issue();
        let resolution = test_resolution();
        watcher.request_approval("test", &issue, &resolution).await;

        let activities = tracker.get_recent_activities(10, None).unwrap();
        assert!(
            activities
                .iter()
                .any(|a| a.activity_type == "approval_requested"),
            "Expected approval_requested activity to be logged"
        );
    }

    #[tokio::test]
    async fn test_request_approval_lgtm_variant() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("LGTM"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier.clone(), tracker.clone(), true, None);

        let issue = test_issue();
        let resolution = test_resolution();
        assert_eq!(
            watcher.request_approval("test", &issue, &resolution).await,
            ApprovalDecision::Approved
        );
    }

    #[tokio::test]
    async fn test_request_approval_reject_variant() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("reject"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier.clone(), tracker.clone(), true, None);

        let issue = test_issue();
        let resolution = test_resolution();
        assert_eq!(
            watcher.request_approval("test", &issue, &resolution).await,
            ApprovalDecision::Denied
        );
    }

    #[tokio::test]
    async fn test_request_approval_redirect_variant() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("use org/other-repo"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = create_approval_watcher(notifier.clone(), tracker.clone(), true, None);

        let issue = test_issue();
        let resolution = test_resolution();
        let decision = watcher.request_approval("test", &issue, &resolution).await;
        assert_eq!(
            decision,
            ApprovalDecision::Redirect {
                repo_name: "org/other-repo".to_string()
            }
        );
    }

    /// How long the orphan-sweep tests let a run stay silent before a sweep
    /// treats its attempt as orphaned: a few seconds, as attempt timestamps
    /// only resolve whole seconds, and enough that a stalled test runtime is
    /// not mistaken for a dead run.
    const ORPHAN_SWEEP_WINDOW: Duration = Duration::from_secs(5);

    /// How often the orphan-sweep tests' runs send heartbeats: many times
    /// within [`ORPHAN_SWEEP_WINDOW`].
    const TEST_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);

    /// How the orphan-sweep tests' runs send heartbeats and how long their
    /// sweeps let a run stay silent.
    const TEST_LIVENESS: Liveness = Liveness {
        interval: TEST_HEARTBEAT_INTERVAL,
        stale_after: ORPHAN_SWEEP_WINDOW,
    };

    /// A watcher that asks for approval before every run, waits up to a
    /// minute for the answer, and sends heartbeats and sweeps orphans on
    /// test-sized timing.
    fn approval_gated_watcher(
        notifier: Arc<ApprovalMockNotifier>,
        tracker: Arc<SqliteTracker>,
    ) -> Arc<Watcher> {
        let mut watcher = create_approval_watcher(notifier, tracker, true, Some(60));
        watcher.liveness = TEST_LIVENESS;
        Arc::new(watcher)
    }

    /// An [`approval_gated_watcher`] that runs over `source` and retries a
    /// failed attempt as soon as its retry manager next runs.
    fn retrying_approval_gated_watcher(
        notifier: Arc<ApprovalMockNotifier>,
        tracker: Arc<SqliteTracker>,
        source: Arc<dyn IssueSource>,
    ) -> Arc<Watcher> {
        let mut watcher = create_approval_watcher(notifier, tracker, true, Some(60));
        watcher.liveness = TEST_LIVENESS;
        watcher.sources = vec![source];
        watcher.config.retry.base_delay_ms = 0;
        watcher.config.retry.max_delay_ms = 0;
        watcher.config.processing_delay_ms = 0;
        watcher.set_running(true);
        Arc::new(watcher)
    }

    /// How many times `decision` was recorded on `issue`'s timeline.
    fn decisions_recorded(tracker: &SqliteTracker, issue: &Issue, decision: &str) -> usize {
        tracker
            .get_activities_for_issue(&issue.source, &issue.id)
            .unwrap()
            .into_iter()
            .filter(|activity| activity.activity_type == "decision")
            .filter_map(|activity| activity.metadata)
            .filter(|metadata| metadata["decision"] == decision)
            .count()
    }

    /// Process `issue` in the background until its run stops to wait for
    /// approval, by when its attempt is recorded.
    async fn start_run_awaiting_approval(
        watcher: &Arc<Watcher>,
        notifier: &ApprovalMockNotifier,
        issue: Issue,
    ) -> tokio::task::JoinHandle<IssueRun> {
        let source = Arc::new(MockSource::with_issues(&issue.source, vec![issue.clone()]))
            as Arc<dyn IssueSource>;
        let run = {
            let watcher = Arc::clone(watcher);
            tokio::spawn(async move {
                watcher
                    .process_issue(
                        source,
                        issue,
                        MatchResult::matched("Mock match", MatchPriority::Normal),
                        None,
                        None,
                        None,
                        None,
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while notifier.ask_count() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the run should stop to ask for approval");
        run
    }

    #[tokio::test]
    async fn test_attempt_awaiting_approval_survives_orphan_sweeps() {
        let notifier = Arc::new(ApprovalMockNotifier::with_no_reply());
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = approval_gated_watcher(notifier.clone(), tracker.clone());
        let issue = test_issue();
        let run = start_run_awaiting_approval(&watcher, &notifier, issue.clone()).await;

        tokio::time::sleep(ORPHAN_SWEEP_WINDOW + Duration::from_secs(1)).await;
        tracker
            .release_orphaned_pending_attempts(ORPHAN_SWEEP_WINDOW)
            .unwrap();
        watcher.release_orphaned_attempts();
        let attempt = tracker
            .get_attempt(&issue.source, &issue.id)
            .unwrap()
            .unwrap();

        *notifier.reply.lock().unwrap() = Some("no".to_string());
        let finished = tokio::time::timeout(Duration::from_secs(10), run).await;
        assert_eq!(
            attempt.status,
            FixAttemptStatus::Pending,
            "a run waiting on approval is live, so no sweep, in its own process or another, \
             may release its attempt, however far the wait outlasts the sweep window"
        );
        assert!(
            finished.is_ok(),
            "the run should end once approval is refused"
        );
    }

    #[tokio::test]
    async fn test_attempt_of_an_aborted_run_is_released_once_its_heartbeat_goes_stale() {
        let notifier = Arc::new(ApprovalMockNotifier::with_no_reply());
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let watcher = approval_gated_watcher(notifier.clone(), tracker.clone());
        let issue = test_issue();
        let run = start_run_awaiting_approval(&watcher, &notifier, issue.clone()).await;

        run.abort();
        assert!(
            run.await.is_err_and(|error| error.is_cancelled()),
            "the run should be aborted while it waits"
        );
        tokio::time::sleep(ORPHAN_SWEEP_WINDOW + Duration::from_secs(2)).await;
        watcher.release_orphaned_attempts();

        assert_eq!(
            tracker
                .get_attempt(&issue.source, &issue.id)
                .unwrap()
                .unwrap()
                .status,
            FixAttemptStatus::Failed,
            "a run that died stops showing it is alive, so the watcher's next sweep past the \
             window must hand its attempt back to the retry queue rather than leave the issue \
             blocked"
        );
    }

    #[tokio::test]
    async fn test_denied_approval_is_not_asked_again_after_orphan_sweeps_and_retries() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("no"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let issue = test_issue();
        let source = Arc::new(MockSource::with_issues(&issue.source, vec![issue.clone()]))
            as Arc<dyn IssueSource>;
        let watcher =
            retrying_approval_gated_watcher(notifier.clone(), tracker.clone(), source.clone());

        watcher
            .process_issue(
                source,
                issue.clone(),
                MatchResult::matched("Mock match", MatchPriority::Normal),
                None,
                None,
                None,
                None,
            )
            .await;
        let declined = tracker
            .get_attempt(&issue.source, &issue.id)
            .unwrap()
            .unwrap();
        tokio::time::sleep(ORPHAN_SWEEP_WINDOW + Duration::from_secs(2)).await;
        watcher.release_orphaned_attempts();
        watcher.process_ready_retries().await.unwrap();

        assert_eq!(
            notifier.ask_count(),
            1,
            "a denied approval is final, so neither an orphan sweep nor the retry manager may \
             run the issue again and ask the approver a second time"
        );
        assert_ne!(
            declined.status,
            FixAttemptStatus::Pending,
            "a denied approval must close the attempt rather than leave it pending for an \
             orphan sweep to release"
        );
        assert!(
            !RetryManager::new(watcher.config.retry.clone(), tracker.clone())
                .should_retry(&declined),
            "a denied approval must close the attempt in a status the retry manager never \
             retries"
        );
        assert_eq!(
            decisions_recorded(&tracker, &issue, "approval_denied"),
            1,
            "the decline should be recorded once for operators to see"
        );
    }

    #[tokio::test]
    async fn test_retry_declined_at_approval_is_neither_refunded_nor_asked_again() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("no"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let issue = test_issue();
        tracker
            .record_attempt(&issue.source, &issue.id, &issue.short_id)
            .unwrap();
        tracker
            .mark_failed(&issue.source, &issue.id, "agent crashed")
            .unwrap();
        let failed = tracker
            .get_attempt(&issue.source, &issue.id)
            .unwrap()
            .unwrap();
        let source = Arc::new(MockSource::with_issues(&issue.source, vec![issue.clone()]))
            as Arc<dyn IssueSource>;
        let watcher = retrying_approval_gated_watcher(notifier.clone(), tracker.clone(), source);

        watcher.process_ready_retries().await.unwrap();
        let declined = tracker
            .get_attempt(&issue.source, &issue.id)
            .unwrap()
            .unwrap();
        watcher.process_ready_retries().await.unwrap();

        assert_eq!(
            notifier.ask_count(),
            1,
            "a retry the approver declined is not in flight anywhere, so the retry manager must \
             not run it again and ask a second time"
        );
        assert_eq!(
            declined.retry_count,
            failed.retry_count + 1,
            "a retry the approver declined did run, so the retry it spent must not be refunded \
             as if it never started"
        );
        assert!(
            !RetryManager::new(watcher.config.retry.clone(), tracker.clone())
                .should_retry(&declined),
            "a retry the approver declined must close the attempt in a status the retry manager \
             never retries"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_retry_unanswered_at_approval_is_refunded_and_asked_again() {
        for reply in [None, Some("maybe later")] {
            let notifier = Arc::new(match reply {
                Some(answer) => ApprovalMockNotifier::with_reply(answer),
                None => ApprovalMockNotifier::with_no_reply(),
            });
            let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
            let issue = test_issue();
            tracker
                .record_attempt(&issue.source, &issue.id, &issue.short_id)
                .unwrap();
            tracker
                .mark_failed(&issue.source, &issue.id, "agent crashed")
                .unwrap();
            let failed = tracker
                .get_attempt(&issue.source, &issue.id)
                .unwrap()
                .unwrap();
            let source = Arc::new(MockSource::with_issues(&issue.source, vec![issue.clone()]))
                as Arc<dyn IssueSource>;
            let agent_calls = Arc::new(AtomicUsize::new(0));
            let mut watcher =
                create_approval_watcher(notifier.clone(), tracker.clone(), true, Some(60));
            watcher.sources = vec![source];
            watcher.agent = Arc::new(ScriptedQaAgent {
                calls: Arc::clone(&agent_calls),
                answer: QaAnswer::Crash,
            });
            watcher.config.retry = single_retry_config().retry;
            watcher.set_running(true);

            watcher.process_ready_retries().await.unwrap();
            let unanswered = tracker
                .get_attempt(&issue.source, &issue.id)
                .unwrap()
                .unwrap();
            watcher.process_ready_retries().await.unwrap();

            assert_eq!(
                (unanswered.status, unanswered.retry_count),
                (FixAttemptStatus::Failed, failed.retry_count),
                "a retry whose approval request was answered with {reply:?} did no work, so it \
                 must give back the retry it spent and leave the attempt failed, not pending"
            );
            assert!(
                RetryManager::new(watcher.config.retry.clone(), tracker.clone())
                    .should_retry(&unanswered),
                "a retry whose approval request was answered with {reply:?} must keep the \
                 attempt's only retry"
            );
            assert_eq!(
                notifier.ask_count(),
                2,
                "a retry whose approval request was answered with {reply:?} must leave the \
                 attempt for a later run to ask again"
            );
            assert_eq!(
                agent_calls.load(AtomicOrdering::SeqCst),
                0,
                "a retry whose approval request was answered with {reply:?} must not run the agent"
            );
        }
    }

    #[tokio::test]
    async fn test_approval_request_without_a_refusal_leaves_the_attempt_open() {
        for (reply, expected) in [
            (None, IssueRun::Unanswered),
            (Some("maybe later"), IssueRun::Unanswered),
            (Some("use org/unknown"), IssueRun::Skipped),
        ] {
            let notifier = Arc::new(match reply {
                Some(answer) => ApprovalMockNotifier::with_reply(answer),
                None => ApprovalMockNotifier::with_no_reply(),
            });
            let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
            let watcher = create_approval_watcher(notifier, tracker.clone(), true, Some(1));
            let issue = test_issue();
            let source = Arc::new(MockSource::with_issues(&issue.source, vec![issue.clone()]))
                as Arc<dyn IssueSource>;

            let outcome = watcher
                .process_issue(
                    source,
                    issue.clone(),
                    MatchResult::matched("Mock match", MatchPriority::Normal),
                    None,
                    None,
                    None,
                    None,
                )
                .await;

            assert_eq!(
                outcome, expected,
                "an approval request answered with {reply:?} was not refused, so the run must end \
                 {expected:?} rather than declined"
            );
            assert_eq!(
                tracker
                    .get_attempt(&issue.source, &issue.id)
                    .unwrap()
                    .unwrap()
                    .status,
                FixAttemptStatus::Pending,
                "an approval request answered with {reply:?} must leave the attempt open for a \
                 later run to ask again"
            );
        }
    }

    #[tokio::test]
    async fn test_review_rerun_declined_at_approval_keeps_its_pr_watched() {
        let notifier = Arc::new(ApprovalMockNotifier::with_reply("no"));
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let issue = Issue::new(
            "1",
            "MOCK-1",
            "Mock issue",
            "http://example.com/mock/1",
            "mock",
        );
        tracker
            .record_attempt(&issue.source, &issue.id, &issue.short_id)
            .unwrap();
        tracker
            .mark_success(
                &issue.source,
                &issue.id,
                "https://github.com/org/repo/pull/1",
            )
            .unwrap();
        let source = Arc::new(MockSource::with_issues(&issue.source, vec![issue.clone()]))
            as Arc<dyn IssueSource>;
        let checkout = tempfile::tempdir().unwrap();
        let mut watcher =
            create_approval_watcher(notifier.clone(), tracker.clone(), true, Some(60));
        watcher.sources = vec![source];
        watcher.inferrer = Some(inferrer_indexing(IndexedRepo::new(
            "org/repo",
            checkout.path(),
        )));
        let attempt = tracker
            .get_attempt(&issue.source, &issue.id)
            .unwrap()
            .unwrap();

        let outcome = watcher
            .process_review_action(&attempt, "Please add a test")
            .await;

        assert!(
            matches!(outcome, Ok(ReviewOutcome::Handled)),
            "a review rerun the approver declined is dealt with, so its feedback is not raised \
             and asked about again, but got {outcome:?}"
        );
        assert_eq!(notifier.ask_count(), 1, "the approver should be asked once");
        assert_eq!(
            tracker
                .get_attempt(&issue.source, &issue.id)
                .unwrap()
                .unwrap()
                .status,
            FixAttemptStatus::Success,
            "declining a review rerun must leave the attempt's open PR watched"
        );
    }
}
