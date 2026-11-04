//! Read-only validation of a report against current code (Phase A), and a
//! git-history check for an existing fix when it does not reproduce (Phase B).

use super::history::{self, History};
use super::notify::ReviewReason;
use super::prompt::{self, HistoryCommit, ReportPrompt};
use super::scrub::scrub_assessment;
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
}

impl Verdict {
    pub(crate) fn review_reason(&self) -> Option<ReviewReason> {
        match self {
            Self::NeedsHuman(reason) => Some(*reason),
            Self::FixedAfterReport(_) => Some(ReviewReason::FixFoundInHistory),
            _ => None,
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
        if let Some(affected) = self.redirected_repository(&phase, &checkout) {
            let redirected = resolve_repo_for_cascade(self.inferrer.as_ref(), &affected);
            if redirected.is_resolved() {
                checkout = self.checkout(&redirected).await?;
                phase = self.phase_a(report, &checkout).await?;
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

    fn redirected_repository(&self, phase: &PhaseA, checkout: &Checkout) -> Option<String> {
        let affected = phase.affected_repository.trim();
        let plausible = affected.split('/').count() == 2
            && affected
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_./".contains(character))
            && !affected.contains("..");
        (plausible && !affected.eq_ignore_ascii_case(&checkout.repository))
            .then(|| affected.to_string())
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
}
