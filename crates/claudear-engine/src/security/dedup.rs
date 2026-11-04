//! First-reporter deduplication. Only ever runs while holding the inbox's
//! `registration` lock, so two valid reports never both become first.

use super::assessment::{Assessment, MergedFix, NO_TOOLS_TIMEOUT};
use super::prompt::{self, DuplicateCandidate};
use super::SecurityInbox;
use async_trait::async_trait;
use claudear_analysis::feedback::EmbeddingClient;
use claudear_core::error::{Error, Result};
use claudear_core::security::{
    select_first_reporter, FirstReport, FirstReporterDecision, NewSecurityEvent,
    NewSecurityVulnerability, SecurityEventKind, SecurityOutcome, SecurityReport,
    SecurityReportChanges, SecurityReportState, SecurityVulnerability,
    SecurityVulnerabilityCandidate, VulnerabilityState,
};
use claudear_core::types::cosine_similarity_f32;
use claudear_integrations::runner::SecuritySandbox;
use serde::Deserialize;

const RECENT_CANDIDATES: usize = 20;
const MAX_COMPARED_VULNERABILITIES: usize = 1000;
const NO_DUPLICATE_CHOICE: &str = "none";

#[async_trait]
pub(crate) trait TextEmbedder: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

#[async_trait]
impl TextEmbedder for EmbeddingClient {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        EmbeddingClient::embed(self, text).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistrationKind {
    /// A new vulnerability with this report as its first reporter.
    Created,
    /// The report was already the first reporter of an existing vulnerability.
    Adopted,
    /// The report predates the existing first reporter and took its place.
    Replaced,
    /// The report duplicates a vulnerability first reported by someone else.
    Duplicate,
    /// The report was already linked to a vulnerability it does not first
    /// report, for example one whose first reporter was replaced.
    Relinked,
}

#[derive(Debug, Clone)]
pub(crate) struct Registration {
    pub kind: RegistrationKind,
    pub vulnerability: SecurityVulnerability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssessmentOrigin {
    Agent,
    Admin,
}

impl AssessmentOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Admin => "admin",
        }
    }

    /// The state a report holds while this origin registers it.
    fn registering_state(self) -> SecurityReportState {
        match self {
            Self::Agent => SecurityReportState::Assessing,
            Self::Admin => SecurityReportState::Registering,
        }
    }
}

#[derive(Debug, Deserialize)]
struct DuplicateVerdict {
    duplicate_of: String,
}

pub(crate) struct ValidReport<'a> {
    pub report: &'a SecurityReport,
    pub assessment: &'a Assessment,
    pub repository: Option<&'a str>,
    pub merged: Option<&'a MergedFix>,
    pub origin: AssessmentOrigin,
}

impl ValidReport<'_> {
    fn first_report(&self) -> FirstReport {
        FirstReport {
            report_id: self.report.id,
            reported_at: self.report.reported_at,
        }
    }

    fn event(
        &self,
        kind: SecurityEventKind,
        vulnerability_id: i64,
        registration: RegistrationKind,
    ) -> NewSecurityEvent {
        let mut detail = self
            .assessment
            .detail(SecurityOutcome::Valid.as_str(), self.repository);
        detail["origin"] = self.origin.as_str().into();
        detail["vulnerability_id"] = vulnerability_id.into();
        if registration == RegistrationKind::Relinked {
            detail["relinked"] = true.into();
        }
        NewSecurityEvent::new(kind).with_detail(detail)
    }

    fn changes(&self, outcome: SecurityOutcome, vulnerability_id: i64) -> SecurityReportChanges {
        SecurityReportChanges {
            outcome: Some(outcome),
            assessment_summary: Some(self.assessment.summary.clone()),
            affected_repository: self.repository.map(str::to_string),
            fixed_by_commit: self.merged.map(|merged| merged.commit.clone()),
            vulnerability_id: Some(vulnerability_id),
            ..SecurityReportChanges::default()
        }
    }
}

