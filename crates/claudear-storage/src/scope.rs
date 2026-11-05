//! Which fix attempts, and the PRs, executions, metrics and activity derived
//! from them, a read counts.

use claudear_core::security::SECURITY_FIX_SOURCE;
use claudear_core::types::{FixAttemptStats, SourceStats};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AttemptScope {
    #[default]
    All,
    WithoutSecurity,
}

impl AttemptScope {
    /// The source whose work this scope leaves out, compared case-insensitively.
    pub fn excluded_source(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::WithoutSecurity => Some(SECURITY_FIX_SOURCE),
        }
    }

    pub fn includes(self, source: &str) -> bool {
        self.excluded_source()
            .is_none_or(|excluded| !source.eq_ignore_ascii_case(excluded))
    }

    /// Work with no source belongs to no excluded source.
    pub fn includes_optional(self, source: Option<&str>) -> bool {
        source.is_none_or(|source| self.includes(source))
    }

    /// A metric with no source aggregates every source's work, so only a scope
    /// that excludes nothing includes it.
    pub fn includes_metric(self, source: Option<&str>) -> bool {
        match source {
            Some(source) => self.includes(source),
            None => self == Self::All,
        }
    }

    /// Work attributed to several sources counts only when every one is included.
    pub fn includes_all<'a>(self, sources: impl IntoIterator<Item = &'a str>) -> bool {
        sources.into_iter().all(|source| self.includes(source))
    }

    /// Removes the excluded sources' buckets from `stats`, and their attempts
    /// from the totals, for stores that cannot scope the count itself.
    pub fn scrub(self, stats: &mut FixAttemptStats) {
        let excluded: Vec<String> = stats
            .by_source
            .keys()
            .filter(|source| !self.includes(source))
            .cloned()
            .collect();
        for source in excluded {
            if let Some(hidden) = stats.by_source.remove(&source) {
                subtract(stats, &hidden);
            }
        }
    }
}

fn subtract(stats: &mut FixAttemptStats, hidden: &SourceStats) {
    let settled =
        hidden.success + hidden.failed + hidden.merged + hidden.closed + hidden.cannot_fix;
    stats.total = stats.total.saturating_sub(hidden.total);
    stats.pending = stats
        .pending
        .saturating_sub(hidden.total.saturating_sub(settled));
    stats.success = stats.success.saturating_sub(hidden.success);
    stats.failed = stats.failed.saturating_sub(hidden.failed);
    stats.merged = stats.merged.saturating_sub(hidden.merged);
    stats.closed = stats.closed.saturating_sub(hidden.closed);
    stats.cannot_fix = stats.cannot_fix.saturating_sub(hidden.cannot_fix);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_includes_every_source() {
        assert_eq!(AttemptScope::All.excluded_source(), None);
        assert!(AttemptScope::All.includes(SECURITY_FIX_SOURCE));
        assert!(AttemptScope::All.includes_all([SECURITY_FIX_SOURCE, "linear"]));
    }

    #[test]
    fn without_security_excludes_the_security_source_in_any_case() {
        let scope = AttemptScope::WithoutSecurity;
        assert_eq!(scope.excluded_source(), Some(SECURITY_FIX_SOURCE));
        assert!(!scope.includes(SECURITY_FIX_SOURCE));
        assert!(!scope.includes("SECURITY"));
        assert!(scope.includes("linear"));
        assert!(scope.includes_optional(None));
        assert!(!scope.includes_optional(Some("Security")));
        assert!(!scope.includes_all(["linear", SECURITY_FIX_SOURCE]));
        assert!(scope.includes_all(["linear", "sentry"]));
    }

    #[test]
    fn only_all_includes_unattributed_metrics() {
        assert!(AttemptScope::All.includes_metric(None));
        assert!(AttemptScope::All.includes_metric(Some(SECURITY_FIX_SOURCE)));
        let scope = AttemptScope::WithoutSecurity;
        assert!(!scope.includes_metric(None));
        assert!(!scope.includes_metric(Some("Security")));
        assert!(scope.includes_metric(Some("linear")));
    }

    #[test]
    fn scrub_removes_the_excluded_bucket_and_its_attempts() {
        let mut stats = FixAttemptStats {
            total: 5,
            pending: 2,
            success: 1,
            failed: 2,
            ..FixAttemptStats::default()
        };
        stats.by_source.insert(
            "Security".to_string(),
            SourceStats {
                total: 3,
                failed: 2,
                ..SourceStats::default()
            },
        );
        stats.by_source.insert(
            "linear".to_string(),
            SourceStats {
                total: 2,
                success: 1,
                ..SourceStats::default()
            },
        );

        let mut all = stats.clone();
        AttemptScope::All.scrub(&mut all);
        assert_eq!(all.total, 5);
        assert_eq!(all.by_source.len(), 2);

        AttemptScope::WithoutSecurity.scrub(&mut stats);
        assert_eq!(stats.total, 2);
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.success, 1);
        assert_eq!(stats.by_source.keys().collect::<Vec<_>>(), ["linear"]);
    }
}
