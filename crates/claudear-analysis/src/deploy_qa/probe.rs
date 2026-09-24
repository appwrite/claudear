//! Live-QA probe seam and verdict classification.
//!
//! External host / browser automation is **not** invoked in CI. Production
//! agents follow the playbook; this trait is the hook for future hermetic
//! probes and for classifying the agent's report.

use async_trait::async_trait;
use claudear_core::error::Result;
use regex_lite::Regex;
use std::sync::LazyLock;

/// Prefix of the machine-readable footer that ends every deploy-QA report.
pub const VERDICT_PREFIX: &str = "DEPLOY_QA_VERDICT:";

/// Footer value declaring every LIVE-TESTABLE PR passed.
pub const VERDICT_ALL_VERIFIED: &str = "ALL_VERIFIED";

/// Footer value declaring nothing failed but at least one LIVE-TESTABLE PR
/// could not be checked.
pub const VERDICT_UNVERIFIED: &str = "UNVERIFIED";

/// Footer value declaring at least one LIVE-TESTABLE PR failed.
pub const VERDICT_FAIL: &str = "FAIL";

static LIVE_FAIL: LazyLock<Regex> = LazyLock::new(|| live_result("FAIL(ED)?"));

static LIVE_BLOCKED: LazyLock<Regex> = LazyLock::new(|| live_result("BLOCKED"));

/// Outcome of a deploy-QA attempt, used for Discord posting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployQaVerdict {
    /// The report ends with an all-verified footer and no LIVE-TESTABLE PR
    /// failed or was blocked.
    AllVerified,
    /// Nothing failed, but a LIVE-TESTABLE PR was blocked or the report lacks
    /// an all-verified footer.
    Unverified,
    /// A LIVE-TESTABLE PR failed.
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

/// Classify an agent report into a Discord posting verdict.
///
/// Any line reporting a word-bounded `LIVE FAIL` / `LIVE FAILED` result is a
/// [`DeployQaVerdict::Fail`], whatever the footer says. Otherwise the footer
/// decides: the last line starting with [`VERDICT_PREFIX`] (ignoring case,
/// surrounding whitespace, backticks and `*`). A [`VERDICT_FAIL`] footer is a
/// failure, and a [`VERDICT_ALL_VERIFIED`] footer is
/// [`DeployQaVerdict::AllVerified`] unless a line reports `LIVE BLOCKED`.
/// Everything else, including a blocked check and a missing or unrecognised
/// footer, is [`DeployQaVerdict::Unverified`].
pub fn classify_deploy_qa_verdict(report: &str) -> DeployQaVerdict {
    if reports(report, &LIVE_FAIL) {
        return DeployQaVerdict::Fail;
    }
    match footer(report) {
        Some(value) if value.eq_ignore_ascii_case(VERDICT_FAIL) => DeployQaVerdict::Fail,
        Some(value)
            if value.eq_ignore_ascii_case(VERDICT_ALL_VERIFIED)
                && !reports(report, &LIVE_BLOCKED) =>
        {
            DeployQaVerdict::AllVerified
        }
        _ => DeployQaVerdict::Unverified,
    }
}

fn live_result(result: &str) -> Regex {
    Regex::new(&format!(r"(?i)\bLIVE\s+{result}\b")).expect("live result pattern is valid")
}

fn reports(report: &str, result: &Regex) -> bool {
    report.lines().any(|line| result.is_match(line))
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
    fn unverified_footer_is_unverified() {
        let text = report(&["- #1 foo LIVE PASS", &verdict_line(VERDICT_UNVERIFIED)]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::Unverified
        );
    }

    #[test]
    fn missing_footer_is_unverified() {
        let text = report(&["- #1 ci INFRA", "- #2 docs INFRA", "all good"]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::Unverified
        );
    }

    #[test]
    fn garbled_footer_is_unverified() {
        let text = report(&["- #1 foo LIVE PASS", &verdict_line("MOSTLY_FINE")]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::Unverified
        );
    }

    #[test]
    fn live_fail_line_without_footer_fails() {
        let text = report(&["- #1 foo LIVE FAIL", "no footer"]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn live_failed_line_fails() {
        let text = report(&[
            "- #1 foo live failed: 500 on /v1/health",
            &verdict_line(VERDICT_UNVERIFIED),
        ]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn blocked_line_without_footer_is_unverified() {
        let text = report(&["- #1 foo LIVE BLOCKED", "- #2 ci INFRA"]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::Unverified
        );
    }

    #[test]
    fn blocked_line_overrides_all_verified_footer() {
        let text = report(&[
            "- #1 foo LIVE PASS",
            "- #2 bar live blocked",
            &verdict_line(VERDICT_ALL_VERIFIED),
        ]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::Unverified
        );
    }

    #[test]
    fn blocked_line_does_not_soften_fail_footer() {
        let text = report(&["- #1 foo LIVE BLOCKED", &verdict_line(VERDICT_FAIL)]);
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
    fn live_fail_line_overrides_unverified_footer() {
        let text = report(&[
            "- #1 foo LIVE BLOCKED",
            "- #2 bar LIVE FAIL",
            &verdict_line(VERDICT_UNVERIFIED),
        ]);
        assert_eq!(classify_deploy_qa_verdict(&text), DeployQaVerdict::Fail);
    }

    #[test]
    fn keep_alive_failures_are_not_a_live_failure() {
        let text = report(&[
            "- #1 fix keep-alive failures on edge LIVE PASS",
            &verdict_line(VERDICT_ALL_VERIFIED),
        ]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::AllVerified
        );
    }

    #[test]
    fn live_failover_is_not_a_live_failure() {
        let text = report(&[
            "- #1 live failover drill for fra LIVE PASS",
            &verdict_line(VERDICT_ALL_VERIFIED),
        ]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::AllVerified
        );
    }

    #[test]
    fn live_and_fail_on_separate_lines_are_not_a_live_failure() {
        let text = report(&[
            "- #1 foo LIVE",
            "FAIL handling documented; LIVE PASS",
            &verdict_line(VERDICT_ALL_VERIFIED),
        ]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::AllVerified
        );
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
    fn last_unverified_footer_overrides_earlier_all_verified() {
        let text = report(&[
            &verdict_line(VERDICT_ALL_VERIFIED),
            "- #1 foo LIVE PASS",
            &verdict_line(VERDICT_UNVERIFIED),
        ]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::Unverified
        );
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

    #[test]
    fn bold_unverified_footer_is_unverified() {
        let text = report(&[
            "- #1 foo LIVE BLOCKED",
            &format!("**{}**", verdict_line(VERDICT_UNVERIFIED)),
        ]);
        assert_eq!(
            classify_deploy_qa_verdict(&text),
            DeployQaVerdict::Unverified
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