impl SecurityInbox {
    /// Link a valid report to its canonical vulnerability. A report already
    /// linked to a vulnerability goes back to it without being judged again,
    /// so it never spawns a second vulnerability or reward. Returns `None`
    /// when the report left its registering state meanwhile.
    pub(crate) async fn register(&self, valid: &ValidReport<'_>) -> Result<Option<Registration>> {
        let _registration = self.registration.lock().await;
        if let Some(existing) = self
            .tracker
            .find_security_vulnerability_by_first_report(valid.report.id)?
        {
            return self
                .link_tracking(valid, existing, RegistrationKind::Adopted)
                .await;
        }
        if let Some(linked) = self.linked_vulnerability(valid.report)? {
            return self
                .link_tracking(valid, linked, RegistrationKind::Relinked)
                .await;
        }
        let embedding = self.embedding(valid.assessment).await;
        let compared = self
            .tracker
            .list_security_vulnerability_candidates(MAX_COMPARED_VULNERABILITIES)?;
        let candidates = self.candidates(&compared, embedding.as_deref());
        if let Some(matched) = self.judge(valid.assessment, &candidates).await? {
            let matched = self
                .tracker
                .get_security_vulnerability(matched)?
                .ok_or_else(|| Error::Other("duplicate vulnerability disappeared".into()))?;
            return self.link_duplicate(valid, matched).await;
        }
        let state = if valid.merged.is_some() {
            VulnerabilityState::Merged
        } else {
            VulnerabilityState::AwaitingFix
        };
        let vulnerability =
            self.tracker
                .create_security_vulnerability(&NewSecurityVulnerability {
                    title: valid.assessment.title.clone(),
                    summary: valid.assessment.summary.clone(),
                    embedding: embedding.unwrap_or_default(),
                    repository: valid.repository.map(str::to_string),
                    severity: valid.assessment.severity,
                    state,
                    first_report_id: valid.report.id,
                    first_reported_at: valid.report.reported_at,
                    merge_commit: valid.merged.map(|merged| merged.commit.clone()),
                    merged_at: valid.merged.map(|merged| merged.merged_at),
                })?;
        self.link_tracking(valid, vulnerability, RegistrationKind::Created)
            .await
    }

    fn linked_vulnerability(
        &self,
        report: &SecurityReport,
    ) -> Result<Option<SecurityVulnerability>> {
        match report.vulnerability_id {
            Some(vulnerability_id) => self.tracker.get_security_vulnerability(vulnerability_id),
            None => Ok(None),
        }
    }

