//! Severity scoring.
//!
//! Computes a composite numeric score from multiple signals:
//! severity, frequency, regression risk, blast radius, and cluster membership.

use crate::prioritisation::blast_radius::blast_radius_score;
use claudear_config::config::PrioritisationConfig;
use claudear_core::types::{
    BlastRadius, Issue, IssuePriority, MatchPriority, MatchResult, SeverityScore,
};

/// Compute the severity score for a single issue.
pub fn compute(
    issue: &Issue,
    match_result: &MatchResult,
    blast_radius: BlastRadius,
    in_cluster: bool,
    config: &PrioritisationConfig,
) -> SeverityScore {
    let severity_component = severity_component(issue.priority, match_result.priority);
    let frequency_component = frequency_component(issue);
    let regression_component = regression_component(issue);
    let br_component = blast_radius_score(blast_radius);
    let cluster_boost = if in_cluster { 1.0 } else { 0.0 };

    let score = config.severity_weight * severity_component
        + config.frequency_weight * frequency_component
        + config.regression_weight * regression_component
        + config.blast_radius_weight * br_component
        + config.cluster_weight * cluster_boost;

    SeverityScore {
        score,
        severity_component,
        frequency_component,
        regression_component,
        blast_radius_component: br_component,
        cluster_boost,
    }
}

/// Blend of `IssuePriority` (60%) and `MatchPriority` (40%).
fn severity_component(issue_prio: IssuePriority, match_prio: MatchPriority) -> f64 {
    let ip = match issue_prio {
        IssuePriority::None => 0.0,
        IssuePriority::Low => 0.25,
        IssuePriority::Medium => 0.5,
        IssuePriority::High => 0.75,
        IssuePriority::Critical => 1.0,
    };
    let mp = match match_prio {
        MatchPriority::Low => 0.25,
        MatchPriority::Normal => 0.5,
        MatchPriority::High => 0.75,
        MatchPriority::Urgent => 1.0,
    };
    0.6 * ip + 0.4 * mp
}

/// Frequency component: log-scaled event_count (40%), user_count (30%), escalation_rate (30%).
fn frequency_component(issue: &Issue) -> f64 {
    let event_count = issue
        .get_metadata::<f64>("event_count")
        .or_else(|| issue.get_metadata::<i64>("event_count").map(|v| v as f64))
        .unwrap_or(1.0)
        .max(0.0);
    let user_count = issue
        .get_metadata::<f64>("user_count")
        .or_else(|| issue.get_metadata::<i64>("user_count").map(|v| v as f64))
        .unwrap_or(0.0)
        .max(0.0);
    let escalation_rate = issue.get_metadata::<f64>("escalation_rate").unwrap_or(0.0);

    // Log-scale event count up to 1M so 10k and 1M events still rank apart
    let event_score = if event_count <= 1.0 {
        0.0
    } else {
        (event_count.log10() / 6.0).min(1.0)
    };

    // Log-scale user count up to 10k
    let user_score = if user_count <= 1.0 {
        0.0
    } else {
        (user_count.log10() / 4.0).min(1.0)
    };

    // Sources report escalation as a percentage (0-100)
    let escalation_score = (escalation_rate / 100.0).clamp(0.0, 1.0);

    0.4 * event_score + 0.3 * user_score + 0.3 * escalation_score
}

