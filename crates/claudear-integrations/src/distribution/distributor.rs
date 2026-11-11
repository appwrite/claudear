//! Assigning open Claudear PRs and telling their assignees.

use crate::distribution::PullRequestHost;
use crate::notifier::Delivery;
use crate::notifier::Notifier;
use chrono::DateTime;
use chrono::Utc;
use claudear_analysis::deploy_qa::GitHubDiscordMap;
use claudear_analysis::distribution::AssignmentNotice;
use claudear_analysis::distribution::Balancer;
use claudear_analysis::distribution::CodeOwners;
use claudear_analysis::distribution::Developer;
use claudear_analysis::distribution::Digest;
use claudear_analysis::distribution::DigestGroup;
use claudear_analysis::distribution::Evidence;
use claudear_analysis::distribution::Pending;
use claudear_analysis::distribution::PullRequest;
use claudear_analysis::distribution::PullRequestState;
use claudear_analysis::distribution::PullRequestSummary;
use claudear_analysis::distribution::Roster;
use claudear_analysis::distribution::WrappedUp;
use claudear_config::config::Config;
use claudear_config::config::PrDistributionConfig;
use claudear_config::users::UserRegistry;
use claudear_core::error::Error;
use claudear_core::error::Result;
use claudear_core::security::SECURITY_FIX_SOURCE;
use claudear_core::types::AssignmentStatus;
use claudear_core::types::Issue;
use claudear_core::types::PrAssignment;
use claudear_storage::parse_pr_url;
use claudear_storage::FixAttemptTracker;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

/// Host requests in flight at once while reading a PR's file history. GitHub
/// treats bursts of concurrent requests as abuse and rate-limits them.
const CONCURRENT_REQUESTS: usize = 8;

/// What one scheduled run changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// PRs given a new assignee.
    pub assigned: usize,
    /// Assignments ended because the assignee left the roster.
    pub released: usize,
    /// Assignments ended because the PR was merged or closed.
    pub closed: usize,
    /// PRs listed in the digest.
    pub listed: usize,
}

/// Makes one developer responsible for each open Claudear PR.
pub struct Distributor {
    roster: Roster,
    tolerance: usize,
    notify_on_open: bool,
    digest: bool,
    store: Arc<dyn FixAttemptTracker>,
    host: Arc<dyn PullRequestHost>,
    notifier: Arc<dyn Notifier>,
}

/// A PR waiting for an assignee: its URL as Claudear recorded it, the host's
/// view of it, and who held it before when it is being handed over.
struct Unassigned {
    url: String,
    pull_request: PullRequest,
    previous: Option<String>,
}

impl Distributor {
    /// A distributor over `roster` configured by `config`.
    pub fn new(
        config: &PrDistributionConfig,
        roster: Roster,
        store: Arc<dyn FixAttemptTracker>,
        host: Arc<dyn PullRequestHost>,
        notifier: Arc<dyn Notifier>,
    ) -> Self {
        Self {
            roster,
            tolerance: config.tolerance,
            notify_on_open: config.notify_on_open,
            digest: config.digest,
            store,
            host,
            notifier,
        }
    }

    /// The distributor `config` describes, or `None` when `[pr_distribution]`
    /// is disabled, there is no code host, or nobody is on the roster.
    ///
    /// The roster is built fresh from `registry` on each call, so console
    /// identity edits apply from the next PR or scheduled run without a
    /// restart. The `[deploy_qa]` GitHub↔Discord map is only a fallback for
    /// what the registry does not know.
    pub fn from_config(
        config: &Config,
        registry: &UserRegistry,
        store: Arc<dyn FixAttemptTracker>,
        host: Option<Arc<dyn PullRequestHost>>,
        notifier: Arc<dyn Notifier>,
    ) -> Option<Self> {
        let distribution = &config.pr_distribution;
        if !distribution.enabled {
            return None;
        }
        let Some(host) = host else {
            tracing::warn!(
                component = "pr_distribution",
                "No GitHub client; PR distribution disabled"
            );
            return None;
        };
        let map = config
            .deploy_qa
            .github_discord_map_path
            .as_deref()
            .map(|path| {
                GitHubDiscordMap::load_from_path(path).unwrap_or_else(|error| {
                    tracing::warn!(
                        component = "pr_distribution",
                        path,
                        error = %error,
                        "Failed to load the GitHub↔Discord map; using the user registry only"
                    );
                    GitHubDiscordMap::default()
                })
            })
            .unwrap_or_default();
        let roster = Roster::new(registry, &map, &distribution.excluded);
        if roster.is_empty() {
            tracing::warn!(
                component = "pr_distribution",
                "Nobody on the roster: add GitHub usernames to people in the console or [users]"
            );
            return None;
        }
        Some(Self::new(distribution, roster, store, host, notifier))
    }

    /// Assign a PR Claudear just opened for `issue` and ping the assignee.
    ///
    /// A PR that already has an assignee keeps it, so review reruns that
    /// report the same PR change nothing. Returns `None` when assignment on
    /// open is turned off, the PR is not a GitHub PR, or it does not need
    /// anyone yet; the scheduled run picks those up later. A security fix is
    /// never assigned, since a ping would announce it in a shared channel. Only the caller
    /// whose assignment is recorded pings, so concurrent callers never ping
    /// two people for one PR; a ping that fails or reaches no remote channel
    /// leaves the assignment unnotified for the scheduled run to ping again.
    pub async fn assign_opened(&self, pr_url: &str, issue: &Issue) -> Result<Option<PrAssignment>> {
        if !self.notify_on_open || issue.source == SECURITY_FIX_SOURCE {
            return Ok(None);
        }
        let Some((repo, number)) = github_pull_request(pr_url) else {
            return Ok(None);
        };
        if let Some(existing) = self.store.active_pr_assignment(pr_url)? {
            return Ok(Some(existing));
        }
        let pull_request = self.host.pull_request(&repo, number).await?;
        if !pull_request.attention(None).is_needed() {
            return Ok(None);
        }

        let held: Vec<String> = self
            .store
            .active_pr_assignments()?
            .into_iter()
            .map(|assignment| assignment.assignee)
            .collect();
        let evidence = self
            .evidence(
                &repo,
                number,
                self.roster.requesters(issue),
                &mut Repositories::default(),
            )
            .await;
        let pending = Pending {
            key: pr_url.to_string(),
            candidates: evidence.rank(&self.roster),
        };
        let Some(decision) = Balancer::new(&self.roster, self.tolerance)
            .assign(vec![pending], &held)
            .into_iter()
            .next()
        else {
            return Ok(None);
        };

        let assignment = PrAssignment::new(
            pr_url,
            &repo,
            number,
            &decision.assignee,
            decision.reason,
            decision.score,
        );
        let Some(stored) = self.store.record_pr_assignment(&assignment)? else {
            return self.store.active_pr_assignment(pr_url);
        };
        if let Some(developer) = self.roster.find(&stored.assignee) {
            match self.ping(developer, &pull_request, &stored).await {
                Ok(Delivery::Delivered) => {}
                Ok(Delivery::Local) => tracing::info!(
                    component = "pr_distribution",
                    pr_url,
                    "No remote notifier is configured; the scheduled run pings the new assignee once one is"
                ),
                Err(error) => tracing::warn!(
                    component = "pr_distribution",
                    pr_url,
                    error = %error,
                    "Failed to ping the new assignee; the scheduled run pings them again"
                ),
            }
        }
        self.store.active_pr_assignment(pr_url)
    }

