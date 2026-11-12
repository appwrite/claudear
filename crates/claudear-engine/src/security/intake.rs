//! Intake of polled security conversations: what to do with each one, and
//! the screening and assessment of new reports.

use super::assessment::{AssessmentResult, ReportText, Verdict};
use super::dedup::{AssessmentOrigin, Registration, RegistrationKind, ValidReport};
use super::notify::{OwnerAction, PingSubject, ReviewReason};
use super::replies::truncated_error;
use super::screening::Screening;
use super::scrub::public_reason;
use super::{SecurityInbox, SecurityPollDecision, SecurityRoute};
use chrono::Utc;
use claudear_analysis::inference::RepoResolution;
use claudear_config::config::{
    SpamAction, ATTACHMENTS_METADATA_KEY, CUSTOMER_THREAD_COUNT_METADATA_KEY,
    MAILBOX_ID_METADATA_KEY,
};
use claudear_core::error::Result;
use claudear_core::security::{
    security_fix_short_id, NewSecurityEvent, NewSecurityReport, ReportTransition,
    SecurityEventKind, SecurityOutcome, SecurityReplyKind, SecurityReport, SecurityReportChanges,
    SecurityReportState, VulnerabilityState, SECURITY_FIX_SOURCE,
};
use claudear_core::types::Issue;
use serde_json::json;

const CUSTOMER_EMAIL_METADATA_KEY: &str = "customer_email";
const CUSTOMER_ID_METADATA_KEY: &str = "customer_id";
const SPAM_NOTE: &str = "Closed automatically as spam.";
const SUPPORT_NOTE: &str =
    "Moved automatically from the security inbox: a support request, not a security report.";

fn thread_count(issue: &Issue) -> u32 {
    issue
        .get_metadata::<u32>(CUSTOMER_THREAD_COUNT_METADATA_KEY)
        .unwrap_or_default()
}

fn new_report(issue: &Issue) -> NewSecurityReport {
    NewSecurityReport {
        source: issue.source.clone(),
        conversation_id: issue.id.clone(),
        short_id: issue.short_id.clone(),
        mailbox_id: issue
            .get_metadata::<String>(MAILBOX_ID_METADATA_KEY)
            .unwrap_or_default(),
        url: issue.url.clone(),
        subject: issue.title.clone(),
        reporter_email: issue.get_metadata::<String>(CUSTOMER_EMAIL_METADATA_KEY),
        reporter_customer_id: issue.get_metadata::<String>(CUSTOMER_ID_METADATA_KEY),
        reported_at: issue.created_at.unwrap_or_else(Utc::now),
        customer_thread_count: thread_count(issue),
    }
}

fn report_text(issue: &Issue, report: &SecurityReport) -> ReportText {
    ReportText {
        subject: issue.title.clone(),
        body: issue.description.clone().unwrap_or_default(),
        attachments: issue
            .get_metadata::<Vec<String>>(ATTACHMENTS_METADATA_KEY)
            .unwrap_or_default(),
        reported_at: report.reported_at,
    }
}

fn reply_outcome(verdict: &Verdict) -> Option<SecurityOutcome> {
    match verdict {
        Verdict::NotReproducible => Some(SecurityOutcome::NotReproducible),
        Verdict::AlreadyFixed { .. } => Some(SecurityOutcome::AlreadyFixed),
        Verdict::NotVulnerability => Some(SecurityOutcome::NotVulnerability),
        Verdict::OutOfScope => Some(SecurityOutcome::OutOfScope),
        Verdict::Valid
        | Verdict::FixedAfterReport(_)
        | Verdict::NeedsHuman(_)
        | Verdict::UnindexedRepository(_)
        | Verdict::ConflictingRepository(_) => None,
    }
}

impl SecurityInbox {
    /// A conversation without a creation time is treated as old, so the
    /// existing backlog stays untouched.
    fn predates_processing(&self, issue: &Issue) -> bool {
        match (self.config.process_after, issue.created_at) {
            (None, _) => false,
            (Some(after), Some(created_at)) => created_at < after,
            (Some(_), None) => true,
        }
    }

