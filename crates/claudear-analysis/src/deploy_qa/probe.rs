//! Live-QA probe seam and verdict classification.
//!
//! External host / browser automation is **not** invoked in CI. Production
//! agents follow the playbook; this trait is the hook for future hermetic
//! probes and for classifying the agent's report.

use async_trait::async_trait;
use claudear_core::error::Result;

/// Prefix of the machine-readable footer that ends every deploy-QA report.
pub const VERDICT_PREFIX: &str = "DEPLOY_QA_VERDICT:";

/// Footer value declaring every LIVE-TESTABLE PR passed.
pub const VERDICT_ALL_VERIFIED: &str = "ALL_VERIFIED";

/// Footer value declaring at least one live failure or blocked check.
pub const VERDICT_FAIL: &str = "FAIL";

const LIVE_FAIL: &str = "LIVE FAIL";

const LIVE_BLOCKED: &str = "LIVE BLOCKED";

/// Outcome of a deploy-QA attempt, used for Discord posting.
///
/// Classification is fail-closed: anything short of an explicit all-verified
/// report is a [`DeployQaVerdict::Fail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployQaVerdict {
    /// The report ends with an all-verified footer and no LIVE-TESTABLE PR
    /// failed or was blocked.
    AllVerified,
    /// A live failure, a blocked check, or no explicit all-verified verdict.
    Fail,
}

/// Optional live probe used by tests / future tool injection.
///
/// The production agent performs live checks itself. Implementations of this
/// trait must not require production secrets; [`NoopLiveQaProbe`] is the CI
/// default.
#[async_trait]
pub trait LiveQaProbe: Send + Sync {
    /// Run a named probe. `host` / `path` are informational only.
    async fn probe(&self, name: &str, host: &str, path: &str) -> Result<LiveQaProbeResult>;
}

/// Result of a single probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveQaProbeResult {
    /// Probe name.
    pub name: String,
    /// Whether the probe passed.
    pub passed: bool,
    /// Short note (no curl dumps).
    pub detail: String,
}

/// CI-safe no-op probe. Always reports "not run".
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopLiveQaProbe;

#[async_trait]
impl LiveQaProbe for NoopLiveQaProbe {
    async fn probe(&self, name: &str, _host: &str, _path: &str) -> Result<LiveQaProbeResult> {
        Ok(LiveQaProbeResult {
            name: name.to_string(),
            passed: false,
            detail: "noop: live hosts are not contacted in CI".to_string(),
        })
    }
}

/// Classify an agent report into a Discord posting verdict, failing closed.
///
/// The footer is the last line starting with [`VERDICT_PREFIX`] (ignoring
/// case, surrounding whitespace, backticks and `*`). The report is
/// [`DeployQaVerdict::AllVerified`] only when that footer's value is
/// [`VERDICT_ALL_VERIFIED`] (ignoring case) and no line reports a `LIVE FAIL`
/// or `LIVE BLOCKED` result. Everything else, including a missing or
/// unrecognised footer, is a [`DeployQaVerdict::Fail`].
pub fn classify_deploy_qa_verdict(report: &str) -> DeployQaVerdict {
    let upper = report.to_ascii_uppercase();
    if upper.contains(LIVE_FAIL) || upper.contains(LIVE_BLOCKED) {
        return DeployQaVerdict::Fail;
    }
    match footer(report) {
        Some(value) if value.eq_ignore_ascii_case(VERDICT_ALL_VERIFIED) => {
            DeployQaVerdict::AllVerified
        }
        _ => DeployQaVerdict::Fail,
    }
}

fn footer(report: &str) -> Option<&str> {
    report
        .lines()
        .rev()
        .find_map(|line| strip_prefix_ignore_case(trim_markup(line), VERDICT_PREFIX))
        .map(trim_markup)
}

fn trim_markup(text: &str) -> &str {
    text.trim_matches(|character: char| {
        character.is_whitespace() || character == '`' || character == '*'
    })
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict_line(value: &str) -> String {
        format!("{VERDICT_PREFIX} {value}")
    }

    fn report(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn all_verified_footer_passes() {
        let text = report(&[
            "- #1 foo LIVE PASS",
            "- #2 ci INFRA",
            &verdict_line(VERDICT_ALL_VERIFIED),
        ]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::AllVerified
        );
    }

    #[test]
    fn fail_footer_fails() {
        let text = report(&["- #1 foo LIVE PASS", &verdict_line(VERDICT_FAIL)]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn missing_footer_fails() {
        let text = report(&["- #1 ci INFRA", "- #2 docs INFRA", "all good"]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn live_fail_line_without_footer_fails() {
        let text = report(&["- #1 foo LIVE FAIL", "no footer"]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn blocked_line_without_footer_fails() {
        let text = report(&["- #1 foo LIVE BLOCKED", "- #2 ci INFRA"]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn blocked_line_overrides_all_verified_footer() {
        let text = report(&[
            "- #1 foo LIVE PASS",
            "- #2 bar live blocked",
            &verdict_line(VERDICT_ALL_VERIFIED),
        ]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn live_fail_line_overrides_all_verified_footer() {
        let text = report(&[
            "- #1 foo LIVE PASS",
            "- #2 bar LIVE FAIL",
            &verdict_line(VERDICT_ALL_VERIFIED),
        ]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn unverified_footer_fails() {
        let text = report(&["- #1 foo LIVE PASS", &verdict_line("UNVERIFIED")]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn lowercase_all_verified_value_passes() {
        let text = report(&[
            "- #1 foo LIVE PASS",
            &verdict_line(&VERDICT_ALL_VERIFIED.to_ascii_lowercase()),
        ]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::AllVerified
        );
    }

    #[test]
    fn lowercase_prefix_is_recognised() {
        let line = format!(
            "{} {VERDICT_ALL_VERIFIED}",
            VERDICT_PREFIX.to_ascii_lowercase()
        );
        let text = report(&["- #1 foo LIVE PASS", &line]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::AllVerified
        );
    }

    #[test]
    fn last_all_verified_footer_overrides_earlier_fail() {
        let text = report(&[
            &verdict_line(VERDICT_FAIL),
            "- #1 foo LIVE PASS (retried after flaky host)",
            &verdict_line(VERDICT_ALL_VERIFIED),
        ]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::AllVerified
        );
    }

    #[test]
    fn last_fail_footer_overrides_earlier_all_verified() {
        let text = report(&[
            &verdict_line(VERDICT_ALL_VERIFIED),
            "- #1 foo LIVE PASS",
            &verdict_line(VERDICT_FAIL),
        ]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn backticked_fail_footer_fails() {
        let text = report(&[
            "- #1 foo LIVE PASS",
            &format!("`{}`", verdict_line(VERDICT_FAIL)),
        ]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn bold_fail_footer_fails() {
        let text = report(&[
            "- #1 foo LIVE PASS",
            &format!("**{}**", verdict_line(VERDICT_FAIL)),
        ]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn backticked_all_verified_footer_passes() {
        let text = report(&[
            "- #1 foo LIVE PASS",
            &format!("`{}`", verdict_line(VERDICT_ALL_VERIFIED)),
        ]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::AllVerified
        );
    }

    #[tokio::test]
    async fn noop_probe_does_not_touch_network() {
        let probe = NoopLiveQaProbe;
        let result = probe
            .probe("health", "cloud.appwrite.io", "/v1/health")
            .await
            .unwrap();
        assert!(!result.passed);
        assert!(result.detail.contains("noop"));
    }
}
