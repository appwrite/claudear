use super::assessment::{ReportText, NO_TOOLS_TIMEOUT};
use super::prompt::{self, SCREENING_SCHEMA};
use super::SecurityInbox;
use claudear_core::error::{Error, Result};
use claudear_integrations::runner::SecuritySandbox;
use regex_lite::Regex;
use serde::Deserialize;
use std::sync::LazyLock;

const MIN_REPORT_CHARS: usize = 20;
const EMPTY_REPORT_CONFIDENCE: f64 = 1.0;

static HTML_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]*>").expect("valid tag pattern"));

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Screening {
    Spam { confidence: f64 },
    Support { mailbox_id: String, confidence: f64 },
    Report { confidence: Option<f64> },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScreeningVerdict {
    Spam,
    Support,
    Report,
}

#[derive(Debug, Deserialize)]
struct ScreeningResponse {
    verdict: ScreeningVerdict,
    confidence: f64,
}

/// A body with no text left after stripping markup, or under 20 characters,
/// is spam without asking an agent, unless the reporter attached files.
pub(crate) fn is_trivially_empty(report: &ReportText) -> bool {
    let text = HTML_TAG.replace_all(&report.body, " ");
    report.attachments.is_empty() && text.trim().chars().count() < MIN_REPORT_CHARS
}

impl SecurityInbox {
    pub(crate) async fn screen(&self, report: &ReportText) -> Result<Screening> {
        if is_trivially_empty(report) {
            return Ok(Screening::Spam {
                confidence: EMPTY_REPORT_CONFIDENCE,
            });
        }
        let sandbox = tempfile::tempdir()?;
        let value = self
            .agent
            .security_query(
                &prompt::screening_prompt(&report.prompt()),
                SCREENING_SCHEMA,
                sandbox.path(),
                SecuritySandbox::NoTools,
                NO_TOOLS_TIMEOUT,
            )
            .await?;
        let response: ScreeningResponse = serde_json::from_value(value)
            .map_err(|error| Error::Other(format!("malformed screening verdict: {error}")))?;
        let confidence = response.confidence.clamp(0.0, 1.0);
        let confident = confidence >= self.config.spam_confidence;
        Ok(match (response.verdict, &self.config.support_mailbox_id) {
            (ScreeningVerdict::Spam, _) if confident => Screening::Spam { confidence },
            (ScreeningVerdict::Support, Some(mailbox_id)) if confident => Screening::Support {
                mailbox_id: mailbox_id.clone(),
                confidence,
            },
            _ => Screening::Report {
                confidence: Some(confidence),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn report(body: &str, attachments: &[&str]) -> ReportText {
        ReportText {
            subject: "Report".to_string(),
            body: body.to_string(),
            attachments: attachments.iter().map(|name| name.to_string()).collect(),
            reported_at: Utc::now(),
        }
    }

    #[test]
    fn empty_or_short_bodies_are_trivial_only_without_attachments() {
        assert!(is_trivially_empty(&report("", &[])));
        assert!(is_trivially_empty(&report("<p> <br/> </p>", &[])));
        assert!(is_trivially_empty(&report("hi there", &[])));
        assert!(!is_trivially_empty(&report("hi there", &["poc.mp4"])));
        assert!(!is_trivially_empty(&report("", &["poc.png"])));
        assert!(!is_trivially_empty(&report(
            "The password reset token never expires.",
            &[]
        )));
    }
}
