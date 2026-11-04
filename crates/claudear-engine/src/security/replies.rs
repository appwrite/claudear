//! Fixed-template customer replies, sent at most once per report and kind.
//! A reply that may have been sent is never retried automatically.

use super::notify::{OwnerAction, PingSubject, ReviewReason};
use super::scrub::{public_reason, scrub_internal};
use super::SecurityInbox;
use claudear_config::config::{render_security_template, SecurityReplyVariables};
use claudear_core::error::Result;
use claudear_core::security::{
    DeliveryStatus, NewSecurityEvent, SecurityEventKind, SecurityOutcome, SecurityReplyKind,
    SecurityReport, SecurityReportChanges, SecurityReportState,
};
use claudear_integrations::source::{ConversationStatus, ReporterReply};
use serde_json::json;

const MAX_ERROR_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Another delivery holds or completed this reply.
    Skipped,
    Sent,
    Failed,
    Unconfirmed,
}

/// The state change a sent reply completes.
pub(crate) type Completion = (SecurityReportState, SecurityReportState);

pub(crate) fn truncated_error(error: &str) -> String {
    scrub_internal(error)
        .chars()
        .take(MAX_ERROR_CHARS)
        .collect()
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

    /// Send `kind` unless a delivery of it already exists; when it is sent,
    /// apply `completion` to the report.
    pub(crate) async fn deliver(
        &self,
        report: &SecurityReport,
        kind: SecurityReplyKind,
        completion: Option<Completion>,
    ) -> Result<Delivery> {
        if !self.tracker.claim_security_reply(report.id, kind)? {
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
                self.tracker.complete_security_reply(
                    report.id,
                    kind,
                    DeliveryStatus::Sent,
                    None,
                )?;
                self.record_sent(report, kind, status, completion)?;
                Ok(Delivery::Sent)
            }
            ReporterReply::Rejected(error) => {
                let error = truncated_error(&error);
                self.tracker.complete_security_reply(
                    report.id,
                    kind,
                    DeliveryStatus::Failed,
                    Some(&error),
                )?;
                self.tracker.record_security_event(
                    Some(report.id),
                    None,
                    &NewSecurityEvent::new(SecurityEventKind::ReplyFailed)
                        .with_detail(json!({ "kind": kind, "error": error })),
                )?;
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
    /// sent (for example one an admin confirmed as delivered).
    pub(crate) async fn ensure_delivered(
        &self,
        report: &SecurityReport,
        kind: SecurityReplyKind,
        completion: Completion,
    ) -> Result<Delivery> {
        let existing = self
            .tracker
            .list_security_replies(report.id)?
            .into_iter()
            .find(|reply| reply.kind == kind)
            .map(|reply| reply.status);
        match existing {
            None | Some(DeliveryStatus::Failed) => {
                self.deliver(report, kind, Some(completion)).await
            }
            Some(DeliveryStatus::Sent) => {
                let (from, to) = completion;
                self.tracker.transition_security_report(
                    report.id,
                    from,
                    to,
                    &SecurityReportChanges::default(),
                    &NewSecurityEvent::new(SecurityEventKind::ReplySent)
                        .with_detail(json!({ "kind": kind, "recovered": true })),
                )?;
                Ok(Delivery::Skipped)
            }
            Some(DeliveryStatus::Sending | DeliveryStatus::Unconfirmed) => Ok(Delivery::Skipped),
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
        )?;
        self.tracker.record_security_event(
            Some(report.id),
            None,
            &NewSecurityEvent::new(SecurityEventKind::ReplyUnconfirmed)
                .with_detail(json!({ "kind": kind, "error": error })),
        )?;
        let subject = match report.vulnerability_id {
            Some(vulnerability_id) => {
                match self.tracker.get_security_vulnerability(vulnerability_id)? {
                    Some(vulnerability) => PingSubject::for_report(report.id, &vulnerability),
                    None => PingSubject::report(report.id),
                }
            }
            None => PingSubject::report(report.id),
        };
        self.ping(
            OwnerAction::NeedsReview(ReviewReason::ReplyUnconfirmed),
            subject,
        )
        .await;
        Ok(())
    }

    fn record_sent(
        &self,
        report: &SecurityReport,
        kind: SecurityReplyKind,
        status: Option<ConversationStatus>,
        completion: Option<Completion>,
    ) -> Result<()> {
        let event = NewSecurityEvent::new(SecurityEventKind::ReplySent).with_detail(json!({
            "kind": kind,
            "conversation_status": status.map(ConversationStatus::as_str),
        }));
        let completed = match completion {
            Some((from, to)) => self.tracker.transition_security_report(
                report.id,
                from,
                to,
                &SecurityReportChanges::default(),
                &event,
            )?,
            None => false,
        };
        if !completed {
            self.tracker
                .record_security_event(Some(report.id), None, &event)?;
        }
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
