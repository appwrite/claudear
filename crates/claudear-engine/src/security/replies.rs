//! Fixed-template customer replies, sent at most once per report and kind.
//! A reply that may have been sent is never retried automatically; one the
//! source rejected is retried with a growing delay, and after
//! [`MAX_REPLY_ATTEMPTS`] rejections the report goes to an admin.

use super::notify::{OwnerAction, PingSubject, ReviewReason};
use super::scrub::{public_reason, scrub_internal};
use super::SecurityInbox;
use chrono::{DateTime, TimeDelta, Utc};
use claudear_config::config::{render_security_template, SecurityReplyVariables};
use claudear_core::error::Result;
use claudear_core::security::{
    DeliveryStatus, NewSecurityEvent, ReportTransition, SecurityEventKind, SecurityOutcome,
    SecurityReply, SecurityReplyKind, SecurityReport, SecurityReportChanges, SecurityReportState,
};
use claudear_integrations::source::{ConversationStatus, ReporterReply};
use serde_json::json;

const MAX_ERROR_CHARS: usize = 500;
const MAX_REPLY_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Another delivery holds or completed this reply, or its retry is not due.
    Skipped,
    Sent,
    Failed,
    Unconfirmed,
}

pub(crate) fn truncated_error(error: &str) -> String {
    scrub_internal(error)
        .chars()
        .take(MAX_ERROR_CHARS)
        .collect()
}

/// A rejected reply waits `backoff` times its attempt count since its last
/// claim before it is sent again.
fn retry_due(reply: &SecurityReply, backoff: TimeDelta, now: DateTime<Utc>) -> bool {
    let wait = backoff
        .checked_mul(i32::try_from(reply.attempts).unwrap_or(i32::MAX))
        .unwrap_or(TimeDelta::MAX);
    reply
        .claimed_at
        .checked_add_signed(wait)
        .is_some_and(|due| due <= now)
}

fn subject_for(inbox: &SecurityInbox, report: &SecurityReport) -> Result<PingSubject> {
    let Some(vulnerability_id) = report.vulnerability_id else {
        return Ok(PingSubject::report(report.id));
    };
    Ok(
        match inbox.tracker.get_security_vulnerability(vulnerability_id)? {
            Some(vulnerability) => PingSubject::for_report(report.id, &vulnerability),
            None => PingSubject::report(report.id),
        },
    )
}

impl SecurityInbox {
    fn template(&self, kind: SecurityReplyKind) -> &str {
        let templates = &self.config.templates;
        match kind {
            SecurityReplyKind::Confirmed => &templates.confirmed,
            SecurityReplyKind::NotReproducible => &templates.not_reproducible,
            SecurityReplyKind::AlreadyFixed => &templates.already_fixed,
            SecurityReplyKind::NotVulnerability => &templates.not_vulnerability,
            SecurityReplyKind::OutOfScope => &templates.out_of_scope,
            SecurityReplyKind::Duplicate => &templates.duplicate,
            SecurityReplyKind::FixLive => &templates.fix_live,
            SecurityReplyKind::FixLiveRewarded => &templates.fix_live_rewarded,
        }
    }

    pub(crate) fn reply_body(&self, report: &SecurityReport, kind: SecurityReplyKind) -> String {
        let outcome = report.outcome.unwrap_or(SecurityOutcome::NotVulnerability);
        let reason = public_reason(report.public_reason.as_deref().unwrap_or_default(), outcome);
        render_security_template(
            self.template(kind),
            &SecurityReplyVariables {
                reason: Some(&reason),
                fixed_on: report.fixed_on,
            },
        )
    }

    fn conversation_status(&self, kind: SecurityReplyKind) -> Option<ConversationStatus> {
        if kind.is_terminal() && self.config.close_after_reply {
            Some(ConversationStatus::Closed)
        } else if kind == SecurityReplyKind::Confirmed {
            Some(ConversationStatus::Pending)
        } else {
            None
        }
    }

    fn ledger_entry(
        &self,
        report_id: i64,
        kind: SecurityReplyKind,
    ) -> Result<Option<SecurityReply>> {
        Ok(self
            .tracker
            .list_security_replies(report_id)?
            .into_iter()
            .find(|reply| reply.kind.shares_ledger_entry_with(kind)))
    }

    /// Send `kind` unless a delivery of it already exists or its retry is
    /// not due; when it is sent, apply `completion` to the report.
    pub(crate) async fn deliver(
        &self,
        report: &SecurityReport,
        kind: SecurityReplyKind,
        completion: Option<ReportTransition>,
    ) -> Result<Delivery> {
        let previous = self.ledger_entry(report.id, kind)?;
        let backoff = TimeDelta::from_std(self.reply_retry_backoff).unwrap_or(TimeDelta::MAX);
        if previous.as_ref().is_some_and(|reply| {
            reply.status != DeliveryStatus::Failed || !retry_due(reply, backoff, Utc::now())
        }) {
            return Ok(Delivery::Skipped);
        }
        let attempt = previous.as_ref().map_or(0, |reply| reply.attempts) + 1;
        let claimed = NewSecurityEvent::new(SecurityEventKind::ReplyClaimed).with_detail(json!({
            "kind": kind,
            "attempt": attempt,
            "previous_error": previous.as_ref().and_then(|reply| reply.error.clone()),
        }));
        if !self
            .tracker
            .claim_security_reply(report.id, kind, &claimed)?
        {
            return Ok(Delivery::Skipped);
        }
        let status = self.conversation_status(kind);
        let body = self.reply_body(report, kind);
        let result = self
            .source
            .reply_to_reporter(&report.conversation_id, &body, status)
            .await;
        match result {
            ReporterReply::Sent => {
                self.record_sent(report, kind, status, completion)?;
                Ok(Delivery::Sent)
            }
            ReporterReply::Rejected(error) => {
                self.record_rejected(report, kind, attempt, &truncated_error(&error))
                    .await?;
                Ok(Delivery::Failed)
            }
            ReporterReply::Unconfirmed(error) => {
                let error = truncated_error(&error);
                self.mark_unconfirmed(report, kind, &error).await?;
                Ok(Delivery::Unconfirmed)
            }
        }
    }