    pub fn poll_decision(&self, issue: &Issue) -> Result<SecurityPollDecision> {
        if self.predates_processing(issue) {
            return Ok(SecurityPollDecision::Skip);
        }
        let Some(report) = self
            .tracker
            .get_security_report_by_conversation(&issue.source, &issue.id)?
        else {
            return Ok(SecurityPollDecision::Route);
        };
        Ok(match report.state {
            SecurityReportState::Received => SecurityPollDecision::Route,
            SecurityReportState::Assessing => SecurityPollDecision::Skip,
            _ if thread_count(issue) > report.customer_thread_count => {
                SecurityPollDecision::FollowUp
            }
            _ => SecurityPollDecision::Skip,
        })
    }

    pub async fn observe_follow_up(&self, issue: &Issue) -> Result<()> {
        let Some(report) = self
            .tracker
            .get_security_report_by_conversation(&issue.source, &issue.id)?
        else {
            return Ok(());
        };
        if self.tracker.record_security_follow_up(
            report.id,
            report.customer_thread_count,
            thread_count(issue),
        )? {
            let subject = match report.vulnerability_id {
                Some(vulnerability_id) => self
                    .tracker
                    .get_security_vulnerability(vulnerability_id)?
                    .map(|vulnerability| PingSubject::for_report(report.id, &vulnerability))
                    .unwrap_or_else(|| PingSubject::report(report.id)),
                None => PingSubject::report(report.id),
            };
            self.ping(OwnerAction::ReporterFollowUp, subject).await;
        }
        Ok(())
    }

    pub async fn route(&self, issue: &Issue, resolution: &RepoResolution) -> SecurityRoute {
        match self.route_report(issue, resolution).await {
            Ok(route) => route,
            Err(error) => {
                let reason = truncated_error(&error.to_string());
                tracing::warn!(error = %reason, "security report routing failed");
                SecurityRoute::done(reason)
            }
        }
    }

    async fn route_report(
        &self,
        issue: &Issue,
        resolution: &RepoResolution,
    ) -> Result<SecurityRoute> {
        if self.predates_processing(issue) {
            return Ok(SecurityRoute::done("received before process_after"));
        }
        let report = self.tracker.upsert_security_report(&new_report(issue))?;
        if report.state != SecurityReportState::Received {
            return self.route_existing(&report);
        }
        if report.assessment_attempts >= self.config.max_assessment_attempts {
            if self.tracker.transition_security_report(
                report.id,
                SecurityReportState::Received,
                SecurityReportState::NeedsHuman,
                &SecurityReportChanges::default(),
                &NewSecurityEvent::new(SecurityEventKind::NeedsHuman).with_detail(json!({
                    "reason": ReviewReason::AssessmentFailed.as_str(),
                    "attempts": report.assessment_attempts,
                })),
            )? {
                self.ping(
                    OwnerAction::NeedsReview(ReviewReason::AssessmentFailed),
                    PingSubject::report(report.id),
                )
                .await;
            }
            return Ok(SecurityRoute::done("assessment attempts exhausted"));
        }
        if !self.tracker.transition_security_report(
            report.id,
            SecurityReportState::Received,
            SecurityReportState::Assessing,
            &SecurityReportChanges::default(),
            &NewSecurityEvent::new(SecurityEventKind::AssessmentStarted)
                .with_detail(json!({ "attempt": report.assessment_attempts + 1 })),
        )? {
            return Ok(SecurityRoute::done("assessment already claimed"));
        }
        let text = report_text(issue, &report);
        match self.screen_and_assess(&report, &text, resolution).await {
            Ok(route) => Ok(route),
            Err(error) => {
                self.release(&report, &error.to_string())?;
                Ok(SecurityRoute::done("assessment failed and was released"))
            }
        }
    }

    fn route_existing(&self, report: &SecurityReport) -> Result<SecurityRoute> {
        if report.state == SecurityReportState::Tracking {
            if let Some(request) = self.immediate_fix(report)? {
                return Ok(SecurityRoute::RunFix(request));
            }
        }
        Ok(SecurityRoute::done(format!("report is {}", report.state)))
    }

    /// The fix request for a first reporter whose vulnerability still awaits
    /// a fix run that has not been started.
    fn immediate_fix(&self, report: &SecurityReport) -> Result<Option<super::SecurityFixRequest>> {
        let Some(vulnerability_id) = report.vulnerability_id else {
            return Ok(None);
        };
        let Some(vulnerability) = self.tracker.get_security_vulnerability(vulnerability_id)? else {
            return Ok(None);
        };
        let waiting = vulnerability.state == VulnerabilityState::AwaitingFix
            && vulnerability.first_report_id == report.id
            && self
                .tracker
                .get_attempt(
                    SECURITY_FIX_SOURCE,
                    &security_fix_short_id(vulnerability_id),
                )?
                .is_none()
            && self.fix_approved(vulnerability_id)?;
        if waiting {
            self.fix_request(vulnerability_id)
        } else {
            Ok(None)
        }
    }