    /// Reconcile every open Claudear PR, then send the digest.
    ///
    /// The PRs reconciled are those Claudear still tracks as open plus every
    /// PR with an active assignment, so an assignment outlives Claudear losing
    /// track of its PR (a failed review rerun, a reset) for as long as the PR
    /// is open on the host. Ends the assignments of merged and closed PRs,
    /// hands the PRs of developers who left the roster to someone else, and
    /// assigns every PR still needing someone that has no assignee. With the
    /// digest on, every assignee is then sent the PRs still waiting on them;
    /// with it off, each assignee no ping has reached yet is pinged instead.
    /// An assignment is marked notified only once a remote channel delivered
    /// its ping or its assignee's digest message, so one that only reached
    /// the console, or no channel at all, is sent again later.
    ///
    /// Every PR is reconciled and pinged before a ping that no channel
    /// delivered fails the run, so the scheduler retries it; an assignee who
    /// stays unreachable keeps their assignments and is pinged again on each
    /// retry and scheduled run until a ping reaches them. A digest fails the
    /// run only when it reached no assignee, since a retry would resend it to
    /// everyone it reached; the assignees it missed are listed again by the
    /// next scheduled digest. With no remote channel configured the run
    /// succeeds and marks nothing, so the scheduler does not retry it on
    /// every tick.
    pub async fn distribute(&self, now: DateTime<Utc>) -> Result<Outcome> {
        let mut outcome = Outcome::default();

        let mut open: BTreeMap<String, (String, i64)> = BTreeMap::new();
        for attempt in self.store.get_pending_prs()? {
            if attempt.source == SECURITY_FIX_SOURCE {
                continue;
            }
            if let Some(url) = attempt.pr_url {
                if let Some(pull_request) = github_pull_request(&url) {
                    open.insert(url, pull_request);
                }
            }
        }

        let mut active: HashMap<String, PrAssignment> = HashMap::new();
        for assignment in self.store.active_pr_assignments()? {
            open.entry(assignment.pr_url.clone())
                .or_insert_with(|| (assignment.repo.clone(), assignment.pr_number));
            active.insert(assignment.pr_url.clone(), assignment);
        }

        let mut held: Vec<String> = Vec::new();
        let mut finished: Vec<(PullRequest, PrAssignment)> = Vec::new();
        let mut kept: Vec<(PullRequest, PrAssignment)> = Vec::new();
        let mut unassigned: Vec<Unassigned> = Vec::new();
        for (url, (repo, number)) in &open {
            let assignment = active.remove(url);
            let pull_request = match self.host.pull_request(repo, *number).await {
                Ok(pull_request) => pull_request,
                Err(error) => {
                    tracing::warn!(
                        component = "pr_distribution",
                        pr_url = %url,
                        error = %error,
                        "Failed to fetch PR; leaving its assignment as it is"
                    );
                    held.extend(assignment.map(|assignment| assignment.assignee));
                    continue;
                }
            };
            let attention = pull_request.attention(None);
            if attention.is_finished() {
                if let Some(assignment) = assignment {
                    finished.push((pull_request, assignment));
                }
                continue;
            }
            let previous = match assignment {
                Some(assignment) if self.roster.find(&assignment.assignee).is_some() => {
                    held.push(assignment.assignee.clone());
                    kept.push((pull_request, assignment));
                    continue;
                }
                Some(assignment) => {
                    if self
                        .store
                        .end_pr_assignment(assignment.id, AssignmentStatus::Released)?
                    {
                        outcome.released += 1;
                    }
                    Some(assignment.assignee)
                }
                None => None,
            };
            if attention.is_needed() {
                unassigned.push(Unassigned {
                    url: url.clone(),
                    pull_request,
                    previous,
                });
            }
        }

        let mut repositories = Repositories::default();
        let mut pending = Vec::with_capacity(unassigned.len());
        for item in &unassigned {
            let evidence = self
                .evidence(
                    &item.pull_request.repo,
                    item.pull_request.number,
                    Vec::new(),
                    &mut repositories,
                )
                .await;
            pending.push(Pending {
                key: item.url.clone(),
                candidates: evidence.rank(&self.roster),
            });
        }

        let mut assigned: Vec<(&Unassigned, PrAssignment)> = Vec::new();
        for decision in Balancer::new(&self.roster, self.tolerance).assign(pending, &held) {
            let Some(item) = unassigned.iter().find(|item| item.url == decision.key) else {
                continue;
            };
            let assignment = PrAssignment {
                previous: item.previous.clone(),
                ..PrAssignment::new(
                    &item.url,
                    &item.pull_request.repo,
                    item.pull_request.number,
                    &decision.assignee,
                    decision.reason,
                    decision.score,
                )
            };
            if let Some(stored) = self.store.record_pr_assignment(&assignment)? {
                outcome.assigned += 1;
                assigned.push((item, stored));
            }
        }
        let current = kept
            .iter()
            .map(|(pull_request, assignment)| (pull_request, assignment))
            .chain(
                assigned
                    .iter()
                    .map(|(item, assignment)| (&item.pull_request, assignment)),
            );

        let (finished, unrounded): (Vec<_>, Vec<_>) =
            finished.into_iter().partition(|(_, assignment)| {
                self.digest && self.roster.find(&assignment.assignee).is_some()
            });
        self.close(
            unrounded.iter().map(|(_, assignment)| assignment),
            &mut outcome,
        )?;

        if self.digest {
            let digest = self.digest(current.clone(), &finished, now);
            outcome.listed = digest.count();
            if digest.is_empty() {
                return Ok(outcome);
            }
            let delivery = self.notifier.notify_pr_digest(&digest).await?;
            if delivery.is_empty() {
                Self::unreached(digest.count());
                return Ok(outcome);
            }
            for (_, assignment) in current {
                if assignment.notified_at.is_none() && delivery.contains(&assignment.pr_url) {
                    self.store.mark_pr_assignment_notified(assignment.id)?;
                }
            }
            self.close(
                finished
                    .iter()
                    .map(|(_, assignment)| assignment)
                    .filter(|assignment| delivery.reached(&assignment.assignee)),
                &mut outcome,
            )?;
            return Ok(outcome);
        }

        let mut undelivered = 0;
        let mut local = 0;
        for (pull_request, assignment) in current {
            if assignment.notified_at.is_some() {
                continue;
            }
            let Some(developer) = self.roster.find(&assignment.assignee) else {
                continue;
            };
            match self.ping(developer, pull_request, assignment).await {
                Ok(Delivery::Delivered) => {}
                Ok(Delivery::Local) => local += 1,
                Err(error) => {
                    tracing::warn!(
                        component = "pr_distribution",
                        pr_url = %assignment.pr_url,
                        error = %error,
                        "Failed to ping the assignee; pinging them again on the next run"
                    );
                    undelivered += 1;
                }
            }
        }
        if local > 0 {
            Self::unreached(local);
        }
        if undelivered > 0 {
            return Err(Error::Notifier {
                notifier: self.notifier.name().to_string(),
                message: format!("{undelivered} assignees were not pinged"),
            });
        }
        Ok(outcome)
    }

