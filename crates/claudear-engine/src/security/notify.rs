//! Minimal owner pings. A ping names only the action, ids, severity and a
//! dashboard link: never the reporter, subject, summary or any free text.

use super::SecurityInbox;
use claudear_core::security::{security_fix_short_id, SecuritySeverity, SecurityVulnerability};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewReason {
    AssessmentFailed,
    AssessmentInconclusive,
    UnverifiableFix,
    AttachmentEvidence,
    UntrackedRepository,
    PullRequestClosed,
    FixFailed,
    ReplyUnconfirmed,
    DeploymentCheckFailed,
}

impl ReviewReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AssessmentFailed => "assessment_failed",
            Self::AssessmentInconclusive => "assessment_inconclusive",
            Self::UnverifiableFix => "unverifiable_fix",
            Self::AttachmentEvidence => "attachment_evidence",
            Self::UntrackedRepository => "untracked_repository",
            Self::PullRequestClosed => "pull_request_closed",
            Self::FixFailed => "fix_failed",
            Self::ReplyUnconfirmed => "reply_unconfirmed",
            Self::DeploymentCheckFailed => "deployment_check_failed",
        }
    }
}

impl fmt::Display for ReviewReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerAction {
    NewValidReport,
    NeedsReview(ReviewReason),
    CloudBumpRequired,
    AwardReady,
    ReporterFollowUp,
}

impl fmt::Display for OwnerAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewValidReport => formatter.write_str("new valid report"),
            Self::NeedsReview(reason) => write!(formatter, "needs review ({reason})"),
            Self::CloudBumpRequired => formatter.write_str("cloud server-ce bump required"),
            Self::AwardReady => formatter.write_str("award ready"),
            Self::ReporterFollowUp => formatter.write_str("reporter follow-up"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PingSubject {
    pub report_id: i64,
    pub vulnerability_id: Option<i64>,
    pub severity: Option<SecuritySeverity>,
}

impl PingSubject {
    pub(crate) fn report(report_id: i64) -> Self {
        Self {
            report_id,
            vulnerability_id: None,
            severity: None,
        }
    }

    pub(crate) fn vulnerability(vulnerability: &SecurityVulnerability) -> Self {
        Self::for_report(vulnerability.first_report_id, vulnerability)
    }

    pub(crate) fn for_report(report_id: i64, vulnerability: &SecurityVulnerability) -> Self {
        Self {
            report_id,
            vulnerability_id: Some(vulnerability.id),
            severity: vulnerability.severity,
        }
    }
}

pub(crate) fn owner_message(
    owner_mention: Option<&str>,
    dashboard_url: Option<&str>,
    action: OwnerAction,
    subject: PingSubject,
) -> String {
    let vulnerability = subject
        .vulnerability_id
        .map(security_fix_short_id)
        .unwrap_or_else(|| "SEC--".to_string());
    let severity = subject
        .severity
        .map(|severity| severity.as_str())
        .unwrap_or("unrated");
    let mut message = String::new();
    if let Some(mention) = owner_mention.filter(|mention| !mention.trim().is_empty()) {
        message.push_str(mention.trim());
        message.push(' ');
    }
    message.push_str(&format!(
        "[security] {action} for {vulnerability} / report #{} (severity {severity}).",
        subject.report_id
    ));
    if let Some(url) = dashboard_url.filter(|url| !url.trim().is_empty()) {
        message.push_str(&format!(
            " {}/security?report={}",
            url.trim().trim_end_matches('/'),
            subject.report_id
        ));
    }
    message
}

impl SecurityInbox {
    pub(crate) async fn ping(&self, action: OwnerAction, subject: PingSubject) {
        let message = owner_message(
            self.config.owner_mention.as_deref(),
            self.config.dashboard_url.as_deref(),
            action,
            subject,
        );
        if let Err(error) = self.notifier.notify_status(&message).await {
            tracing::warn!(error = %error, report_id = subject.report_id, "security owner ping failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_names_only_the_action_ids_severity_and_link() {
        let message = owner_message(
            Some("<@123>"),
            Some("https://claudear.example/"),
            OwnerAction::NeedsReview(ReviewReason::AttachmentEvidence),
            PingSubject {
                report_id: 7,
                vulnerability_id: Some(3),
                severity: Some(SecuritySeverity::High),
            },
        );
        assert_eq!(
            message,
            "<@123> [security] needs review (attachment_evidence) for SEC-3 / report #7 (severity high). https://claudear.example/security?report=7"
        );
    }

    #[test]
    fn message_without_mention_link_or_vulnerability_stays_minimal() {
        assert_eq!(
            owner_message(
                None,
                None,
                OwnerAction::ReporterFollowUp,
                PingSubject::report(9)
            ),
            "[security] reporter follow-up for SEC-- / report #9 (severity unrated)."
        );
    }
}
