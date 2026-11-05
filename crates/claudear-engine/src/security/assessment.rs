//! Read-only validation of a report against current code (Phase A), and a
//! git-history check for an existing fix when it does not reproduce (Phase B).

use super::history::{self, History};
use super::notify::ReviewReason;
use super::prompt::{self, HistoryCommit, ReportPrompt};
use super::scrub::{scrub_assessment, scrub_internal};
use super::SecurityInbox;
use chrono::{DateTime, NaiveDate, Utc};
use claudear_analysis::inference::{resolve_repo_for_cascade, RepoResolution};
use claudear_analysis::repo::GitOps;
use claudear_core::error::{Error, Result};
use claudear_core::security::SecuritySeverity;
use claudear_integrations::runner::SecuritySandbox;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) const NO_TOOLS_TIMEOUT: Duration = Duration::from_secs(120);
const NO_TOOLS_QUERIES_PER_REPORT: u32 = 3;
const PHASE_A_RUNS_PER_REPORT: u32 = 2;
const REPOSITORY_SYNC_ALLOWANCE: Duration = Duration::from_secs(10 * 60);
const NO_FIX_CHOICE: &str = "none";
const MERGED_AT_DETAIL_KEY: &str = "merged_at";
const FIXED_BY_COMMIT_DETAIL_KEY: &str = "fixed_by_commit";
const NAMED_REPOSITORY_DETAIL_KEY: &str = "named_repository";
const URL_SCHEMES: &[&str] = &["https://", "http://", "ssh://", "git://"];
const SSH_USER: &str = "git@";
const WWW_SUBDOMAIN: &str = "www.";
const GITHUB_HOST: &str = "github.com";
const HOST_SEPARATORS: [char; 2] = ['/', ':'];
const URL_SUFFIX_MARKERS: [char; 2] = ['?', '#'];
const REMOTE_MARKERS: [char; 2] = [':', '@'];
const REPOSITORY_SUFFIX: &str = ".git";