    async fn embedding(&self, assessment: &Assessment) -> Option<Vec<f32>> {
        let embedder = self.embedder.as_ref()?;
        let text = format!("{}\n{}", assessment.title, assessment.summary);
        match embedder.embed(&text).await {
            Ok(embedding) if !embedding.is_empty() => Some(embedding),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(error = %error, "security dedup embedding failed, using recent vulnerabilities");
                None
            }
        }
    }

    /// The vulnerabilities similar enough to compare, most similar first,
    /// followed by the most recent ones that cannot be scored (no embedding,
    /// or one of another length). `compared` is newest first.
    fn candidates<'a>(
        &self,
        compared: &'a [SecurityVulnerabilityCandidate],
        embedding: Option<&[f32]>,
    ) -> Vec<&'a SecurityVulnerabilityCandidate> {
        let Some(embedding) = embedding else {
            return compared.iter().take(RECENT_CANDIDATES).collect();
        };
        let (scorable, unscorable): (Vec<_>, Vec<_>) = compared
            .iter()
            .partition(|candidate| candidate.embedding.len() == embedding.len());
        let threshold = self.config.duplicate_similarity as f32;
        let mut scored: Vec<(f32, &SecurityVulnerabilityCandidate)> = scorable
            .into_iter()
            .map(|candidate| {
                (
                    cosine_similarity_f32(embedding, &candidate.embedding),
                    candidate,
                )
            })
            .filter(|(similarity, _)| *similarity >= threshold)
            .collect();
        scored.sort_by(|left, right| right.0.total_cmp(&left.0));
        scored
            .into_iter()
            .take(self.config.duplicate_candidates)
            .map(|(_, candidate)| candidate)
            .chain(unscorable.into_iter().take(RECENT_CANDIDATES))
            .collect()
    }

    async fn judge(
        &self,
        assessment: &Assessment,
        candidates: &[&SecurityVulnerabilityCandidate],
    ) -> Result<Option<i64>> {
        if candidates.is_empty() {
            return Ok(None);
        }
        let listing: Vec<DuplicateCandidate<'_>> = candidates
            .iter()
            .map(|candidate| DuplicateCandidate {
                id: candidate.id,
                title: &candidate.title,
                summary: &candidate.summary,
            })
            .collect();
        let ids: Vec<i64> = candidates.iter().map(|candidate| candidate.id).collect();
        let sandbox = tempfile::tempdir()?;
        let value = self
            .agent
            .security_query(
                &prompt::duplicate_prompt(&assessment.title, &assessment.summary, &listing),
                &prompt::duplicate_schema(&ids),
                sandbox.path(),
                SecuritySandbox::NoTools,
                NO_TOOLS_TIMEOUT,
            )
            .await?;
        let verdict: DuplicateVerdict = serde_json::from_value(value)
            .map_err(|error| Error::Other(format!("malformed duplicate verdict: {error}")))?;
        if verdict.duplicate_of == NO_DUPLICATE_CHOICE {
            return Ok(None);
        }
        ids.into_iter()
            .find(|id| id.to_string() == verdict.duplicate_of)
            .map(Some)
            .ok_or_else(|| Error::Other("duplicate verdict named an unknown candidate".into()))
    }

    async fn link_duplicate(
        &self,
        valid: &ValidReport<'_>,
        matched: SecurityVulnerability,
    ) -> Result<Option<Registration>> {
        let award_exists = self.tracker.get_security_award(matched.id)?.is_some();
        let decision = select_first_reporter(
            FirstReport {
                report_id: matched.first_report_id,
                reported_at: matched.first_reported_at,
            },
            valid.first_report(),
            award_exists,
        );
        if decision == FirstReporterDecision::Replace
            && self.tracker.replace_first_reporter(
                matched.id,
                matched.first_report_id,
                valid.report.id,
                valid.report.reported_at,
            )?
        {
            let replaced = self
                .tracker
                .get_security_vulnerability(matched.id)?
                .unwrap_or(matched);
            return self
                .link_tracking(valid, replaced, RegistrationKind::Replaced)
                .await;
        }
        let moved = self.tracker.transition_security_report(
            valid.report.id,
            valid.origin.registering_state(),
            SecurityReportState::Replying,
            &valid.changes(SecurityOutcome::Duplicate, matched.id),
            &valid.event(
                SecurityEventKind::DuplicateLinked,
                matched.id,
                RegistrationKind::Duplicate,
            ),
        )?;
        Ok(moved.then_some(Registration {
            kind: RegistrationKind::Duplicate,
            vulnerability: matched,
        }))
    }

    async fn link_tracking(
        &self,
        valid: &ValidReport<'_>,
        vulnerability: SecurityVulnerability,
        kind: RegistrationKind,
    ) -> Result<Option<Registration>> {
        let moved = self.tracker.transition_security_report(
            valid.report.id,
            valid.origin.registering_state(),
            SecurityReportState::Tracking,
            &valid.changes(SecurityOutcome::Valid, vulnerability.id),
            &valid.event(SecurityEventKind::Assessed, vulnerability.id, kind),
        )?;
        Ok(moved.then_some(Registration {
            kind,
            vulnerability,
        }))
    }
}