    /// Deliver `kind`, or finish `completion` for a reply that was already
    /// sent (for example one an admin confirmed as delivered). A fix-live
    /// reply already recorded under the other fix-live kind keeps its kind.
    pub(crate) async fn ensure_delivered(
        &self,
        report: &SecurityReport,
        kind: SecurityReplyKind,
        completion: ReportTransition,
    ) -> Result<Delivery> {
        let existing = self.ledger_entry(report.id, kind)?;
        match existing.as_ref().map(|reply| (reply.kind, reply.status)) {
            None => self.deliver(report, kind, Some(completion)).await,
            Some((recorded, DeliveryStatus::Failed)) => {
                self.deliver(report, recorded, Some(completion)).await
            }
            Some((recorded, DeliveryStatus::Sent)) => {
                self.tracker.transition_security_report(
                    report.id,
                    completion.from,
                    completion.to,
                    &SecurityReportChanges::default(),
                    &NewSecurityEvent::new(SecurityEventKind::ReplySent)
                        .with_detail(json!({ "kind": recorded, "recovered": true })),
                )?;
                Ok(Delivery::Skipped)
            }
            Some((_, DeliveryStatus::Sending | DeliveryStatus::Unconfirmed)) => {
                Ok(Delivery::Skipped)
            }
        }
    }

    pub(crate) async fn mark_unconfirmed(
        &self,
        report: &SecurityReport,
        kind: SecurityReplyKind,
        error: &str,
    ) -> Result<()> {
        self.tracker.complete_security_reply(
            report.id,
            kind,
            DeliveryStatus::Unconfirmed,
            Some(error),
            &NewSecurityEvent::new(SecurityEventKind::ReplyUnconfirmed)
                .with_detail(json!({ "kind": kind, "error": error })),
            None,
        )?;
        self.ping(
            OwnerAction::NeedsReview(ReviewReason::ReplyUnconfirmed),
            subject_for(self, report)?,
        )
        .await;
        Ok(())
    }

    /// Record a rejection; the last allowed attempt also hands the report to
    /// an admin in the same write.
    async fn record_rejected(
        &self,
        report: &SecurityReport,
        kind: SecurityReplyKind,
        attempt: u32,
        error: &str,
    ) -> Result<()> {
        let exhausted = attempt >= MAX_REPLY_ATTEMPTS;
        let mut detail = json!({ "kind": kind, "error": error, "attempt": attempt });
        let escalation = exhausted
            .then(|| ReportTransition::new(report.state, SecurityReportState::NeedsHuman))
            .filter(|transition| transition.is_allowed());
        if escalation.is_some() {
            detail["reason"] = ReviewReason::ReplyFailed.as_str().into();
        }
        let escalated = self.tracker.complete_security_reply(
            report.id,
            kind,
            DeliveryStatus::Failed,
            Some(error),
            &NewSecurityEvent::new(SecurityEventKind::ReplyFailed).with_detail(detail),
            escalation,
        )?;
        if escalated {
            self.ping(
                OwnerAction::NeedsReview(ReviewReason::ReplyFailed),
                subject_for(self, report)?,
            )
            .await;
        }
        Ok(())
    }

    fn record_sent(
        &self,
        report: &SecurityReport,
        kind: SecurityReplyKind,
        status: Option<ConversationStatus>,
        completion: Option<ReportTransition>,
    ) -> Result<()> {
        self.tracker.complete_security_reply(
            report.id,
            kind,
            DeliveryStatus::Sent,
            None,
            &NewSecurityEvent::new(SecurityEventKind::ReplySent).with_detail(json!({
                "kind": kind,
                "conversation_status": status.map(ConversationStatus::as_str),
            })),
            completion,
        )?;
        if status == Some(ConversationStatus::Closed) {
            self.tracker.record_security_event(
                Some(report.id),
                None,
                &NewSecurityEvent::new(SecurityEventKind::ConversationClosed)
                    .with_detail(json!({ "kind": kind })),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn reply(attempts: u32, claimed_minute: u32) -> SecurityReply {
        SecurityReply {
            id: 1,
            report_id: 1,
            kind: SecurityReplyKind::Confirmed,
            status: DeliveryStatus::Failed,
            error: Some("422".to_string()),
            attempts,
            claimed_at: Utc
                .with_ymd_and_hms(2026, 11, 4, 9, claimed_minute, 0)
                .unwrap(),
            completed_at: None,
        }
    }

    #[test]
    fn a_rejected_reply_waits_longer_after_each_attempt() {
        let backoff = TimeDelta::minutes(5);
        let at = |minute: u32| Utc.with_ymd_and_hms(2026, 11, 4, 9, minute, 0).unwrap();
        assert!(!retry_due(&reply(1, 0), backoff, at(4)));
        assert!(retry_due(&reply(1, 0), backoff, at(5)));
        assert!(!retry_due(&reply(2, 0), backoff, at(9)));
        assert!(retry_due(&reply(2, 0), backoff, at(10)));
        assert!(retry_due(&reply(3, 0), TimeDelta::zero(), at(0)));
        assert!(!retry_due(&reply(u32::MAX, 0), TimeDelta::MAX, at(59)));
    }
}