    fn release(&self, report: &SecurityReport, error: &str) -> Result<()> {
        self.tracker.transition_security_report(
            report.id,
            SecurityReportState::Assessing,
            SecurityReportState::Received,
            &SecurityReportChanges {
                assessment_attempts: Some(report.assessment_attempts + 1),
                ..SecurityReportChanges::default()
            },
            &NewSecurityEvent::new(SecurityEventKind::AssessmentFailed)
                .with_detail(json!({ "error": truncated_error(error) })),
        )?;
        Ok(())
    }

    async fn screen_and_assess(
        &self,
        report: &SecurityReport,
        text: &ReportText,
        resolution: &RepoResolution,
    ) -> Result<SecurityRoute> {
        match self.screen(text).await? {
            Screening::Spam { confidence } => {
                self.close_spam(report, confidence).await?;
                Ok(SecurityRoute::done("screened as spam"))
            }
            Screening::Support {
                mailbox_id,
                confidence,
            } => {
                self.forward_to_support(report, &mailbox_id, confidence)
                    .await?;
                Ok(SecurityRoute::done("forwarded to support"))
            }
            Screening::Report { .. } => {
                let result = self.assess(text, resolution).await?;
                self.apply_assessment(report, result).await
            }
        }
    }

    async fn forward_to_support(
        &self,
        report: &SecurityReport,
        mailbox_id: &str,
        confidence: f64,
    ) -> Result<()> {
        self.source
            .move_conversation(&report.conversation_id, mailbox_id)
            .await?;
        if let Err(error) = self
            .source
            .add_comment(&report.conversation_id, SUPPORT_NOTE)
            .await
        {
            tracing::warn!(error = %error, report_id = report.id, "failed to note a support forward");
        }
        self.tracker.transition_security_report(
            report.id,
            SecurityReportState::Assessing,
            SecurityReportState::Forwarded,
            &SecurityReportChanges {
                outcome: Some(SecurityOutcome::Support),
                ..SecurityReportChanges::default()
            },
            &NewSecurityEvent::new(SecurityEventKind::ForwardedToSupport)
                .with_detail(json!({ "mailbox_id": mailbox_id, "confidence": confidence })),
        )?;
        Ok(())
    }

    async fn close_spam(&self, report: &SecurityReport, confidence: f64) -> Result<()> {
        self.run_spam_action(&report.conversation_id).await?;
        if let Err(error) = self
            .source
            .add_comment(&report.conversation_id, SPAM_NOTE)
            .await
        {
            tracing::warn!(error = %error, report_id = report.id, "failed to note a spam closure");
        }
        self.tracker.record_security_event(
            Some(report.id),
            None,
            &NewSecurityEvent::new(SecurityEventKind::ConversationClosed)
                .with_detail(json!({ "spam_action": self.config.spam_action.as_str() })),
        )?;
        self.tracker.transition_security_report(
            report.id,
            SecurityReportState::Assessing,
            SecurityReportState::Spam,
            &SecurityReportChanges {
                outcome: Some(SecurityOutcome::Spam),
                spam_confidence: Some(confidence),
                ..SecurityReportChanges::default()
            },
            &NewSecurityEvent::new(SecurityEventKind::ScreenedSpam)
                .with_detail(json!({ "confidence": confidence })),
        )?;
        Ok(())
    }

