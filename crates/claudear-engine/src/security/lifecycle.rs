//! `advance()`: the periodic, idempotent sweep that moves every report and
//! vulnerability forward from persisted state alone.

use super::assessment::Assessment;
use super::dedup::{AssessmentOrigin, ValidReport};
use super::notify::{OwnerAction, PingSubject, ReviewReason};
use super::replies::{truncated_error, Delivery};
use super::scrub::scrub_internal;
use super::{SecurityAdvanceSummary, SecurityInbox};
use chrono::{TimeDelta, Utc};
use claudear_analysis::release::{LiveStatus, RepositoryRole};
use claudear_core::error::{Error, Result};
use claudear_core::security::{
    security_fix_short_id, AwardStatus, DeliveryStatus, NewSecurityAward, NewSecurityEvent,
    SecurityEvent, SecurityEventKind, SecurityReplyKind, SecurityReport, SecurityReportFilter,
    SecurityReportState, SecurityVulnerability, SecurityVulnerabilityChanges, VulnerabilityState,
    SECURITY_FIX_SOURCE,
};
use claudear_core::types::FixAttemptStatus;
use claudear_storage::parse_pr_url;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const DEPLOYMENT_CHECK_PING_INTERVAL: TimeDelta = TimeDelta::hours(24);
const UNCONFIRMED_AFTER_RESTART: &str =
    "delivery outcome unknown: the reply was still sending when claudear stopped";

impl SecurityAdvanceSummary {
    fn count(&mut self, delivery: Delivery) {
        if delivery == Delivery::Sent {
            self.replies_sent += 1;
        }
    }
}

fn record<T>(summary: &mut SecurityAdvanceSummary, step: &str, result: Result<T>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            summary.errors += 1;
            tracing::warn!(step, error = %scrub_internal(&error.to_string()), "security inbox step failed");
            None
        }
    }
}

fn every_report(state: SecurityReportState) -> SecurityReportFilter {
    SecurityReportFilter {
        state: Some(state),
        outcome: None,
        limit: Some(usize::MAX),
    }
}

fn confirmed_assessment(detail: &Value) -> Assessment {
    let text =
        |key: &str| scrub_internal(detail.get(key).and_then(Value::as_str).unwrap_or_default());
    Assessment {
        title: text("title"),
        summary: text("summary"),
        severity: detail
            .get("severity")
            .and_then(Value::as_str)
            .and_then(|severity| severity.parse().ok()),
        ..Assessment::default()
    }
}

impl SecurityInbox {
    pub async fn advance(&self) -> SecurityAdvanceSummary {
        let mut summary = SecurityAdvanceSummary::default();
        self.register_admin_confirmed(&mut summary).await;
        let stale_after = self.config.assessment_timeout().saturating_mul(2);
        if let Some(released) = record(
            &mut summary,
            "release_stale_assessments",
            self.tracker.release_stale_security_assessments(stale_after),
        ) {
            summary.released = released;
        }
        self.settle_replies(&mut summary).await;
        self.close_overridden_spam(&mut summary).await;
        self.advance_fixing(&mut summary).await;
        self.advance_merged(&mut summary).await;
        self.advance_live(&mut summary).await;
        summary
    }

    async fn register_admin_confirmed(&self, summary: &mut SecurityAdvanceSummary) {
        let Some(confirmed) = record(
            summary,
            "list_admin_confirmed_reports",
            self.tracker.list_admin_confirmed_reports(),
        ) else {
            return;
        };
        for (report, confirmation) in confirmed {
            let result = self.register_confirmation(&report, &confirmation).await;
            record(summary, "register_admin_confirmed", result);
        }
    }