    /// Everything known about who owns `repo#number`. A host request that
    /// fails only drops its signal, so the PR is still assigned on the rest.
    async fn evidence(
        &self,
        repo: &str,
        number: i64,
        requesters: Vec<String>,
        repositories: &mut Repositories,
    ) -> Evidence {
        let files = self
            .host
            .changed_files(repo, number)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    component = "pr_distribution",
                    repo,
                    number,
                    error = %error,
                    "Failed to read changed files; assigning without them"
                );
                Vec::new()
            });
        let requests: Vec<_> = files
            .iter()
            .map(|path| {
                let host = Arc::clone(&self.host);
                let repo = repo.to_string();
                let path = path.clone();
                async move { host.commit_authors(&repo, Some(&path)).await }
            })
            .collect();
        let file_authors: Vec<String> = futures::stream::iter(requests)
            .buffered(CONCURRENT_REQUESTS)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flat_map(|authors| {
                authors.unwrap_or_else(|error| {
                    tracing::warn!(
                        component = "pr_distribution",
                        repo,
                        number,
                        error = %error,
                        "Failed to read file history; assigning without it"
                    );
                    Vec::new()
                })
            })
            .collect();
        Evidence {
            requesters,
            codeowners: repositories.codeowners(self.host.as_ref(), repo).await,
            repository_authors: repositories.authors(self.host.as_ref(), repo).await,
            files,
            file_authors,
        }
    }

    /// Tell `developer`, `assignment`'s assignee, they own `pull_request`,
    /// then record that they were told so later runs stop pinging them. A
    /// ping that reached only local channels is not recorded.
    async fn ping(
        &self,
        developer: &Developer,
        pull_request: &PullRequest,
        assignment: &PrAssignment,
    ) -> Result<Delivery> {
        let mut summary = PullRequestSummary::from(pull_request);
        summary.url = assignment.pr_url.clone();
        let delivery = self
            .notifier
            .notify_pr_assignment(&AssignmentNotice {
                developer: developer.clone(),
                pull_request: summary,
                reason: assignment.reason,
                previous: assignment.previous.clone(),
            })
            .await?;
        if delivery == Delivery::Delivered {
            self.store.mark_pr_assignment_notified(assignment.id)?;
        }
        Ok(delivery)
    }

    /// End the assignments of PRs that merged or closed. A PR in a roundup is
    /// only closed once its assignee's digest was sent, so a roundup that
    /// failed to reach them is retried on the next run.
    fn close<'a>(
        &self,
        assignments: impl Iterator<Item = &'a PrAssignment>,
        outcome: &mut Outcome,
    ) -> Result<()> {
        for assignment in assignments {
            if self
                .store
                .end_pr_assignment(assignment.id, AssignmentStatus::Closed)?
            {
                outcome.closed += 1;
            }
        }
        Ok(())
    }

    /// Report, once per run, that `count` PRs reached no remote channel and
    /// so stay unnotified until one is configured.
    fn unreached(count: usize) {
        tracing::warn!(
            component = "pr_distribution",
            count,
            "No remote notifier is configured; PR assignees stay unnotified until one is"
        );
    }

    /// The digest of assigned PRs still waiting on their assignee, one group
    /// per assignee in roster order with their PRs oldest first, each with
    /// the roundup of their PRs `finished` in this run.
    fn digest<'a>(
        &self,
        assigned: impl Iterator<Item = (&'a PullRequest, &'a PrAssignment)>,
        finished: &[(PullRequest, PrAssignment)],
        now: DateTime<Utc>,
    ) -> Digest {
        let mut wrapped_up: HashMap<String, Vec<WrappedUp>> = HashMap::new();
        for (pull_request, assignment) in finished {
            let Some(developer) = self.roster.find(&assignment.assignee) else {
                continue;
            };
            let mut summary = PullRequestSummary::from(pull_request);
            summary.url = assignment.pr_url.clone();
            wrapped_up
                .entry(developer.login.clone())
                .or_default()
                .push(WrappedUp {
                    pull_request: summary,
                    merged: pull_request.state == PullRequestState::Merged,
                });
        }
        let mut waiting: HashMap<String, Vec<PullRequestSummary>> = HashMap::new();
        for (pull_request, assignment) in assigned {
            let Some(developer) = self.roster.find(&assignment.assignee) else {
                continue;
            };
            if !pull_request.attention(Some(&developer.login)).is_needed() {
                continue;
            }
            let mut summary = PullRequestSummary::from(pull_request);
            summary.url = assignment.pr_url.clone();
            waiting
                .entry(developer.login.clone())
                .or_default()
                .push(summary);
        }
        let groups = self
            .roster
            .developers()
            .iter()
            .filter_map(|developer| {
                let mut pull_requests = waiting.remove(&developer.login).unwrap_or_default();
                let mut wrapped_up = wrapped_up.remove(&developer.login).unwrap_or_default();
                if pull_requests.is_empty() && wrapped_up.is_empty() {
                    return None;
                }
                pull_requests.sort_by(|left, right| {
                    left.opened_at
                        .cmp(&right.opened_at)
                        .then_with(|| left.url.cmp(&right.url))
                });
                wrapped_up
                    .sort_by(|left, right| left.pull_request.url.cmp(&right.pull_request.url));
                Some(DigestGroup {
                    developer: developer.clone(),
                    pull_requests,
                    wrapped_up,
                })
            })
            .collect();
        Digest {
            groups,
            generated_at: now,
        }
    }
}

/// Repository-wide evidence, read once per repository per run.
#[derive(Default)]
struct Repositories {
    codeowners: HashMap<String, Option<CodeOwners>>,
    authors: HashMap<String, Vec<String>>,
}

impl Repositories {
    async fn codeowners(&mut self, host: &dyn PullRequestHost, repo: &str) -> Option<CodeOwners> {
        if let Some(codeowners) = self.codeowners.get(repo) {
            return codeowners.clone();
        }
        let codeowners = match host.codeowners(repo).await {
            Ok(text) => text.as_deref().map(CodeOwners::parse),
            Err(error) => {
                tracing::warn!(
                    component = "pr_distribution",
                    repo,
                    error = %error,
                    "Failed to read CODEOWNERS"
                );
                None
            }
        };
        self.codeowners.insert(repo.to_string(), codeowners.clone());
        codeowners
    }

    async fn authors(&mut self, host: &dyn PullRequestHost, repo: &str) -> Vec<String> {
        if let Some(authors) = self.authors.get(repo) {
            return authors.clone();
        }
        let authors = host
            .commit_authors(repo, None)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    component = "pr_distribution",
                    repo,
                    error = %error,
                    "Failed to read repository activity"
                );
                Vec::new()
            });
        self.authors.insert(repo.to_string(), authors.clone());
        authors
    }
}