/// The longest one assessment can legitimately hold a report in `Assessing`:
/// both Phase A runs, screening, the history check, the duplicate judge and
/// the repository syncs.
pub(crate) fn assessment_deadline(assessment_timeout: Duration) -> Duration {
    assessment_timeout
        .saturating_mul(PHASE_A_RUNS_PER_REPORT)
        .saturating_add(NO_TOOLS_TIMEOUT.saturating_mul(NO_TOOLS_QUERIES_PER_REPORT))
        .saturating_add(history::WORST_CASE_DURATION)
        .saturating_add(REPOSITORY_SYNC_ALLOWANCE)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Assessment {
    pub title: String,
    pub summary: String,
    pub root_cause: String,
    pub suggested_fix: String,
    pub evidence: String,
    pub public_reason: String,
    pub severity: Option<SecuritySeverity>,
}

impl Assessment {
    /// The event detail the fix pipeline later rebuilds its brief from.
    pub(crate) fn detail(&self, outcome: &str, repository: Option<&str>) -> Value {
        json!({
            "outcome": outcome,
            "title": self.title,
            "summary": self.summary,
            "root_cause": self.root_cause,
            "suggested_fix": self.suggested_fix,
            "evidence": self.evidence,
            "severity": self.severity,
            "repository": repository,
        })
    }

    pub(crate) fn from_detail(detail: &Value) -> Self {
        let text = |key: &str| {
            detail
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Self {
            title: text("title"),
            summary: text("summary"),
            root_cause: text("root_cause"),
            suggested_fix: text("suggested_fix"),
            evidence: text("evidence"),
            public_reason: String::new(),
            severity: detail
                .get("severity")
                .and_then(Value::as_str)
                .and_then(|value| value.parse().ok()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergedFix {
    pub commit: String,
    pub merged_at: DateTime<Utc>,
}

impl MergedFix {
    pub(crate) fn record(&self, detail: &mut Value) {
        detail[FIXED_BY_COMMIT_DETAIL_KEY] = self.commit.clone().into();
        detail[MERGED_AT_DETAIL_KEY] = self.merged_at.to_rfc3339().into();
    }

    pub(crate) fn from_detail(detail: &Value) -> Option<Self> {
        let commit = detail.get(FIXED_BY_COMMIT_DETAIL_KEY)?.as_str()?;
        history::validate_sha(commit).ok()?;
        let merged_at = DateTime::parse_from_rfc3339(detail.get(MERGED_AT_DETAIL_KEY)?.as_str()?)
            .ok()?
            .with_timezone(&Utc);
        Some(Self {
            commit: commit.to_string(),
            merged_at,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Valid,
    /// The report does not reproduce because a fix landed after it arrived:
    /// it may be valid, but only an admin can confirm that.
    FixedAfterReport(MergedFix),
    NotReproducible,
    AlreadyFixed {
        commit: String,
        fixed_on: NaiveDate,
    },
    NotVulnerability,
    OutOfScope,
    NeedsHuman(ReviewReason),
    /// The assessment placed the defect in a repository that is not indexed,
    /// so the checked repository's verdict cannot be trusted.
    UnindexedRepository(String),
    /// The re-run in the repository the first run named placed the defect
    /// somewhere else again, so neither run can be trusted.
    ConflictingRepository(String),
}

impl Verdict {
    pub(crate) fn review_reason(&self) -> Option<ReviewReason> {
        match self {
            Self::NeedsHuman(reason) => Some(*reason),
            Self::FixedAfterReport(_) => Some(ReviewReason::FixFoundInHistory),
            Self::UnindexedRepository(_) => Some(ReviewReason::UnindexedRepository),
            Self::ConflictingRepository(_) => Some(ReviewReason::AssessmentInconclusive),
            _ => None,
        }
    }

    /// Record what an admin needs to review this verdict in its event detail.
    pub(crate) fn record(&self, detail: &mut Value) {
        match self {
            Self::FixedAfterReport(merged) => merged.record(detail),
            Self::UnindexedRepository(repository) | Self::ConflictingRepository(repository) => {
                detail[NAMED_REPOSITORY_DETAIL_KEY] = scrub_internal(repository).into();
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AssessmentResult {
    pub assessment: Assessment,
    pub verdict: Verdict,
    pub repository: Option<String>,
}

pub(crate) struct ReportText {
    pub subject: String,
    pub body: String,
    pub attachments: Vec<String>,
    pub reported_at: DateTime<Utc>,
}

impl ReportText {
    pub(crate) fn prompt(&self) -> ReportPrompt<'_> {
        ReportPrompt {
            subject: &self.subject,
            body: &self.body,
            attachments: &self.attachments,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentOutcome {
    Valid,
    NotReproducible,
    NotVulnerability,
    OutOfScope,
    NeedsHuman,
}

#[derive(Debug, Deserialize)]
struct PhaseA {
    outcome: AgentOutcome,
    #[serde(default)]
    affected_repository: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    public_reason: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    root_cause: String,
    #[serde(default)]
    suggested_fix: String,
    #[serde(default)]
    evidence: String,
    #[serde(default)]
    history_paths: Vec<String>,
    #[serde(default)]
    history_terms: Vec<String>,
}

impl PhaseA {
    fn assessment(&self) -> Assessment {
        let mut assessment = Assessment {
            title: self.title.clone(),
            summary: self.summary.clone(),
            root_cause: self.root_cause.clone(),
            suggested_fix: self.suggested_fix.clone(),
            evidence: self.evidence.clone(),
            public_reason: self.public_reason.clone(),
            severity: self.severity.parse().ok(),
        };
        scrub_assessment(&mut assessment);
        assessment
    }

    /// A result that places the defect in no repository, for an admin to
    /// settle.
    fn unresolved(&self, verdict: Verdict) -> AssessmentResult {
        AssessmentResult {
            assessment: self.assessment(),
            verdict,
            repository: None,
        }
    }
}

/// The repository a Phase A run placed the defect in, relative to the
/// repository it checked.
#[derive(Debug, PartialEq, Eq)]
enum NamedRepository<'a> {
    Checked,
    Other(&'a str),
    Unrecognised(&'a str),
}

impl<'a> NamedRepository<'a> {
    fn of(phase: &'a PhaseA, checkout: &Checkout) -> Self {
        let reference = phase.affected_repository.trim();
        if reference.is_empty() || reference.eq_ignore_ascii_case(checkout.repository.trim()) {
            return Self::Checked;
        }
        match repository_name(reference) {
            Some(name) if is_same_repository(name, &checkout.repository) => Self::Checked,
            Some(name) => Self::Other(name),
            None => Self::Unrecognised(reference),
        }
    }

    /// What the run named, when it is not the checked repository.
    fn elsewhere(&self) -> Option<&'a str> {
        match self {
            Self::Checked => None,
            Self::Other(reference) | Self::Unrecognised(reference) => Some(reference),
        }
    }
}

fn is_same_repository(name: &str, checked: &str) -> bool {
    name.eq_ignore_ascii_case(repository_name(checked).unwrap_or_else(|| checked.trim()))
}

/// The repository name in an agent-supplied reference: `owner/name` from a
/// GitHub URL or SSH remote, or a bare slash-separated name of at least two
/// segments, without surrounding slashes or a `.git` suffix.
fn repository_name(reference: &str) -> Option<&str> {
    let trimmed = reference.trim();
    let name = match github_path(trimmed) {
        Some(path) => github_repository(path)?,
        None if trimmed.contains(REMOTE_MARKERS) => return None,
        None => trimmed.trim_matches('/'),
    };
    let name = name.strip_suffix(REPOSITORY_SUFFIX).unwrap_or(name);
    let plausible = name.split('/').count() >= 2
        && name.split('/').all(|segment| !segment.is_empty())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_./".contains(character))
        && !name.contains("..");
    plausible.then_some(name)
}

/// The path after the host when `reference` is a GitHub URL or SSH remote.
fn github_path(reference: &str) -> Option<&str> {
    let unschemed = URL_SCHEMES
        .iter()
        .find_map(|scheme| strip_prefix_ignoring_case(reference, scheme))
        .unwrap_or(reference);
    let host = strip_prefix_ignoring_case(unschemed, SSH_USER).unwrap_or(unschemed);
    let host = strip_prefix_ignoring_case(host, WWW_SUBDOMAIN).unwrap_or(host);
    strip_prefix_ignoring_case(host, GITHUB_HOST)?.strip_prefix(HOST_SEPARATORS)
}

/// `owner/name` from a GitHub path, ignoring anything below the repository
/// such as `/tree/main/src`, a query or a fragment.
fn github_repository(path: &str) -> Option<&str> {
    let path = path
        .split(URL_SUFFIX_MARKERS)
        .next()
        .unwrap_or_default()
        .trim_start_matches('/');
    let mut segments = path.splitn(3, '/');
    let owner = segments.next()?;
    let name = segments.next()?;
    path.get(..owner.len() + 1 + name.len())
}

fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

#[derive(Debug, Deserialize)]
struct PhaseB {
    fixed_by_commit: String,
}

struct Checkout {
    project_dir: PathBuf,
    repository: String,
    default_branch: String,
}

impl SecurityInbox {
    pub(crate) async fn assess(
        &self,
        report: &ReportText,
        resolution: &RepoResolution,
    ) -> Result<AssessmentResult> {
        let mut checkout = self.checkout(resolution).await?;
        let mut phase = self.phase_a(report, &checkout).await?;
        let named = NamedRepository::of(&phase, &checkout);
        if let Some(reference) = named.elsewhere() {
            let Some(redirected) = self.redirection(&named)? else {
                let verdict = Verdict::UnindexedRepository(scrub_internal(reference));
                return Ok(phase.unresolved(verdict));
            };
            checkout = self.checkout(&redirected).await?;
            phase = self.phase_a(report, &checkout).await?;
            if let Some(reference) = NamedRepository::of(&phase, &checkout).elsewhere() {
                let verdict = Verdict::ConflictingRepository(scrub_internal(reference));
                return Ok(phase.unresolved(verdict));
            }
        }
        let assessment = phase.assessment();
        let verdict = match phase.outcome {
            AgentOutcome::Valid => Verdict::Valid,
            AgentOutcome::NotVulnerability => Verdict::NotVulnerability,
            AgentOutcome::OutOfScope => Verdict::OutOfScope,
            AgentOutcome::NeedsHuman => Verdict::NeedsHuman(ReviewReason::AssessmentInconclusive),
            AgentOutcome::NotReproducible => {
                match self.phase_b(report, &phase, &assessment, &checkout).await {
                    Verdict::NotReproducible if !report.attachments.is_empty() => {
                        Verdict::NeedsHuman(ReviewReason::AttachmentEvidence)
                    }
                    verdict => verdict,
                }
            }
        };
        Ok(AssessmentResult {
            assessment,
            verdict,
            repository: Some(checkout.repository),
        })
    }

    async fn checkout(&self, resolution: &RepoResolution) -> Result<Checkout> {
        let fallback;
        let resolution = match (resolution, self.config.default_repository.as_deref()) {
            (RepoResolution::Skip { .. }, Some(default)) => {
                fallback = resolve_repo_for_cascade(self.inferrer.as_ref(), default);
                &fallback
            }
            _ => resolution,
        };
        let RepoResolution::Resolved {
            project_dir,
            repo_name,
            scm_url,
            ..
        } = resolution
        else {
            return Err(Error::Other(
                "no repository resolved for the security assessment".to_string(),
            ));
        };
        let default_branch = GitOps::ensure_repo_synced(project_dir, scm_url).await?;
        Ok(Checkout {
            project_dir: project_dir.clone(),
            repository: repo_name.clone(),
            default_branch,
        })
    }

    /// The indexed repository a Phase A run named instead of the checked one.
    fn redirection(&self, named: &NamedRepository<'_>) -> Result<Option<RepoResolution>> {
        match named {
            NamedRepository::Other(name) => self.indexed_resolution(name),
            NamedRepository::Checked | NamedRepository::Unrecognised(_) => Ok(None),
        }
    }

    /// The indexed repository named `repository`, ignoring case, or `None`
    /// when the index does not hold it.
    fn indexed_resolution(&self, repository: &str) -> Result<Option<RepoResolution>> {
        let Some(inferrer) = self.inferrer.as_ref() else {
            return Ok(None);
        };
        let name = inferrer.with_index(|index| {
            Ok(index
                .list()
                .into_iter()
                .find(|indexed| indexed.name.eq_ignore_ascii_case(repository))
                .map(|indexed| indexed.name.clone()))
        })?;
        Ok(name
            .map(|name| resolve_repo_for_cascade(Some(inferrer), &name))
            .filter(RepoResolution::is_resolved))
    }

    async fn phase_a(&self, report: &ReportText, checkout: &Checkout) -> Result<PhaseA> {
        let prompt =
            prompt::assessment_prompt(&report.prompt(), &checkout.repository, &self.config.scope);
        let value = self
            .agent
            .security_query(
                &prompt,
                prompt::ASSESSMENT_SCHEMA,
                &checkout.project_dir,
                SecuritySandbox::RepositoryRead,
                self.config.assessment_timeout(),
            )
            .await?;
        serde_json::from_value(value)
            .map_err(|error| Error::Other(format!("malformed assessment: {error}")))
    }

    async fn phase_b(
        &self,
        report: &ReportText,
        phase: &PhaseA,
        assessment: &Assessment,
        checkout: &Checkout,
    ) -> Verdict {
        match self
            .find_existing_fix(report, phase, assessment, checkout)
            .await
        {
            Ok(verdict) => verdict,
            Err(error) => {
                tracing::warn!(error = %error, "security history check failed");
                Verdict::NeedsHuman(ReviewReason::UnverifiableFix)
            }
        }
    }

    async fn find_existing_fix(
        &self,
        report: &ReportText,
        phase: &PhaseA,
        assessment: &Assessment,
        checkout: &Checkout,
    ) -> Result<Verdict> {
        let history = History::new(&checkout.project_dir, &checkout.default_branch)?;
        let commits = self.candidate_commits(&history, phase).await?;
        if commits.is_empty() {
            return Ok(Verdict::NotReproducible);
        }
        let listing: Vec<HistoryCommit<'_>> = commits
            .iter()
            .map(|commit| HistoryCommit {
                sha: &commit.sha,
                committed_at: &commit.committed_at,
                subject: &commit.subject,
            })
            .collect();
        let shas: Vec<String> = commits.iter().map(|commit| commit.sha.clone()).collect();
        let sandbox = tempfile::tempdir()?;
        let value = self
            .agent
            .security_query(
                &prompt::history_prompt(&assessment.summary, &listing),
                &prompt::history_schema(&shas),
                sandbox.path(),
                SecuritySandbox::NoTools,
                NO_TOOLS_TIMEOUT,
            )
            .await?;
        let choice: PhaseB = serde_json::from_value(value)
            .map_err(|error| Error::Other(format!("malformed history verdict: {error}")))?;
        if choice.fixed_by_commit == NO_FIX_CHOICE {
            return Ok(Verdict::NotReproducible);
        }
        if !shas.contains(&choice.fixed_by_commit) {
            return Err(Error::Other(
                "history verdict named an unlisted commit".to_string(),
            ));
        }
        if !history.is_ancestor(&choice.fixed_by_commit).await? {
            return Err(Error::Other(
                "chosen fix is not on the default branch".to_string(),
            ));
        }
        let (commit, landed_at) = history.landed_at(&choice.fixed_by_commit).await?;
        Ok(if landed_at < report.reported_at {
            Verdict::AlreadyFixed {
                commit,
                fixed_on: landed_at.date_naive(),
            }
        } else {
            Verdict::FixedAfterReport(MergedFix {
                commit,
                merged_at: landed_at,
            })
        })
    }

    async fn candidate_commits(
        &self,
        history: &History,
        phase: &PhaseA,
    ) -> Result<Vec<history::Commit>> {
        let paths = history::validate_paths(history.project_dir(), &phase.history_paths);
        let terms = history::validate_terms(&phase.history_terms);
        let mut commits = history.commits_touching(&paths).await?;
        for term in &terms {
            commits.extend(history.commits_mentioning(term).await?);
        }
        let mut seen = HashSet::new();
        commits.retain(|commit| seen.insert(commit.sha.clone()));
        Ok(commits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deadline_covers_every_step_of_one_assessment() {
        let timeout = Duration::from_secs(900);
        let deadline = assessment_deadline(timeout);
        assert!(deadline > timeout.saturating_mul(2) + NO_TOOLS_TIMEOUT.saturating_mul(3));
        assert!(deadline >= timeout.saturating_mul(2) + history::WORST_CASE_DURATION);
        assert_eq!(assessment_deadline(Duration::MAX), Duration::MAX);
    }

    #[test]
    fn a_merged_fix_round_trips_through_event_detail() {
        let merged = MergedFix {
            commit: "a".repeat(40),
            merged_at: DateTime::parse_from_rfc3339("2026-11-06T09:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let mut detail = json!({ "reason": "fix_found_in_history" });
        merged.record(&mut detail);
        assert_eq!(MergedFix::from_detail(&detail), Some(merged));
        detail[FIXED_BY_COMMIT_DETAIL_KEY] = "--output".into();
        assert_eq!(MergedFix::from_detail(&detail), None);
        assert_eq!(MergedFix::from_detail(&json!({})), None);
    }

    #[test]
    fn repository_references_normalise_to_a_repository_name() {
        for (reference, expected) in [
            ("appwrite/console", Some("appwrite/console")),
            ("  Appwrite/Console  ", Some("Appwrite/Console")),
            ("/appwrite/console/", Some("appwrite/console")),
            ("appwrite/console.git", Some("appwrite/console")),
            ("group/subgroup/project", Some("group/subgroup/project")),
            (
                "https://github.com/appwrite/console",
                Some("appwrite/console"),
            ),
            (
                "HTTPS://GitHub.com/appwrite/console.git",
                Some("appwrite/console"),
            ),
            (
                "http://github.com/appwrite/console/",
                Some("appwrite/console"),
            ),
            ("github.com/appwrite/console", Some("appwrite/console")),
            ("www.github.com/appwrite/console", Some("appwrite/console")),
            (
                "https://www.github.com/appwrite/console",
                Some("appwrite/console"),
            ),
            (
                "git@github.com:appwrite/console.git",
                Some("appwrite/console"),
            ),
            ("git@github.com:appwrite/console", Some("appwrite/console")),
            (
                "ssh://git@github.com/appwrite/console.git",
                Some("appwrite/console"),
            ),
            (
                "git://github.com/appwrite/console.git",
                Some("appwrite/console"),
            ),
            (
                "https://github.com/appwrite/console/tree/main/src",
                Some("appwrite/console"),
            ),
            (
                "https://github.com/appwrite/console/blob/main/src/app.ts#L4",
                Some("appwrite/console"),
            ),
            (
                "https://github.com/appwrite/console?tab=readme",
                Some("appwrite/console"),
            ),
            (
                "https://github.com/appwrite/console#readme",
                Some("appwrite/console"),
            ),
            ("", None),
            ("   ", None),
            ("console", None),
            ("not a repository", None),
            ("appwrite/", None),
            ("/console", None),
            ("appwrite//console", None),
            ("appwrite/../etc", None),
            ("appwrite/con sole", None),
            ("https://github.com/appwrite", None),
            ("https://github.com/", None),
            ("https://github.company.com/appwrite/console", None),
            ("https://gitlab.com/appwrite/console", None),
            ("git@gitlab.com:appwrite/console.git", None),
            ("file:///srv/appwrite/console", None),
        ] {
            assert_eq!(repository_name(reference), expected, "{reference:?}");
        }
    }

    fn checkout(repository: &str) -> Checkout {
        Checkout {
            project_dir: PathBuf::new(),
            repository: repository.to_string(),
            default_branch: "main".to_string(),
        }
    }

    fn phase(affected: &str) -> PhaseA {
        serde_json::from_value(json!({
            "outcome": "valid",
            "affected_repository": affected,
        }))
        .unwrap()
    }

    #[test]
    fn only_an_empty_or_matching_name_keeps_the_checked_repository() {
        let checkout = checkout("appwrite/appwrite");
        for affected in [
            "",
            "  ",
            "Appwrite/Appwrite",
            "https://github.com/appwrite/appwrite.git",
            "git@github.com:appwrite/appwrite.git",
            "www.github.com/appwrite/appwrite",
            "https://github.com/appwrite/appwrite/tree/main/src",
        ] {
            assert_eq!(
                NamedRepository::of(&phase(affected), &checkout),
                NamedRepository::Checked,
                "{affected:?}"
            );
        }
        for affected in [
            "appwrite/console",
            "git@github.com:appwrite/console.git",
            "https://github.com/appwrite/console/tree/main/src",
            "appwrite/appwrite/src",
        ] {
            let named = phase(affected);
            let other = NamedRepository::of(&named, &checkout);
            assert!(matches!(other, NamedRepository::Other(_)), "{affected:?}");
            assert!(other.elsewhere().is_some());
        }
        for affected in [
            "appwrite",
            "console",
            "not a repository",
            "https://gitlab.com/appwrite/appwrite",
        ] {
            let named = phase(affected);
            assert_eq!(
                NamedRepository::of(&named, &checkout),
                NamedRepository::Unrecognised(affected),
                "{affected:?}"
            );
        }
    }

    #[test]
    fn a_multi_segment_checked_repository_is_compared_in_full() {
        let checkout = checkout("Group/Subgroup/Project");
        assert_eq!(
            NamedRepository::of(&phase("group/subgroup/project"), &checkout),
            NamedRepository::Checked
        );
        assert_eq!(
            NamedRepository::of(&phase("group/subgroup/other"), &checkout),
            NamedRepository::Other("group/subgroup/other")
        );
        assert_eq!(
            NamedRepository::of(&phase("subgroup/project"), &checkout),
            NamedRepository::Other("subgroup/project")
        );
    }

    #[test]
    fn a_checked_repository_that_does_not_normalise_is_compared_verbatim() {
        let checkout = checkout("legacy");
        assert_eq!(
            NamedRepository::of(&phase(" Legacy "), &checkout),
            NamedRepository::Checked
        );
        assert_eq!(
            NamedRepository::of(&phase("legacy-other"), &checkout),
            NamedRepository::Unrecognised("legacy-other")
        );
        assert_eq!(
            NamedRepository::of(&phase("appwrite/console"), &checkout),
            NamedRepository::Other("appwrite/console")
        );
        assert_eq!(
            NamedRepository::of(&phase(""), &checkout),
            NamedRepository::Checked
        );
    }

    #[test]
    fn an_unindexed_repository_records_its_scrubbed_name() {
        let mut detail = json!({});
        Verdict::UnindexedRepository("appwrite/console".to_string()).record(&mut detail);
        assert_eq!(
            detail[NAMED_REPOSITORY_DETAIL_KEY],
            json!("appwrite/console")
        );
        Verdict::UnindexedRepository("appwrite-labs/cloud".to_string()).record(&mut detail);
        assert_eq!(
            detail[NAMED_REPOSITORY_DETAIL_KEY],
            json!(super::super::scrub::REDACTED)
        );
        assert_eq!(
            Verdict::UnindexedRepository(String::new()).review_reason(),
            Some(ReviewReason::UnindexedRepository)
        );
    }

    #[test]
    fn a_conflicting_repository_is_inconclusive_and_records_its_scrubbed_name() {
        let mut detail = json!({});
        Verdict::ConflictingRepository("appwrite/appwrite".to_string()).record(&mut detail);
        assert_eq!(
            detail[NAMED_REPOSITORY_DETAIL_KEY],
            json!("appwrite/appwrite")
        );
        Verdict::ConflictingRepository("appwrite-labs/cloud".to_string()).record(&mut detail);
        assert_eq!(
            detail[NAMED_REPOSITORY_DETAIL_KEY],
            json!(super::super::scrub::REDACTED)
        );
        assert_eq!(
            Verdict::ConflictingRepository(String::new()).review_reason(),
            Some(ReviewReason::AssessmentInconclusive)
        );
    }
}