    async fn register_confirmation(
        &self,
        report: &SecurityReport,
        confirmation: &SecurityEvent,
    ) -> Result<()> {
        let assessment = confirmed_assessment(&confirmation.detail);
        let repository = confirmation
            .detail
            .get("repository")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.config.default_repository.clone());
        let registration = self
            .register(&ValidReport {
                report,
                assessment: &assessment,
                repository: repository.as_deref(),
                merged: None,
                origin: AssessmentOrigin::Admin,
            })
            .await?;
        if let Some(registration) = registration {
            self.acknowledge(report.id, &registration).await?;
        }
        Ok(())
    }

    async fn settle_replies(&self, summary: &mut SecurityAdvanceSummary) {
        for state in [SecurityReportState::Replying, SecurityReportState::Tracking] {
            let Some(reports) = record(
                summary,
                "list_reports",
                self.tracker.list_security_reports(&every_report(state)),
            ) else {
                continue;
            };
            for report in reports {
                let result = self.settle_report_replies(&report, summary).await;
                record(summary, "settle_replies", result);
            }
        }
    }

    async fn settle_report_replies(
        &self,
        report: &SecurityReport,
        summary: &mut SecurityAdvanceSummary,
    ) -> Result<()> {
        self.expire_stale_sending(report).await?;
        match report.state {
            SecurityReportState::Replying => {
                let Some(kind) = report.outcome.and_then(SecurityReplyKind::for_outcome) else {
                    return Ok(());
                };
                let delivery = self
                    .ensure_delivered(
                        report,
                        kind,
                        (SecurityReportState::Replying, SecurityReportState::Closed),
                    )
                    .await?;
                summary.count(delivery);
            }
            SecurityReportState::Tracking if self.awaits_confirmation(report)? => {
                let delivery = self
                    .deliver(report, SecurityReplyKind::Confirmed, None)
                    .await?;
                summary.count(delivery);
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether a first reporter still lacks its acknowledgement while the fix
    /// is not live yet.
    fn awaits_confirmation(&self, report: &SecurityReport) -> Result<bool> {
        if !self.config.acknowledge_valid {
            return Ok(false);
        }
        let Some(vulnerability_id) = report.vulnerability_id else {
            return Ok(false);
        };
        let Some(vulnerability) = self.tracker.get_security_vulnerability(vulnerability_id)? else {
            return Ok(false);
        };
        if vulnerability.first_report_id != report.id
            || vulnerability.state == VulnerabilityState::Live
        {
            return Ok(false);
        }
        Ok(self
            .tracker
            .list_security_replies(report.id)?
            .iter()
            .find(|reply| reply.kind == SecurityReplyKind::Confirmed)
            .is_none_or(|reply| reply.status == DeliveryStatus::Failed))
    }

    /// A reply still `Sending` long after its claim was interrupted, so it
    /// may or may not have gone out: never resend it, ask an admin instead.
    async fn expire_stale_sending(&self, report: &SecurityReport) -> Result<()> {
        let stale_after = TimeDelta::from_std(self.sending_stale_after).unwrap_or(TimeDelta::MAX);
        let cutoff = Utc::now()
            .checked_sub_signed(stale_after)
            .unwrap_or(chrono::DateTime::<Utc>::MIN_UTC);
        for reply in self.tracker.list_security_replies(report.id)? {
            if reply.status == DeliveryStatus::Sending && reply.claimed_at <= cutoff {
                self.mark_unconfirmed(report, reply.kind, UNCONFIRMED_AFTER_RESTART)
                    .await?;
            }
        }
        Ok(())
    }

    /// Run the spam action for reports an admin marked as spam, whose
    /// conversation is still open.
    async fn close_overridden_spam(&self, summary: &mut SecurityAdvanceSummary) {
        let Some(reports) = record(
            summary,
            "list_open_spam",
            self.tracker.list_security_reports_without_event(
                SecurityReportState::Spam,
                SecurityEventKind::ConversationClosed,
            ),
        ) else {
            return;
        };
        for report in reports {
            let result = async {
                self.run_spam_action(&report.conversation_id).await?;
                self.tracker.record_security_event(
                    Some(report.id),
                    None,
                    &NewSecurityEvent::new(SecurityEventKind::ConversationClosed)
                        .with_detail(json!({ "spam_action": self.config.spam_action.as_str() })),
                )
            }
            .await;
            record(summary, "close_overridden_spam", result);
        }
    }

    async fn advance_fixing(&self, summary: &mut SecurityAdvanceSummary) {
        let Some(vulnerabilities) = record(
            summary,
            "list_fixing",
            self.tracker
                .list_security_vulnerabilities(Some(VulnerabilityState::Fixing)),
        ) else {
            return;
        };
        for vulnerability in vulnerabilities {
            let result = self
                .advance_fixing_vulnerability(&vulnerability, summary)
                .await;
            record(summary, "advance_fixing", result);
        }
    }

    async fn advance_fixing_vulnerability(
        &self,
        vulnerability: &SecurityVulnerability,
        summary: &mut SecurityAdvanceSummary,
    ) -> Result<()> {
        let attempt = self.tracker.get_attempt(
            SECURITY_FIX_SOURCE,
            &security_fix_short_id(vulnerability.id),
        )?;
        if attempt.is_some_and(|attempt| attempt.status == FixAttemptStatus::CannotFix) {
            if self
                .escalate(
                    vulnerability.id,
                    VulnerabilityState::Fixing,
                    ReviewReason::FixFailed,
                )
                .await?
            {
                summary.needs_human += 1;
            }
            return Ok(());
        }
        let (Some(deployment), Some(url)) = (&self.deployment, &vulnerability.pull_request_url)
        else {
            return Ok(());
        };
        let Some((repository, number)) = parse_pr_url(url) else {
            return self
                .deployment_check_failed(vulnerability, "unrecognised pull request url")
                .await;
        };
        match deployment.pull_request(&repository, number).await {
            Ok(Some(state)) if state.merged => {
                let moved = self.tracker.transition_security_vulnerability(
                    vulnerability.id,
                    VulnerabilityState::Fixing,
                    VulnerabilityState::Merged,
                    &SecurityVulnerabilityChanges {
                        repository: Some(repository.clone()),
                        merge_commit: state.merge_commit_sha.clone(),
                        merged_at: Some(state.merged_at.unwrap_or_else(Utc::now)),
                        ..SecurityVulnerabilityChanges::default()
                    },
                    &NewSecurityEvent::new(SecurityEventKind::Merged).with_detail(json!({
                        "pull_request_url": url,
                        "merge_commit": state.merge_commit_sha,
                    })),
                )?;
                if moved {
                    summary.merged += 1;
                    if deployment.role(&repository) == RepositoryRole::Package {
                        self.tracker.record_security_event(
                            None,
                            Some(vulnerability.id),
                            &NewSecurityEvent::new(SecurityEventKind::CloudBumpRequired)
                                .with_detail(json!({ "repository": repository })),
                        )?;
                        self.ping(
                            OwnerAction::CloudBumpRequired,
                            PingSubject::vulnerability(vulnerability),
                        )
                        .await;
                    }
                }
            }
            Ok(Some(state)) if state.closed => {
                if self
                    .escalate(
                        vulnerability.id,
                        VulnerabilityState::Fixing,
                        ReviewReason::PullRequestClosed,
                    )
                    .await?
                {
                    summary.needs_human += 1;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                self.deployment_check_failed(vulnerability, "pull request not found")
                    .await?;
            }
            Err(error) => {
                self.deployment_check_failed(vulnerability, &error.to_string())
                    .await?;
            }
        }
        Ok(())
    }

    async fn advance_merged(&self, summary: &mut SecurityAdvanceSummary) {
        let Some(deployment) = &self.deployment else {
            return;
        };
        let Some(vulnerabilities) = record(
            summary,
            "list_merged",
            self.tracker
                .list_security_vulnerabilities(Some(VulnerabilityState::Merged)),
        ) else {
            return;
        };
        for vulnerability in vulnerabilities {
            let result = async {
                let (Some(repository), Some(commit)) =
                    (&vulnerability.repository, &vulnerability.merge_commit)
                else {
                    if self
                        .escalate(
                            vulnerability.id,
                            VulnerabilityState::Merged,
                            ReviewReason::UntrackedRepository,
                        )
                        .await?
                    {
                        summary.needs_human += 1;
                    }
                    return Ok(());
                };
                match deployment.commit_status(repository, commit).await {
                    Ok(LiveStatus::Live { versions }) => {
                        if self.tracker.transition_security_vulnerability(
                            vulnerability.id,
                            VulnerabilityState::Merged,
                            VulnerabilityState::Live,
                            &SecurityVulnerabilityChanges {
                                deployed_version: Some(versions.join(", ")),
                                live_at: Some(Utc::now()),
                                ..SecurityVulnerabilityChanges::default()
                            },
                            &NewSecurityEvent::new(SecurityEventKind::Live)
                                .with_detail(json!({ "versions": versions })),
                        )? {
                            summary.live += 1;
                        }
                    }
                    Ok(LiveStatus::Pending { .. }) => {}
                    Ok(LiveStatus::Untracked) => {
                        if self
                            .escalate(
                                vulnerability.id,
                                VulnerabilityState::Merged,
                                ReviewReason::UntrackedRepository,
                            )
                            .await?
                        {
                            summary.needs_human += 1;
                        }
                    }
                    Err(error) => {
                        self.deployment_check_failed(&vulnerability, &error.to_string())
                            .await?;
                    }
                }
                Ok::<(), Error>(())
            }
            .await;
            record(summary, "advance_merged", result);
        }
    }

    /// Record a failed deployment lookup and ping, at most once a day per
    /// vulnerability. A failure is never treated as live.
    async fn deployment_check_failed(
        &self,
        vulnerability: &SecurityVulnerability,
        error: &str,
    ) -> Result<()> {
        let recent = self
            .tracker
            .last_security_event(vulnerability.id, SecurityEventKind::DeploymentCheckFailed)?
            .is_some_and(|event| Utc::now() - event.created_at < DEPLOYMENT_CHECK_PING_INTERVAL);
        if recent {
            return Ok(());
        }
        self.tracker.record_security_event(
            None,
            Some(vulnerability.id),
            &NewSecurityEvent::new(SecurityEventKind::DeploymentCheckFailed)
                .with_detail(json!({ "error": truncated_error(error) })),
        )?;
        self.ping(
            OwnerAction::NeedsReview(ReviewReason::DeploymentCheckFailed),
            PingSubject::vulnerability(vulnerability),
        )
        .await;
        Ok(())
    }

    async fn advance_live(&self, summary: &mut SecurityAdvanceSummary) {
        let Some(tracking) = record(
            summary,
            "list_tracking",
            self.tracker
                .list_security_reports(&every_report(SecurityReportState::Tracking)),
        ) else {
            return;
        };
        let mut by_vulnerability: BTreeMap<i64, Vec<SecurityReport>> = BTreeMap::new();
        for report in tracking {
            if let Some(vulnerability_id) = report.vulnerability_id {
                by_vulnerability
                    .entry(vulnerability_id)
                    .or_default()
                    .push(report);
            }
        }
        for (vulnerability_id, reports) in by_vulnerability {
            let result = self
                .announce_live(vulnerability_id, &reports, summary)
                .await;
            record(summary, "announce_live", result);
        }
        self.notify_prepared_awards(summary).await;
    }

    async fn announce_live(
        &self,
        vulnerability_id: i64,
        reports: &[SecurityReport],
        summary: &mut SecurityAdvanceSummary,
    ) -> Result<()> {
        let Some(vulnerability) = self.tracker.get_security_vulnerability(vulnerability_id)? else {
            return Ok(());
        };
        if vulnerability.state != VulnerabilityState::Live {
            return Ok(());
        }
        let first_report = self
            .tracker
            .get_security_report(vulnerability.first_report_id)?;
        if self.tracker.prepare_security_award(
            vulnerability.id,
            vulnerability.first_report_id,
            &NewSecurityAward {
                recipient_email: first_report.and_then(|report| report.reporter_email),
                campaign: self.config.reward.campaign.clone(),
                credits: self.config.reward.credits,
            },
        )? {
            summary.awards_prepared += 1;
        }
        let award = self.tracker.get_security_award(vulnerability.id)?;
        for report in reports {
            let rewarded = award.as_ref().is_some_and(|award| {
                award.report_id == report.id && award.status != AwardStatus::Cancelled
            });
            let kind = if rewarded {
                SecurityReplyKind::FixLiveRewarded
            } else {
                SecurityReplyKind::FixLive
            };
            let delivery = self
                .ensure_delivered(
                    report,
                    kind,
                    (SecurityReportState::Tracking, SecurityReportState::Resolved),
                )
                .await?;
            summary.count(delivery);
        }
        Ok(())
    }

    async fn notify_prepared_awards(&self, summary: &mut SecurityAdvanceSummary) {
        let Some(awards) = record(
            summary,
            "list_prepared_awards",
            self.tracker
                .list_security_awards(Some(AwardStatus::Prepared)),
        ) else {
            return;
        };
        for award in awards {
            let result = async {
                if !self.tracker.update_security_award_status(
                    award.vulnerability_id,
                    &[AwardStatus::Prepared],
                    AwardStatus::OwnerNotified,
                    None,
                    None,
                )? {
                    return Ok(());
                }
                let subject = match self
                    .tracker
                    .get_security_vulnerability(award.vulnerability_id)?
                {
                    Some(vulnerability) => PingSubject::for_report(award.report_id, &vulnerability),
                    None => PingSubject::report(award.report_id),
                };
                self.ping(OwnerAction::AwardReady, subject).await;
                Ok::<(), Error>(())
            }
            .await;
            record(summary, "notify_prepared_awards", result);
        }
    }
}