/// `(owner/repo, number)` of a GitHub PR URL; GitLab merge requests are not
/// distributed.
fn github_pull_request(url: &str) -> Option<(String, i64)> {
    parse_pr_url(url).filter(|_| url.contains("/pull/"))
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::notifier::CompositeNotifier;
    use crate::notifier::ConsoleNotifier;
    use crate::notifier::DigestDelivery;
    use async_trait::async_trait;
    use chrono::TimeZone;
    use claudear_analysis::distribution::PullRequestState;
    use claudear_analysis::distribution::Review;
    use claudear_analysis::distribution::Verdict;
    use claudear_core::types::AssignmentReason;
    use claudear_storage::AttemptTracker;
    use claudear_storage::SqliteTracker;
    use std::collections::HashSet;
    use std::sync::Mutex;

    const REPO: &str = "org/repo";

    fn url(number: i64) -> String {
        format!("https://github.com/{REPO}/pull/{number}")
    }

    fn open(number: i64) -> PullRequest {
        PullRequest {
            url: url(number),
            repo: REPO.to_string(),
            number,
            title: format!("Fix {number}"),
            state: PullRequestState::Open,
            draft: false,
            auto_merge: false,
            head: Some(format!("head-{number}")),
            requested_reviewers: Vec::new(),
            reviews: Vec::new(),
            created_at: Some(Utc::now() - chrono::Duration::days(number)),
        }
    }

    #[derive(Default)]
    struct FakeHost {
        pull_requests: Mutex<HashMap<i64, PullRequest>>,
        files: HashMap<i64, Vec<String>>,
        codeowners: Option<String>,
        file_authors: HashMap<String, Vec<String>>,
        repository_authors: Vec<String>,
        broken_files: bool,
    }

    impl FakeHost {
        fn with(pull_requests: impl IntoIterator<Item = PullRequest>) -> Self {
            Self {
                pull_requests: Mutex::new(
                    pull_requests
                        .into_iter()
                        .map(|pull_request| (pull_request.number, pull_request))
                        .collect(),
                ),
                ..Self::default()
            }
        }

        fn update(&self, number: i64, change: impl FnOnce(&mut PullRequest)) {
            change(self.pull_requests.lock().unwrap().get_mut(&number).unwrap());
        }
    }

    #[async_trait]
    impl PullRequestHost for FakeHost {
        async fn pull_request(&self, _repo: &str, number: i64) -> Result<PullRequest> {
            self.pull_requests
                .lock()
                .unwrap()
                .get(&number)
                .cloned()
                .ok_or_else(|| Error::Other(format!("no PR {number}")))
        }

        async fn changed_files(&self, _repo: &str, number: i64) -> Result<Vec<String>> {
            if self.broken_files {
                return Err(Error::Other("files unavailable".to_string()));
            }
            Ok(self.files.get(&number).cloned().unwrap_or_default())
        }

        async fn codeowners(&self, _repo: &str) -> Result<Option<String>> {
            Ok(self.codeowners.clone())
        }

        async fn commit_authors(&self, _repo: &str, path: Option<&str>) -> Result<Vec<String>> {
            Ok(match path {
                Some(path) => self.file_authors.get(path).cloned().unwrap_or_default(),
                None => self.repository_authors.clone(),
            })
        }
    }

    /// A channel that, like Slack and Discord, sends a digest one assignee at
    /// a time, and cannot reach the developers in `unreachable`.
    #[derive(Default)]
    struct Recorder {
        notices: Mutex<Vec<AssignmentNotice>>,
        digests: Mutex<Vec<Digest>>,
        unreachable: Mutex<HashSet<String>>,
    }

    impl Recorder {
        fn refuse(&self, developer: &Developer) -> Result<()> {
            if self.unreachable.lock().unwrap().contains(&developer.login) {
                return Err(Error::Other("channel unreachable".to_string()));
            }
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
        async fn notify_pr_assignment(&self, notice: &AssignmentNotice) -> Result<Delivery> {
            self.refuse(&notice.developer)?;
            self.notices.lock().unwrap().push(notice.clone());
            Ok(Delivery::Delivered)
        }
        async fn notify_pr_digest(&self, digest: &Digest) -> Result<DigestDelivery> {
            let delivered = Mutex::new(Vec::new());
            let groups = digest
                .groups
                .iter()
                .map(|group| (group, vec![group.clone()]))
                .collect();
            let result = crate::notifier::send_each(self.name(), groups, |group| {
                let sent = self.refuse(&group.developer);
                if sent.is_ok() {
                    delivered.lock().unwrap().push(group);
                }
                std::future::ready(sent)
            })
            .await;
            let groups = delivered.into_inner().unwrap();
            if !groups.is_empty() {
                self.digests.lock().unwrap().push(Digest {
                    groups,
                    generated_at: digest.generated_at,
                });
            }
            result
        }
    }

    struct Harness {
        store: Arc<SqliteTracker>,
        host: Arc<FakeHost>,
        recorder: Arc<Recorder>,
        notifier: Arc<dyn Notifier>,
    }

    impl Harness {
        fn new(host: FakeHost) -> Self {
            let recorder = Arc::new(Recorder::default());
            let mut notifier = CompositeNotifier::new();
            notifier.add(Arc::new(ConsoleNotifier::new()));
            notifier.add(recorder.clone());
            Self {
                store: Arc::new(SqliteTracker::in_memory().unwrap()),
                host: Arc::new(host),
                recorder,
                notifier: Arc::new(notifier),
            }
        }

        fn unreachable(&self, logins: &[&str]) {
            *self.recorder.unreachable.lock().unwrap() =
                logins.iter().map(|login| login.to_string()).collect();
        }

        /// Record that Claudear opened PR `number`.
        fn opened(&self, number: i64) {
            self.opened_from("linear", number);
        }

        /// Record that Claudear opened PR `number` for an issue from `source`.
        fn opened_from(&self, source: &str, number: i64) {
            let issue_id = format!("issue-{number}");
            self.store
                .record_attempt(source, &issue_id, &issue_id)
                .unwrap();
            self.store
                .mark_success(source, &issue_id, &url(number))
                .unwrap();
        }

        fn distributor(&self, logins: &[&str], config: PrDistributionConfig) -> Distributor {
            self.distributor_on(self.host.clone(), logins, config)
        }

        fn distributor_on(
            &self,
            host: Arc<dyn PullRequestHost>,
            logins: &[&str],
            config: PrDistributionConfig,
        ) -> Distributor {
            self.distributor_through(self.notifier.clone(), host, logins, config)
        }

        /// A distributor over the same store and host whose only channel is
        /// the console, as when no remote notifier is configured.
        fn console_only(&self, logins: &[&str], config: PrDistributionConfig) -> Distributor {
            let mut console = CompositeNotifier::new();
            console.add(crate::telemetry::InstrumentedNotifier::wrap(Arc::new(
                ConsoleNotifier::new(),
            )));
            self.distributor_through(Arc::new(console), self.host.clone(), logins, config)
        }

        fn distributor_through(
            &self,
            notifier: Arc<dyn Notifier>,
            host: Arc<dyn PullRequestHost>,
            logins: &[&str],
            config: PrDistributionConfig,
        ) -> Distributor {
            let roster = Roster::from_developers(
                logins
                    .iter()
                    .map(|login| Developer {
                        login: login.to_string(),
                        discord_id: Some(format!("discord-{login}")),
                        slack_id: None,
                    })
                    .collect(),
            );
            Distributor::new(&config, roster, self.store.clone(), host, notifier)
        }

        fn active(&self, number: i64) -> PrAssignment {
            self.store
                .active_pr_assignment(&url(number))
                .unwrap()
                .unwrap()
        }

        fn pinged(&self) -> Vec<(String, String)> {
            self.recorder
                .notices
                .lock()
                .unwrap()
                .iter()
                .map(|notice| {
                    (
                        notice.pull_request.url.clone(),
                        notice.developer.login.clone(),
                    )
                })
                .collect()
        }

        fn assignees(&self) -> Vec<(i64, String)> {
            let mut assignees: Vec<(i64, String)> = self
                .store
                .active_pr_assignments()
                .unwrap()
                .into_iter()
                .map(|assignment| (assignment.pr_number, assignment.assignee))
                .collect();
            assignees.sort();
            assignees
        }

        fn load(&self, login: &str) -> usize {
            self.assignees()
                .iter()
                .filter(|(_, assignee)| assignee == login)
                .count()
        }
    }

    fn config() -> PrDistributionConfig {
        PrDistributionConfig {
            enabled: true,
            ..PrDistributionConfig::default()
        }
    }

    fn requested_by(login: &str) -> Issue {
        let mut issue = Issue::new("1", "o/r#1", "Title", "url", "github_issues");
        issue.set_metadata("author", login);
        issue
    }

    #[tokio::test]
    async fn opened_pr_goes_to_its_requester_and_pings_them() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened(1);
        let distributor = harness.distributor(&["amy", "bob"], config());

        let assignment = distributor
            .assign_opened(&url(1), &requested_by("bob"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(assignment.assignee, "bob");
        assert_eq!(assignment.reason, AssignmentReason::Requester);
        let notices = harness.recorder.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].developer.discord_id.as_deref(),
            Some("discord-bob")
        );
        assert_eq!(notices[0].pull_request.url, url(1));
        assert_eq!(notices[0].reason, AssignmentReason::Requester);
    }

    #[tokio::test]
    async fn an_assigned_pr_keeps_its_assignee_when_reported_again() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened(1);
        let distributor = harness.distributor(&["amy", "bob"], config());
        distributor
            .assign_opened(&url(1), &requested_by("bob"))
            .await
            .unwrap();

        let again = distributor
            .assign_opened(&url(1), &requested_by("amy"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(again.assignee, "bob");
        assert_eq!(harness.recorder.notices.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn opened_pr_is_still_assigned_when_its_changed_files_are_unavailable() {
        let mut host = FakeHost::with([open(1)]);
        host.codeowners = Some("* @amy".to_string());
        host.broken_files = true;
        host.repository_authors = vec!["bob".to_string()];
        let harness = Harness::new(host);
        harness.opened(1);
        let distributor = harness.distributor(&["amy", "bob"], config());

        let assignment = distributor
            .assign_opened(&url(1), &Issue::new("1", "S-1", "T", "u", "sentry"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(assignment.assignee, "bob");
        assert_eq!(assignment.reason, AssignmentReason::Activity);
    }

    #[tokio::test]
    async fn assignment_on_open_respects_the_switch_drafts_and_gitlab() {
        let mut draft = open(2);
        draft.draft = true;
        let harness = Harness::new(FakeHost::with([open(1), draft]));
        let issue = requested_by("amy");

        let off = harness.distributor(
            &["amy"],
            PrDistributionConfig {
                notify_on_open: false,
                ..config()
            },
        );
        assert!(off.assign_opened(&url(1), &issue).await.unwrap().is_none());

        let on = harness.distributor(&["amy"], config());
        assert!(on.assign_opened(&url(2), &issue).await.unwrap().is_none());
        assert!(on
            .assign_opened("https://gitlab.com/org/repo/-/merge_requests/1", &issue)
            .await
            .unwrap()
            .is_none());
        assert!(harness.assignees().is_empty());
        assert!(harness.recorder.notices.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_security_fix_pr_is_never_assigned_or_announced_on_open() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened_from(SECURITY_FIX_SOURCE, 1);
        let distributor = harness.distributor(&["amy"], config());
        let fix = Issue::new("1", "SEC-1", "Title", "url", SECURITY_FIX_SOURCE);

        let assignment = distributor.assign_opened(&url(1), &fix).await.unwrap();

        assert!(assignment.is_none());
        assert!(harness.assignees().is_empty());
        assert!(harness.pinged().is_empty());
    }

    #[tokio::test]
    async fn a_scheduled_run_leaves_security_fix_prs_out_of_assignments_and_digests() {
        let harness = Harness::new(FakeHost::with([open(1), open(2)]));
        harness.opened(1);
        harness.opened_from(SECURITY_FIX_SOURCE, 2);
        let distributor = harness.distributor(&["amy"], config());

        let outcome = distributor.distribute(Utc::now()).await.unwrap();

        assert_eq!(outcome.assigned, 1);
        assert_eq!(outcome.listed, 1);
        assert_eq!(harness.assignees(), vec![(1, "amy".to_string())]);
        let digested: Vec<String> = harness.recorder.digests.lock().unwrap()[0]
            .groups
            .iter()
            .flat_map(|group| group.pull_requests.iter())
            .map(|pull_request| pull_request.url.clone())
            .collect();
        assert_eq!(digested, vec![url(1)]);
    }

    #[tokio::test]
    async fn opened_prs_spread_even_when_one_developer_owns_everything() {
        let mut host = FakeHost::with((1..=6).map(open));
        host.codeowners = Some("* @amy".to_string());
        host.files = (1..=6)
            .map(|number| (number, vec!["src/lib.rs".to_string()]))
            .collect();
        let harness = Harness::new(host);
        let distributor = harness.distributor(&["amy", "bob", "cat"], config());

        for number in 1..=6 {
            harness.opened(number);
            distributor
                .assign_opened(&url(number), &Issue::new("1", "S-1", "T", "u", "sentry"))
                .await
                .unwrap();
        }

        for login in ["amy", "bob", "cat"] {
            assert_eq!(harness.load(login), 2, "{login}");
        }
    }

    #[tokio::test]
    async fn scheduled_run_backfills_balanced_and_sends_one_digest() {
        let mut host = FakeHost::with((1..=6).map(open));
        host.codeowners = Some("* @amy".to_string());
        host.files = (1..=6)
            .map(|number| (number, vec!["src/lib.rs".to_string()]))
            .collect();
        let harness = Harness::new(host);
        (1..=6).for_each(|number| harness.opened(number));
        let distributor = harness.distributor(&["amy", "bob", "cat"], config());

        let outcome = distributor.distribute(Utc::now()).await.unwrap();

        assert_eq!(
            outcome,
            Outcome {
                assigned: 6,
                released: 0,
                closed: 0,
                listed: 6,
            }
        );
        for login in ["amy", "bob", "cat"] {
            assert_eq!(harness.load(login), 2, "{login}");
        }
        assert!(harness.recorder.notices.lock().unwrap().is_empty());
        let digests = harness.recorder.digests.lock().unwrap();
        assert_eq!(digests.len(), 1);
        let groups: Vec<(&str, usize)> = digests[0]
            .groups
            .iter()
            .map(|group| (group.developer.login.as_str(), group.pull_requests.len()))
            .collect();
        assert_eq!(groups, vec![("amy", 2), ("bob", 2), ("cat", 2)]);
        let amy = &digests[0].groups[0].pull_requests;
        assert!(amy[0].opened_at <= amy[1].opened_at);
    }

    #[tokio::test]
    async fn scheduled_runs_are_sticky_and_reproducible() {
        let mut host = FakeHost::with((1..=4).map(open));
        host.repository_authors = vec!["amy".to_string()];
        let first = Harness::new(host);
        (1..=4).for_each(|number| first.opened(number));
        let distributor = first.distributor(&["amy", "bob"], config());
        distributor.distribute(Utc::now()).await.unwrap();
        let assigned = first.assignees();

        let mut changed = FakeHost::with((1..=4).map(open));
        changed.repository_authors = vec!["bob".to_string()];
        let sticky = Distributor::new(
            &config(),
            Roster::from_developers(vec![Developer::new("amy"), Developer::new("bob")]),
            first.store.clone(),
            Arc::new(changed),
            first.notifier.clone(),
        );
        let outcome = sticky.distribute(Utc::now()).await.unwrap();
        assert_eq!(outcome.assigned, 0);
        assert_eq!(first.assignees(), assigned);

        let mut host = FakeHost::with((1..=4).map(open));
        host.repository_authors = vec!["amy".to_string()];
        let second = Harness::new(host);
        (1..=4).for_each(|number| second.opened(number));
        second
            .distributor(&["amy", "bob"], config())
            .distribute(Utc::now())
            .await
            .unwrap();
        assert_eq!(second.assignees(), assigned);
    }

    #[tokio::test]
    async fn assignee_leaving_the_roster_hands_their_prs_over() {
        let harness = Harness::new(FakeHost::with([open(1), open(2)]));
        harness.opened(1);
        harness.opened(2);
        let before = harness.distributor(
            &["amy", "bob"],
            PrDistributionConfig {
                digest: false,
                ..config()
            },
        );
        before.distribute(Utc::now()).await.unwrap();
        assert_eq!(
            harness.assignees(),
            vec![(1, "amy".to_string()), (2, "bob".to_string())]
        );
        harness.recorder.notices.lock().unwrap().clear();

        let after = harness.distributor(
            &["bob", "cat"],
            PrDistributionConfig {
                digest: false,
                ..config()
            },
        );
        let outcome = after.distribute(Utc::now()).await.unwrap();

        assert_eq!(outcome.released, 1);
        assert_eq!(outcome.assigned, 1);
        assert_eq!(
            harness.assignees(),
            vec![(1, "cat".to_string()), (2, "bob".to_string())]
        );
        let notices = harness.recorder.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].developer.login, "cat");
        assert_eq!(notices[0].previous.as_deref(), Some("amy"));
        assert_eq!(notices[0].reason, AssignmentReason::Load);
    }

    fn pings() -> PrDistributionConfig {
        PrDistributionConfig {
            digest: false,
            ..config()
        }
    }

    /// Holds every PR lookup until `callers` lookups are waiting, so that
    /// many callers all see a PR unassigned before any of them records it.
    struct GatedHost {
        inner: Arc<FakeHost>,
        gate: tokio::sync::Barrier,
    }

    #[async_trait]
    impl PullRequestHost for GatedHost {
        async fn pull_request(&self, repo: &str, number: i64) -> Result<PullRequest> {
            self.gate.wait().await;
            self.inner.pull_request(repo, number).await
        }

        async fn changed_files(&self, repo: &str, number: i64) -> Result<Vec<String>> {
            self.inner.changed_files(repo, number).await
        }

        async fn codeowners(&self, repo: &str) -> Result<Option<String>> {
            self.inner.codeowners(repo).await
        }

        async fn commit_authors(&self, repo: &str, path: Option<&str>) -> Result<Vec<String>> {
            self.inner.commit_authors(repo, path).await
        }
    }

    #[tokio::test]
    async fn concurrent_assigners_of_one_pr_ping_only_the_recorded_assignee() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened(1);
        let host: Arc<dyn PullRequestHost> = Arc::new(GatedHost {
            inner: harness.host.clone(),
            gate: tokio::sync::Barrier::new(2),
        });
        let first = harness.distributor_on(host.clone(), &["amy"], config());
        let second = harness.distributor_on(host, &["bob"], config());
        let issue = Issue::new("1", "S-1", "T", "u", "sentry");
        let pr_url = url(1);

        let (first, second) = tokio::join!(
            first.assign_opened(&pr_url, &issue),
            second.assign_opened(&pr_url, &issue)
        );

        let winner = harness.active(1);
        assert_eq!(first.unwrap().unwrap().assignee, winner.assignee);
        assert_eq!(second.unwrap().unwrap().assignee, winner.assignee);
        assert_eq!(
            harness.pinged(),
            vec![(url(1), winner.assignee)],
            "only the caller whose assignment was recorded may ping"
        );
    }

    #[tokio::test]
    async fn concurrent_scheduled_runs_ping_only_the_recorded_assignee() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened(1);
        let host: Arc<dyn PullRequestHost> = Arc::new(GatedHost {
            inner: harness.host.clone(),
            gate: tokio::sync::Barrier::new(2),
        });
        let first = harness.distributor_on(host.clone(), &["amy"], pings());
        let second = harness.distributor_on(host, &["bob"], pings());

        let (first, second) =
            tokio::join!(first.distribute(Utc::now()), second.distribute(Utc::now()));

        assert_eq!(first.unwrap().assigned + second.unwrap().assigned, 1);
        assert_eq!(harness.pinged(), vec![(url(1), harness.active(1).assignee)]);
    }

    #[tokio::test]
    async fn an_undelivered_ping_keeps_the_assignment_and_pings_the_same_assignee_next_run() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened(1);
        let distributor = harness.distributor(&["amy", "bob"], pings());

        harness.unreachable(&["amy", "bob"]);
        assert!(
            distributor.distribute(Utc::now()).await.is_err(),
            "a run whose ping reached nobody must fail so the scheduler retries it"
        );
        let assignment = harness.active(1);
        assert!(assignment.notified_at.is_none());

        harness.unreachable(&[]);
        let outcome = distributor.distribute(Utc::now()).await.unwrap();

        assert_eq!(outcome.assigned, 0);
        assert_eq!(harness.active(1).id, assignment.id);
        assert!(harness.active(1).notified_at.is_some());
        assert_eq!(harness.pinged(), vec![(url(1), assignment.assignee)]);
    }

    #[tokio::test]
    async fn a_delivered_ping_is_recorded_and_not_repeated() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened(1);
        let distributor = harness.distributor(&["amy"], pings());

        distributor.distribute(Utc::now()).await.unwrap();
        assert!(harness.active(1).notified_at.is_some());
        distributor.distribute(Utc::now()).await.unwrap();

        assert_eq!(harness.pinged(), vec![(url(1), "amy".to_string())]);
    }

    #[tokio::test]
    async fn an_undelivered_ping_on_open_is_repeated_by_the_scheduled_run() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened(1);
        let distributor = harness.distributor(&["amy", "bob"], pings());

        harness.unreachable(&["bob"]);
        let assignment = distributor
            .assign_opened(&url(1), &requested_by("bob"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(assignment.assignee, "bob");
        assert!(assignment.notified_at.is_none());
        assert!(harness.pinged().is_empty());

        harness.unreachable(&[]);
        distributor.distribute(Utc::now()).await.unwrap();

        assert_eq!(harness.assignees(), vec![(1, "bob".to_string())]);
        assert_eq!(harness.pinged(), vec![(url(1), "bob".to_string())]);
    }

    #[tokio::test]
    async fn an_unreachable_assignee_does_not_hold_up_the_other_prs() {
        let harness = Harness::new(FakeHost::with([open(1), open(2)]));
        harness.opened(1);
        harness.opened(2);
        let distributor = harness.distributor(&["amy", "bob"], pings());

        harness.unreachable(&["amy"]);
        assert!(distributor.distribute(Utc::now()).await.is_err());
        assert!(distributor.distribute(Utc::now()).await.is_err());

        assert_eq!(
            harness.assignees(),
            vec![(1, "amy".to_string()), (2, "bob".to_string())]
        );
        assert!(harness.active(1).notified_at.is_none());
        assert_eq!(harness.pinged(), vec![(url(2), "bob".to_string())]);

        harness.unreachable(&[]);
        distributor.distribute(Utc::now()).await.unwrap();

        assert_eq!(
            harness.pinged(),
            vec![(url(2), "bob".to_string()), (url(1), "amy".to_string())]
        );
    }

    #[tokio::test]
    async fn a_repeated_handover_ping_still_names_the_previous_assignee() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened(1);
        harness
            .distributor(&["amy"], pings())
            .distribute(Utc::now())
            .await
            .unwrap();
        let after = harness.distributor(&["bob"], pings());

        harness.unreachable(&["bob"]);
        assert!(after.distribute(Utc::now()).await.is_err());
        harness.unreachable(&[]);
        after.distribute(Utc::now()).await.unwrap();

        let notices = harness.recorder.notices.lock().unwrap();
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[1].developer.login, "bob");
        assert_eq!(notices[1].previous.as_deref(), Some("amy"));
    }

    #[tokio::test]
    async fn an_undelivered_digest_fails_the_run_and_is_sent_on_the_next() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened(1);
        let distributor = harness.distributor(&["amy"], config());

        harness.unreachable(&["amy"]);
        assert!(distributor.distribute(Utc::now()).await.is_err());
        assert!(harness.active(1).notified_at.is_none());

        harness.unreachable(&[]);
        let outcome = distributor.distribute(Utc::now()).await.unwrap();

        assert_eq!(outcome.listed, 1);
        assert!(harness.active(1).notified_at.is_some());
        let digests = harness.recorder.digests.lock().unwrap();
        assert_eq!(digests.len(), 1);
        assert_eq!(digests[0].groups[0].pull_requests[0].url, url(1));
    }

    fn digested(harness: &Harness) -> Vec<String> {
        harness
            .recorder
            .digests
            .lock()
            .unwrap()
            .iter()
            .flat_map(|digest| digest.groups.iter())
            .map(|group| group.developer.login.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_digest_that_reached_some_assignees_marks_only_them_and_resends_to_no_one() {
        let harness = Harness::new(FakeHost::with([open(1), open(2)]));
        harness.opened(1);
        harness.opened(2);
        let distributor = harness.distributor(&["amy", "bob"], config());
        let slot = Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap();
        let mut schedule = crate::reports::ReportSchedule::daily("pr-distribution", 9);

        harness.unreachable(&["bob"]);
        for tick in [slot, slot + chrono::Duration::minutes(5)] {
            if schedule.is_due(tick) && distributor.distribute(tick).await.is_ok() {
                schedule.last_sent_at = Some(tick);
            }
        }

        assert_eq!(
            harness.assignees(),
            vec![(1, "amy".to_string()), (2, "bob".to_string())]
        );
        assert_eq!(
            digested(&harness),
            vec!["amy"],
            "amy's digest must be sent once"
        );
        assert!(harness.active(1).notified_at.is_some());
        assert!(
            harness.active(2).notified_at.is_none(),
            "bob was not reached, so his assignment must stay unnotified"
        );

        harness.unreachable(&[]);
        let tomorrow = slot + chrono::Duration::days(1);
        assert!(schedule.is_due(tomorrow));
        distributor.distribute(tomorrow).await.unwrap();

        assert_eq!(digested(&harness), vec!["amy", "amy", "bob"]);
        assert!(harness.active(2).notified_at.is_some());
    }

    #[tokio::test]
    async fn a_missed_digest_assignee_is_pinged_once_the_digest_is_turned_off() {
        let harness = Harness::new(FakeHost::with([open(1), open(2)]));
        harness.opened(1);
        harness.opened(2);

        harness.unreachable(&["bob"]);
        harness
            .distributor(&["amy", "bob"], config())
            .distribute(Utc::now())
            .await
            .unwrap();
        harness.unreachable(&[]);
        harness
            .distributor(&["amy", "bob"], pings())
            .distribute(Utc::now())
            .await
            .unwrap();

        assert_eq!(harness.pinged(), vec![(url(2), "bob".to_string())]);
    }

    #[tokio::test]
    async fn a_digest_with_no_remote_channel_marks_nothing_and_is_sent_once_one_is() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened(1);

        let outcome = harness
            .console_only(&["amy"], config())
            .distribute(Utc::now())
            .await
            .expect("a run with no remote channel must not fail and be retried every tick");

        assert_eq!(outcome.listed, 1);
        assert!(harness.active(1).notified_at.is_none());

        harness
            .distributor(&["amy"], config())
            .distribute(Utc::now())
            .await
            .unwrap();

        assert_eq!(digested(&harness), vec!["amy"]);
        assert!(harness.active(1).notified_at.is_some());
    }

    #[tokio::test]
    async fn a_ping_with_no_remote_channel_marks_nothing_and_is_sent_once_one_is() {
        let harness = Harness::new(FakeHost::with([open(1), open(2)]));
        harness.opened(1);
        harness.opened(2);
        let console = harness.console_only(&["amy", "bob"], pings());

        console
            .assign_opened(&url(1), &requested_by("amy"))
            .await
            .unwrap();
        console
            .distribute(Utc::now())
            .await
            .expect("a run with no remote channel must not fail and be retried every tick");

        assert_eq!(harness.assignees().len(), 2);
        assert!(harness.active(1).notified_at.is_none());
        assert!(harness.active(2).notified_at.is_none());

        harness
            .distributor(&["amy", "bob"], pings())
            .distribute(Utc::now())
            .await
            .unwrap();

        let mut pinged = harness.pinged();
        pinged.sort();
        assert_eq!(
            pinged,
            vec![(url(1), "amy".to_string()), (url(2), "bob".to_string())]
        );
        assert!(harness.active(1).notified_at.is_some());
        assert!(harness.active(2).notified_at.is_some());
    }

    #[tokio::test]
    async fn merged_and_closed_prs_close_their_assignments() {
        let harness = Harness::new(FakeHost::with([open(1), open(2)]));
        harness.opened(1);
        harness.opened(2);
        let distributor = harness.distributor(&["amy", "bob"], config());
        distributor.distribute(Utc::now()).await.unwrap();

        harness.host.update(1, |pull_request| {
            pull_request.state = PullRequestState::Merged
        });
        harness.host.update(2, |pull_request| {
            pull_request.state = PullRequestState::Closed
        });
        harness.store.mark_closed("linear", "issue-2").unwrap();
        let outcome = distributor.distribute(Utc::now()).await.unwrap();

        assert_eq!(outcome.closed, 2);
        assert!(harness.assignees().is_empty());
    }

    #[tokio::test]
    async fn the_digest_rounds_up_the_prs_each_assignee_wrapped_up_since_the_last_run() {
        let harness = Harness::new(FakeHost::with([open(1), open(2), open(3)]));
        harness.opened(1);
        harness.opened(2);
        harness.opened(3);
        let distributor = harness.distributor(&["amy", "bob"], config());
        distributor.distribute(Utc::now()).await.unwrap();
        let assignees: HashMap<i64, String> = harness.assignees().into_iter().collect();
        let holder_of_two = if assignees.values().filter(|login| *login == "amy").count() == 2 {
            "amy"
        } else {
            "bob"
        };
        let (finished_by_holder, others): (Vec<i64>, Vec<i64>) =
            (1..=3).partition(|number| assignees[number] == holder_of_two);

        harness.host.update(finished_by_holder[0], |pull_request| {
            pull_request.state = PullRequestState::Merged
        });
        harness.host.update(others[0], |pull_request| {
            pull_request.state = PullRequestState::Closed
        });
        distributor.distribute(Utc::now()).await.unwrap();

        let latest = harness
            .recorder
            .digests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        let roundup = |login: &str| -> Vec<(i64, bool)> {
            latest
                .groups
                .iter()
                .find(|group| group.developer.login == login)
                .map(|group| {
                    group
                        .wrapped_up
                        .iter()
                        .map(|wrapped| (wrapped.pull_request.number, wrapped.merged))
                        .collect()
                })
                .unwrap_or_default()
        };
        let other = if holder_of_two == "amy" { "bob" } else { "amy" };
        assert_eq!(roundup(holder_of_two), vec![(finished_by_holder[0], true)]);
        assert_eq!(roundup(other), vec![(others[0], false)]);
        let other_group = latest
            .groups
            .iter()
            .find(|group| group.developer.login == other)
            .expect("an assignee with nothing left open still gets their roundup");
        assert!(other_group.pull_requests.is_empty());

        distributor.distribute(Utc::now()).await.unwrap();
        let latest = harness
            .recorder
            .digests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert!(
            latest
                .groups
                .iter()
                .all(|group| group.wrapped_up.is_empty()),
            "a wrapped-up PR is rounded up once"
        );
    }

    #[tokio::test]
    async fn a_roundup_that_missed_its_assignee_is_sent_on_the_next_run() {
        let harness = Harness::new(FakeHost::with([open(1), open(2)]));
        harness.opened(1);
        harness.opened(2);
        let distributor = harness.distributor(&["amy", "bob"], config());
        distributor.distribute(Utc::now()).await.unwrap();
        let assignees: HashMap<i64, String> = harness.assignees().into_iter().collect();
        let unreachable = assignees[&1].clone();
        harness.host.update(1, |pull_request| {
            pull_request.state = PullRequestState::Merged
        });
        harness
            .recorder
            .unreachable
            .lock()
            .unwrap()
            .insert(unreachable.clone());

        let missed = distributor.distribute(Utc::now()).await;
        let rounded_up = |harness: &Harness| -> bool {
            harness
                .recorder
                .digests
                .lock()
                .unwrap()
                .iter()
                .any(|digest| {
                    digest.groups.iter().any(|group| {
                        group.developer.login == unreachable
                            && group
                                .wrapped_up
                                .iter()
                                .any(|wrapped| wrapped.pull_request.number == 1)
                    })
                })
        };
        assert!(!rounded_up(&harness));
        assert!(
            harness.assignees().iter().any(|(number, _)| *number == 1),
            "a merged PR whose roundup missed its assignee keeps its assignment: {missed:?}"
        );

        harness.recorder.unreachable.lock().unwrap().clear();
        let outcome = distributor.distribute(Utc::now()).await.unwrap();

        assert!(rounded_up(&harness));
        assert_eq!(outcome.closed, 1);
        assert!(harness.assignees().iter().all(|(number, _)| *number != 1));
    }

    #[tokio::test]
    async fn an_assignment_outlives_claudear_losing_track_of_an_open_pr() {
        let harness = Harness::new(FakeHost::with([open(1)]));
        harness.opened(1);
        let distributor = harness.distributor(&["amy", "bob"], config());
        distributor
            .assign_opened(&url(1), &requested_by("bob"))
            .await
            .unwrap();
        harness.store.reset_attempt("linear", "issue-1").unwrap();

        let outcome = distributor.distribute(Utc::now()).await.unwrap();

        assert_eq!(outcome.closed, 0);
        assert_eq!(outcome.listed, 1);
        assert_eq!(harness.assignees(), vec![(1, "bob".to_string())]);
    }

    #[tokio::test]
    async fn digest_lists_only_prs_waiting_on_their_assignee() {
        let harness = Harness::new(FakeHost::with((1..=4).map(open)));
        (1..=4).for_each(|number| harness.opened(number));
        let distributor = harness.distributor(&["amy"], config());
        distributor.distribute(Utc::now()).await.unwrap();

        harness
            .host
            .update(1, |pull_request| pull_request.draft = true);
        harness.host.update(2, |pull_request| {
            pull_request.reviews = vec![Review {
                reviewer: "amy".to_string(),
                verdict: Verdict::ChangesRequested,
                commit: Some("head-2".to_string()),
            }];
        });
        harness.host.update(3, |pull_request| {
            pull_request.auto_merge = true;
            pull_request.reviews = vec![Review {
                reviewer: "bob".to_string(),
                verdict: Verdict::Approved,
                commit: Some("head-3".to_string()),
            }];
        });
        let outcome = distributor.distribute(Utc::now()).await.unwrap();

        assert_eq!(outcome.listed, 1);
        let digests = harness.recorder.digests.lock().unwrap();
        let last = digests.last().unwrap();
        assert_eq!(last.groups.len(), 1);
        assert_eq!(last.groups[0].pull_requests[0].url, url(4));
        assert_eq!(harness.assignees().len(), 4);
    }

    #[tokio::test]
    async fn unassigned_drafts_wait_and_nothing_to_list_sends_nothing() {
        let mut draft = open(1);
        draft.draft = true;
        let harness = Harness::new(FakeHost::with([draft]));
        harness.opened(1);

        let outcome = harness
            .distributor(&["amy"], config())
            .distribute(Utc::now())
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::default());
        assert!(harness.assignees().is_empty());
        assert!(harness.recorder.digests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_pr_keeps_its_assignment_and_load() {
        let harness = Harness::new(FakeHost::with([open(1), open(2), open(3)]));
        (1..=3).for_each(|number| harness.opened(number));
        let distributor = harness.distributor(&["amy", "bob"], config());
        distributor
            .assign_opened(&url(1), &requested_by("amy"))
            .await
            .unwrap();
        harness.host.pull_requests.lock().unwrap().remove(&1);

        distributor.distribute(Utc::now()).await.unwrap();

        assert_eq!(
            harness.assignees(),
            vec![
                (1, "amy".to_string()),
                (2, "bob".to_string()),
                (3, "amy".to_string()),
            ]
        );
    }

    fn amy(discord_id: &str) -> claudear_config::config::UserConfig {
        claudear_config::config::UserConfig {
            github_usernames: vec!["amy".to_string()],
            discord_id: Some(discord_id.to_string()),
            ..claudear_config::config::UserConfig::default()
        }
    }

    #[test]
    fn from_config_needs_the_switch_a_host_and_a_roster() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let host: Arc<dyn PullRequestHost> = Arc::new(FakeHost::default());
        let notifier: Arc<dyn Notifier> = Arc::new(Recorder::default());
        let registry = UserRegistry::new(HashMap::from([(
            "amy".to_string(),
            amy("111111111111111111"),
        )]));
        let mut config = Config::default();

        let build =
            |config: &Config, registry: &UserRegistry, host: Option<Arc<dyn PullRequestHost>>| {
                Distributor::from_config(config, registry, store.clone(), host, notifier.clone())
                    .is_some()
            };

        assert!(!build(&config, &registry, Some(host.clone())));
        config.pr_distribution.enabled = true;
        assert!(!build(&config, &registry, None));
        assert!(build(&config, &registry, Some(host.clone())));
        config.pr_distribution.excluded = vec!["AMY".to_string()];
        assert!(!build(&config, &registry, Some(host.clone())));
        config.deploy_qa.github_discord_map_path =
            Some("/nonexistent/github-discord-map.json".to_string());
        assert!(!build(&config, &registry, Some(host.clone())));

        let directory = tempfile::tempdir().unwrap();
        let map = directory.path().join("github-discord-map.json");
        std::fs::write(
            &map,
            r#"{"byGithubLogin": {"bob": {"discordUserId": "222222222222222222"}}}"#,
        )
        .unwrap();
        config.deploy_qa.github_discord_map_path = Some(map.display().to_string());
        assert!(build(
            &config,
            &UserRegistry::new(HashMap::new()),
            Some(host)
        ));
    }

    #[tokio::test]
    async fn console_edits_change_who_is_pinged_without_a_restart() {
        let harness = Harness::new(FakeHost::with([open(1), open(2)]));
        harness.opened(1);
        harness.opened(2);
        let registry = UserRegistry::new(HashMap::from([(
            "amy".to_string(),
            amy("111111111111111111"),
        )]));
        let mut config = Config::default();
        config.pr_distribution.enabled = true;
        let distributor = |registry: &UserRegistry| {
            Distributor::from_config(
                &config,
                registry,
                harness.store.clone(),
                Some(harness.host.clone() as Arc<dyn PullRequestHost>),
                harness.notifier.clone(),
            )
            .unwrap()
        };

        distributor(&registry)
            .assign_opened(&url(1), &requested_by("amy"))
            .await
            .unwrap();
        registry.replace_console(vec![claudear_core::identity::ConsoleIdentity {
            user_id: 1,
            login_email: "amy@example.com".to_string(),
            name: "Amy".to_string(),
            slug: Some("amy".to_string()),
            identities: claudear_core::identity::Identities {
                github_usernames: vec!["amy".to_string()],
                discord_id: Some("222222222222222222".to_string()),
                ..claudear_core::identity::Identities::default()
            },
            updated_at: "2026-11-04 10:00:00".to_string(),
        }]);
        distributor(&registry)
            .assign_opened(&url(2), &requested_by("amy"))
            .await
            .unwrap();

        let pinged: Vec<Option<String>> = harness
            .recorder
            .notices
            .lock()
            .unwrap()
            .iter()
            .map(|notice| notice.developer.discord_id.clone())
            .collect();
        assert_eq!(
            pinged,
            vec![
                Some("111111111111111111".to_string()),
                Some("222222222222222222".to_string()),
            ]
        );
    }
}
