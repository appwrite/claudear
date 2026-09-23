//! Live-QA probe seam and verdict classification.
//!
//! External host / browser automation is **not** invoked in CI. Production
//! agents follow the playbook; this trait is the hook for future hermetic
//! probes and for classifying the agent's report.

use async_trait::async_trait;
use claudear_core::error::Result;

/// Outcome of a deploy-QA attempt, used for Discord posting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployQaVerdict {
    /// No LIVE-TESTABLE failures.
    AllVerified,
    /// At least one LIVE failure (or an explicit FAIL verdict).
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
/// Prefers an explicit `DEPLOY_QA_VERDICT:` footer. Falls back to scanning
/// for a LIVE FAIL line so a slightly messy report still threads correctly.
pub fn classify_deploy_qa_verdict(report: &str) -> DeployQaVerdict {
    for line in report.lines().rev() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("DEPLOY_QA_VERDICT:") {
            let value = rest.trim().to_ascii_uppercase();
            if value.contains("FAIL") {
                return DeployQaVerdict::Fail;
            }
            if value.contains("ALL_VERIFIED") || value.contains("VERIFIED") {
                return DeployQaVerdict::AllVerified;
            }
        }
    }

    let upper = report.to_ascii_uppercase();
    if upper.contains("LIVE FAIL") || upper.contains("LIVE FAILED") {
        return DeployQaVerdict::Fail;
    }
    DeployQaVerdict::AllVerified
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_prefers_footer() {
        let report = "\
- #1 foo LIVE PASS
DEPLOY_QA_VERDICT: ALL_VERIFIED
";
        assert_eq!(
            classify_deploy_qa_verdict(report),
            DeployQaVerdict::AllVerified
        );

        let fail = "\
- #2 bar LIVE FAIL
DEPLOY_QA_VERDICT: FAIL
";
        assert_eq!(classify_deploy_qa_verdict(fail), DeployQaVerdict::Fail);
    }

    #[test]
    fn verdict_fallback_scans_live_fail() {
        assert_eq!(
            classify_deploy_qa_verdict("- #9 x LIVE FAIL\nno footer"),
            DeployQaVerdict::Fail
        );
        assert_eq!(
            classify_deploy_qa_verdict("- #9 x INFRA\nall good"),
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