/// Regression risk: is_unhandled + error level (fatal/error/warning).
fn regression_component(issue: &Issue) -> f64 {
    let is_unhandled = issue.get_metadata::<bool>("is_unhandled").unwrap_or(false);
    let level = issue
        .get_metadata::<String>("level")
        .unwrap_or_default()
        .to_lowercase();

    let unhandled_score: f64 = if is_unhandled { 0.5 } else { 0.0 };

    let level_score: f64 = match level.as_str() {
        "fatal" => 1.0,
        "error" => 0.7,
        "warning" => 0.4,
        _ => 0.0,
    };

    // Combine: 50% unhandled, 50% level
    (0.5 * unhandled_score + 0.5 * level_score).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_core::types::{Issue, IssuePriority, MatchPriority, MatchResult};

    fn default_config() -> PrioritisationConfig {
        PrioritisationConfig::default()
    }

    fn make_issue(priority: IssuePriority) -> Issue {
        let mut issue = Issue::new("id", "SH-1", "title", "url", "sentry");
        issue.priority = priority;
        issue
    }

    fn make_match(priority: MatchPriority) -> MatchResult {
        MatchResult::matched("test", priority)
    }

    #[test]
    fn high_priority_scores_higher() {
        let config = default_config();
        let high = compute(
            &make_issue(IssuePriority::Critical),
            &make_match(MatchPriority::Urgent),
            BlastRadius::Critical,
            true,
            &config,
        );
        let low = compute(
            &make_issue(IssuePriority::Low),
            &make_match(MatchPriority::Low),
            BlastRadius::Cosmetic,
            false,
            &config,
        );
        assert!(
            high.score > low.score,
            "high={} low={}",
            high.score,
            low.score
        );
    }

    #[test]
    fn severity_component_blends() {
        let sc = severity_component(IssuePriority::Critical, MatchPriority::Urgent);
        // 0.6*1.0 + 0.4*1.0 = 1.0
        assert!((sc - 1.0).abs() < 0.001);

        let sc = severity_component(IssuePriority::None, MatchPriority::Low);
        // 0.6*0.0 + 0.4*0.25 = 0.1
        assert!((sc - 0.1).abs() < 0.001);
    }

    #[test]
    fn frequency_with_events() {
        let mut issue = make_issue(IssuePriority::Medium);
        issue.set_metadata("event_count", 1000i64);
        issue.set_metadata("user_count", 100i64);
        issue.set_metadata("escalation_rate", 50.0);
        let fc = frequency_component(&issue);
        assert!(fc > 0.0);
        assert!(fc <= 1.0);
    }

    #[test]
    fn faster_escalation_ranks_higher() {
        // Sentry reports escalation as a percentage; clamping it as a fraction
        // made any growth at all score the same as doubling
        let config = default_config();
        let mr = make_match(MatchPriority::Normal);
        let mut slight = make_issue(IssuePriority::Medium);
        slight.set_metadata("escalation_rate", 10.0);
        let mut doubling = make_issue(IssuePriority::Medium);
        doubling.set_metadata("escalation_rate", 100.0);

        let slight = compute(&slight, &mr, BlastRadius::Core, false, &config);
        let doubling = compute(&doubling, &mr, BlastRadius::Core, false, &config);
        assert!(doubling.score > slight.score);
    }

    #[test]
    fn busier_issues_keep_ranking_higher_past_ten_thousand_events() {
        let mut busy = make_issue(IssuePriority::Medium);
        busy.set_metadata("event_count", 10_000i64);
        let mut busier = make_issue(IssuePriority::Medium);
        busier.set_metadata("event_count", 1_000_000i64);
        let config = default_config();
        let mr = make_match(MatchPriority::Normal);
        let busy = compute(&busy, &mr, BlastRadius::Core, false, &config);
        let busier = compute(&busier, &mr, BlastRadius::Core, false, &config);
        assert!(busier.score > busy.score);
    }

    #[test]
    fn regression_unhandled_fatal() {
        let mut issue = make_issue(IssuePriority::Medium);
        issue.set_metadata("is_unhandled", true);
        issue.set_metadata("level", "fatal");
        let rc = regression_component(&issue);
        // 0.5*0.5 + 0.5*1.0 = 0.75
        assert!((rc - 0.75).abs() < 0.001);
    }

    #[test]
    fn regression_handled_no_level() {
        let issue = make_issue(IssuePriority::Medium);
        let rc = regression_component(&issue);
        assert!((rc - 0.0).abs() < 0.001);
    }

    #[test]
    fn cluster_boost_adds_weight() {
        let config = default_config();
        let issue = make_issue(IssuePriority::Medium);
        let mr = make_match(MatchPriority::Normal);
        let with = compute(&issue, &mr, BlastRadius::Core, true, &config);
        let without = compute(&issue, &mr, BlastRadius::Core, false, &config);
        let diff = with.score - without.score;
        assert!((diff - config.cluster_weight).abs() < 0.001);
    }

    #[test]
    fn score_components_in_range() {
        let mut issue = make_issue(IssuePriority::High);
        issue.set_metadata("event_count", 500i64);
        issue.set_metadata("is_unhandled", true);
        issue.set_metadata("level", "error");
        let mr = make_match(MatchPriority::High);
        let config = default_config();
        let score = compute(&issue, &mr, BlastRadius::Infrastructure, true, &config);

        assert!(score.severity_component >= 0.0 && score.severity_component <= 1.0);
        assert!(score.frequency_component >= 0.0 && score.frequency_component <= 1.0);
        assert!(score.regression_component >= 0.0 && score.regression_component <= 1.0);
        assert!(score.blast_radius_component >= 0.0 && score.blast_radius_component <= 1.0);
        assert!(score.cluster_boost == 0.0 || score.cluster_boost == 1.0);
    }

    #[test]
    fn test_frequency_no_metadata() {
        // Issue with no event_count, user_count, or escalation_rate metadata
        let issue = make_issue(IssuePriority::Medium);
        let fc = frequency_component(&issue);
        // event_count defaults to 1.0 -> event_score = 0.0 (since <= 1.0)
        // user_count defaults to 0.0 -> user_score = 0.0 (since <= 1.0)
        // escalation_rate defaults to 0.0 -> escalation_score = 0.0
        // 0.4*0.0 + 0.3*0.0 + 0.3*0.0 = 0.0
        assert!(
            fc.abs() < 1e-10,
            "frequency with no metadata should be ~0.0, got {}",
            fc
        );
    }

    #[test]
    fn test_frequency_very_high_events() {
        let mut issue = make_issue(IssuePriority::Medium);
        issue.set_metadata("event_count", 10_000_000i64);
        let fc = frequency_component(&issue);
        // log10(10_000_000) / 6 = ~1.17, clamped to 1.0
        // event_score = 1.0
        // user_score = 0.0 (no user_count), escalation = 0.0
        // 0.4*1.0 + 0.3*0.0 + 0.3*0.0 = 0.4
        let expected = 0.4;
        assert!(
            (fc - expected).abs() < 0.001,
            "frequency with 10M events should be {}, got {}",
            expected,
            fc
        );
        // Also verify the event sub-score was clamped
        assert!(fc <= 1.0, "frequency_component must not exceed 1.0");
    }

    #[test]
    fn test_frequency_single_event() {
        let mut issue = make_issue(IssuePriority::Medium);
        issue.set_metadata("event_count", 1i64);
        let fc = frequency_component(&issue);
        // event_count=1.0 -> event_score = 0.0 (boundary: <= 1.0 returns 0.0)
        // user_score = 0.0, escalation = 0.0
        assert!(
            fc.abs() < 1e-10,
            "single event should yield frequency ~0.0, got {}",
            fc
        );
    }

    #[test]
    fn test_regression_warning_level() {
        let mut issue = make_issue(IssuePriority::Medium);
        issue.set_metadata("level", "warning");
        // is_unhandled defaults to false -> unhandled_score = 0.0
        let rc = regression_component(&issue);
        // 0.5*0.0 + 0.5*0.4 = 0.2
        assert!(
            (rc - 0.2).abs() < 0.001,
            "warning level regression should be 0.2, got {}",
            rc
        );
    }

    #[test]
    fn test_regression_error_level() {
        let mut issue = make_issue(IssuePriority::Medium);
        issue.set_metadata("level", "error");
        let rc = regression_component(&issue);
        // 0.5*0.0 + 0.5*0.7 = 0.35
        assert!(
            (rc - 0.35).abs() < 0.001,
            "error level regression should be 0.35, got {}",
            rc
        );
    }

    #[test]
    fn test_regression_unknown_level() {
        let mut issue = make_issue(IssuePriority::Medium);
        issue.set_metadata("level", "info");
        let rc = regression_component(&issue);
        // "info" is not fatal/error/warning -> level_score = 0.0
        // 0.5*0.0 + 0.5*0.0 = 0.0
        assert!(
            rc.abs() < 1e-10,
            "info level regression should be 0.0, got {}",
            rc
        );
    }

    #[test]
    fn test_severity_component_all_variants() {
        // All IssuePriority x MatchPriority combinations
        let issue_prios = [
            (IssuePriority::None, 0.0),
            (IssuePriority::Low, 0.25),
            (IssuePriority::Medium, 0.5),
            (IssuePriority::High, 0.75),
            (IssuePriority::Critical, 1.0),
        ];
        let match_prios = [
            (MatchPriority::Low, 0.25),
            (MatchPriority::Normal, 0.5),
            (MatchPriority::High, 0.75),
            (MatchPriority::Urgent, 1.0),
        ];

        for (ip, ip_val) in &issue_prios {
            for (mp, mp_val) in &match_prios {
                let expected = 0.6 * ip_val + 0.4 * mp_val;
                let actual = severity_component(*ip, *mp);
                assert!(
                    (actual - expected).abs() < 0.001,
                    "severity_component({:?}, {:?}): expected {}, got {}",
                    ip,
                    mp,
                    expected,
                    actual
                );
            }
        }
    }

    #[test]
    fn test_custom_weights() {
        let config = claudear_config::config::PrioritisationConfig {
            severity_weight: 0.5,
            frequency_weight: 0.2,
            regression_weight: 0.1,
            blast_radius_weight: 0.1,
            cluster_weight: 0.1,
            ..Default::default()
        };
        let issue = make_issue(IssuePriority::Critical);
        let mr = make_match(MatchPriority::Urgent);
        let score = compute(&issue, &mr, BlastRadius::Critical, true, &config);

        // severity_component = 0.6*1.0 + 0.4*1.0 = 1.0
        // frequency_component = 0.0 (no metadata)
        // regression_component = 0.0 (no metadata)
        // blast_radius_component = 1.0 (Critical)
        // cluster_boost = 1.0 (in cluster)
        let expected = 0.5 * 1.0 + 0.2 * 0.0 + 0.1 * 0.0 + 0.1 * 1.0 + 0.1 * 1.0;
        assert!(
            (score.score - expected).abs() < 0.001,
            "custom weight score: expected {}, got {}",
            expected,
            score.score
        );
    }

    #[test]
    fn test_zero_weights() {
        let config = claudear_config::config::PrioritisationConfig {
            severity_weight: 0.0,
            frequency_weight: 0.0,
            regression_weight: 0.0,
            blast_radius_weight: 0.0,
            cluster_weight: 0.0,
            ..Default::default()
        };
        let mut issue = make_issue(IssuePriority::Critical);
        issue.set_metadata("event_count", 10_000i64);
        issue.set_metadata("is_unhandled", true);
        issue.set_metadata("level", "fatal");
        let mr = make_match(MatchPriority::Urgent);
        let score = compute(&issue, &mr, BlastRadius::Critical, true, &config);

        assert!(
            score.score.abs() < 1e-10,
            "all zero weights must produce score 0.0, got {}",
            score.score
        );
        // Components themselves should still be computed
        assert!(
            score.severity_component > 0.0,
            "severity_component should still be non-zero"
        );
    }

    #[test]
    fn test_blast_radius_all_scores() {
        use crate::prioritisation::blast_radius::blast_radius_score;

        let cases: Vec<(BlastRadius, f64)> = vec![
            (BlastRadius::Critical, 1.0),
            (BlastRadius::Infrastructure, 0.8),
            (BlastRadius::Core, 0.6),
            (BlastRadius::Peripheral, 0.4),
            (BlastRadius::Test, 0.2),
            (BlastRadius::Cosmetic, 0.1),
        ];

        for (br, expected) in cases {
            let actual = blast_radius_score(br);
            assert!(
                (actual - expected).abs() < 1e-10,
                "blast_radius_score({:?}): expected {}, got {}",
                br,
                expected,
                actual
            );
        }
    }
}