    /// Mark the conversation as spam, falling back to closing it when the
    /// source refuses the spam status.
    pub(crate) async fn run_spam_action(&self, conversation_id: &str) -> Result<()> {
        if self.config.spam_action == SpamAction::Spam {
            match self.source.mark_spam(conversation_id).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    tracing::warn!(error = %error, "marking a conversation as spam failed, closing it instead");
                }
            }
        }
        self.source.resolve_issue(conversation_id).await
    }

    async fn apply_assessment(
        &self,
        report: &SecurityReport,
        result: AssessmentResult,
    ) -> Result<SecurityRoute> {
        let AssessmentResult {
            assessment,
            verdict,
            repository,
        } = result;
        if let Some(reason) = verdict.review_reason() {
            let merged = match &verdict {
                Verdict::FixedAfterReport(merged) => Some(merged),
                _ => None,
            };
            let mut detail = assessment.detail("needs_human", repository.as_deref());
            detail["reason"] = reason.as_str().into();
            verdict.record(&mut detail);
            let moved = self.tracker.transition_security_report(
                report.id,
                SecurityReportState::Assessing,
                SecurityReportState::NeedsHuman,
                &SecurityReportChanges {
                    assessment_summary: Some(assessment.summary.clone()),
                    affected_repository: repository.clone(),
                    fixed_by_commit: merged.map(|merged| merged.commit.clone()),
                    ..SecurityReportChanges::default()
                },
                &NewSecurityEvent::new(SecurityEventKind::NeedsHuman).with_detail(detail),
            )?;
            if moved {
                self.ping(
                    OwnerAction::NeedsReview(reason),
                    PingSubject::report(report.id),
                )
                .await;
            }
            return Ok(SecurityRoute::done(format!("needs human review: {reason}")));
        }
        if let Some(outcome) = reply_outcome(&verdict) {
            let (fixed_by_commit, fixed_on) = match &verdict {
                Verdict::AlreadyFixed { commit, fixed_on } => {
                    (Some(commit.clone()), Some(*fixed_on))
                }
                _ => (None, None),
            };
            let reason = matches!(
                outcome,
                SecurityOutcome::NotVulnerability | SecurityOutcome::OutOfScope
            )
            .then(|| public_reason(&assessment.public_reason, outcome));
            if !self.tracker.transition_security_report(
                report.id,
                SecurityReportState::Assessing,
                SecurityReportState::Replying,
                &SecurityReportChanges {
                    outcome: Some(outcome),
                    public_reason: reason,
                    assessment_summary: Some(assessment.summary.clone()),
                    affected_repository: repository.clone(),
                    fixed_by_commit,
                    fixed_on,
                    ..SecurityReportChanges::default()
                },
                &NewSecurityEvent::new(SecurityEventKind::Assessed)
                    .with_detail(assessment.detail(outcome.as_str(), repository.as_deref())),
            )? {
                return Ok(SecurityRoute::done("report left assessment meanwhile"));
            }
            self.reply_with_outcome(report.id).await?;
            return Ok(SecurityRoute::done(format!("replied {outcome}")));
        }
        let registration = self
            .register(&ValidReport {
                report,
                assessment: &assessment,
                repository: repository.as_deref(),
                merged: None,
                origin: AssessmentOrigin::Agent,
            })
            .await?;
        let Some(registration) = registration else {
            return Ok(SecurityRoute::done("report left assessment meanwhile"));
        };
        self.acknowledge(report.id, &registration).await?;
        let stored = self.tracker.get_security_report(report.id)?;
        match stored {
            Some(stored) => self.route_existing(&stored),
            None => Ok(SecurityRoute::done("report disappeared")),
        }
    }

    pub(crate) async fn reply_with_outcome(&self, report_id: i64) -> Result<()> {
        let Some(report) = self.tracker.get_security_report(report_id)? else {
            return Ok(());
        };
        let Some(kind) = report.outcome.and_then(SecurityReplyKind::for_outcome) else {
            return Ok(());
        };
        self.ensure_delivered(
            &report,
            kind,
            ReportTransition::new(SecurityReportState::Replying, SecurityReportState::Closed),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn acknowledge(
        &self,
        report_id: i64,
        registration: &Registration,
    ) -> Result<()> {
        if registration.kind == RegistrationKind::Created {
            let action = if self.config.require_fix_approval {
                OwnerAction::FixAwaitingApproval
            } else {
                OwnerAction::NewValidReport
            };
            self.ping(
                action,
                PingSubject::vulnerability(&registration.vulnerability),
            )
            .await;
        }
        match registration.kind {
            RegistrationKind::Duplicate => return self.reply_with_outcome(report_id).await,
            RegistrationKind::Relinked => return Ok(()),
            RegistrationKind::Created | RegistrationKind::Adopted | RegistrationKind::Replaced => {}
        }
        if self.config.acknowledge_valid {
            if let Some(report) = self.tracker.get_security_report(report_id)? {
                self.deliver(&report, SecurityReplyKind::Confirmed, None)
                    .await?;
            }
        }
        Ok(())
    }
}
