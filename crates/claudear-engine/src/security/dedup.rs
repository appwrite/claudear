//! First-reporter deduplication. Only ever runs while holding the inbox's
//! `registration` lock, so two valid reports never both become first.

use super::assessment::{Assessment, MergedFix, NO_TOOLS_TIMEOUT};
use super::prompt::{self, DuplicateCandidate};
use super::SecurityInbox;
use async_trait::async_trait;
use claudear_analysis::feedback::EmbeddingClient;
use claudear_core::error::{Error, Result};
use claudear_core::security::{
    select_first_reporter, FirstReporterDecision, NewSecurityEvent, NewSecurityVulnerability,
    SecurityEventKind, SecurityOutcome, SecurityReport, SecurityReportChanges, SecurityReportState,
    SecurityVulnerability, VulnerabilityState,
};
use claudear_core::types::cosine_similarity_f32;
use claudear_integrations::runner::SecuritySandbox;
use serde::Deserialize;

const RECENT_CANDIDATES: usize = 20;
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
}

#[derive(Debug, Clone)]
pub(crate) struct Registration {
    pub kind: RegistrationKind,
    pub vulnerability: SecurityVulnerability,
}

/// Where a valid assessment came from, recorded on the report's event.
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
    fn event(&self, kind: SecurityEventKind, vulnerability_id: i64) -> NewSecurityEvent {
        let mut detail = self
            .assessment
            .detail(SecurityOutcome::Valid.as_str(), self.repository);
        detail["origin"] = self.origin.as_str().into();
        detail["vulnerability_id"] = vulnerability_id.into();
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
    /// Link a valid, `Assessing` report to its canonical vulnerability.
    /// Returns `None` when the report left `Assessing` meanwhile.
    pub(crate) async fn register(&self, valid: &ValidReport<'_>) -> Result<Option<Registration>> {
        let _registration = self.registration.lock().await;
        let vulnerabilities = self.tracker.list_security_vulnerabilities(None)?;
        if let Some(existing) = vulnerabilities
            .iter()
            .find(|vulnerability| vulnerability.first_report_id == valid.report.id)
        {
            return self
                .link_first(valid, existing.clone(), RegistrationKind::Adopted)
                .await;
        }
        let embedding = self.embedding(valid.assessment).await;
        let candidates = self.candidates(&vulnerabilities, embedding.as_deref());
        if let Some(matched) = self.judge(valid.assessment, &candidates).await? {
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
        self.link_first(valid, vulnerability, RegistrationKind::Created)
            .await
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

    fn candidates<'a>(
        &self,
        vulnerabilities: &'a [SecurityVulnerability],
        embedding: Option<&[f32]>,
    ) -> Vec<&'a SecurityVulnerability> {
        let Some(embedding) = embedding else {
            return vulnerabilities
                .iter()
                .rev()
                .take(RECENT_CANDIDATES)
                .collect();
        };
        let threshold = self.config.duplicate_similarity as f32;
        let mut scored: Vec<(f32, &SecurityVulnerability)> = vulnerabilities
            .iter()
            .filter(|vulnerability| vulnerability.embedding.len() == embedding.len())
            .map(|vulnerability| {
                (
                    cosine_similarity_f32(embedding, &vulnerability.embedding),
                    vulnerability,
                )
            })
            .filter(|(similarity, _)| *similarity >= threshold)
            .collect();
        scored.sort_by(|left, right| right.0.total_cmp(&left.0));
        scored
            .into_iter()
            .take(self.config.duplicate_candidates)
            .map(|(_, vulnerability)| vulnerability)
            .collect()
    }

    async fn judge(
        &self,
        assessment: &Assessment,
        candidates: &[&SecurityVulnerability],
    ) -> Result<Option<SecurityVulnerability>> {
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
        let matched = candidates
            .iter()
            .find(|candidate| candidate.id.to_string() == verdict.duplicate_of)
            .ok_or_else(|| Error::Other("duplicate verdict named an unknown candidate".into()))?;
        Ok(Some((*matched).clone()))
    }

    async fn link_duplicate(
        &self,
        valid: &ValidReport<'_>,
        matched: SecurityVulnerability,
    ) -> Result<Option<Registration>> {
        let award_exists = self.tracker.get_security_award(matched.id)?.is_some();
        let decision = select_first_reporter(
            matched.first_reported_at,
            valid.report.reported_at,
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
                .link_first(valid, replaced, RegistrationKind::Replaced)
                .await;
        }
        let moved = self.tracker.transition_security_report(
            valid.report.id,
            SecurityReportState::Assessing,
            SecurityReportState::Replying,
            &valid.changes(SecurityOutcome::Duplicate, matched.id),
            &valid.event(SecurityEventKind::DuplicateLinked, matched.id),
        )?;
        Ok(moved.then_some(Registration {
            kind: RegistrationKind::Duplicate,
            vulnerability: matched,
        }))
    }

    async fn link_first(
        &self,
        valid: &ValidReport<'_>,
        vulnerability: SecurityVulnerability,
        kind: RegistrationKind,
    ) -> Result<Option<Registration>> {
        let moved = self.tracker.transition_security_report(
            valid.report.id,
            SecurityReportState::Assessing,
            SecurityReportState::Tracking,
            &valid.changes(SecurityOutcome::Valid, vulnerability.id),
            &valid.event(SecurityEventKind::Assessed, vulnerability.id),
        )?;
        Ok(moved.then_some(Registration {
            kind,
            vulnerability,
        }))
    }
}
