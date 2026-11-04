//! Keeps security-fix work out of the generic dashboard endpoints for
//! everyone but admins.

use super::auth::AuthUser;
use claudear_core::security::SECURITY_FIX_SOURCE;
use claudear_core::types::{FixAttemptStats, ProcessingMetric};

pub(super) fn hides_security(user: &AuthUser) -> bool {
    !user.is_admin()
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Visibility {
    hides_security: bool,
}

impl Visibility {
    pub(super) fn for_user(user: &AuthUser) -> Self {
        Self {
            hides_security: hides_security(user),
        }
    }

    pub(super) fn shows(self, source: &str) -> bool {
        !(self.hides_security && source.eq_ignore_ascii_case(SECURITY_FIX_SOURCE))
    }

    pub(super) fn shows_optional(self, source: Option<&str>) -> bool {
        source.is_none_or(|source| self.shows(source))
    }

    pub(super) fn shows_all<'a>(self, mut sources: impl Iterator<Item = &'a str>) -> bool {
        sources.all(|source| self.shows(source))
    }

    /// Whether a listing filtered to `source` is hidden as a whole.
    pub(super) fn hides_filter(self, source: Option<&str>) -> bool {
        !self.shows_optional(source)
    }

    /// Whether an unfiltered count includes rows of a hidden source that
    /// must be subtracted.
    pub(super) fn subtracts_hidden(self, source: Option<&str>) -> bool {
        self.hides_security && source.is_none()
    }

    pub(super) fn hidden_source(self) -> &'static str {
        SECURITY_FIX_SOURCE
    }

    /// Removes the hidden source's bucket from attempt statistics, and its
    /// attempts from the totals.
    pub(super) fn scrub_stats(self, stats: &mut FixAttemptStats) {
        if !self.hides_security {
            return;
        }
        let Some(hidden) = stats.by_source.remove(SECURITY_FIX_SOURCE) else {
            return;
        };
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

    /// The source an unfiltered listing must leave out in the store, so a
    /// page costs the same at any offset.
    pub(super) fn excluded_source(self, source: Option<&str>) -> Option<&'static str> {
        self.subtracts_hidden(source).then_some(SECURITY_FIX_SOURCE)
    }

    pub(super) fn visible_metrics(
        self,
        mut metrics: Vec<ProcessingMetric>,
    ) -> Vec<ProcessingMetric> {
        metrics.retain(|metric| self.shows_optional(metric.source.as_deref()));
        metrics
    }

    /// The first `limit` visible rows of a newest-first listing that only
    /// takes a limit, widening the fetch until enough visible rows remain.
    pub(super) fn prefix<T, E>(
        self,
        mut fetch: impl FnMut(usize) -> Result<Vec<T>, E>,
        visible: impl Fn(&T) -> bool,
        limit: usize,
    ) -> Result<Vec<T>, E> {
        if !self.hides_security {
            return fetch(limit);
        }
        let mut requested = limit.max(1);
        loop {
            let rows = fetch(requested)?;
            let exhausted = rows.len() < requested;
            let shown: Vec<T> = rows.into_iter().filter(|row| visible(row)).collect();
            if exhausted || shown.len() >= limit {
                return Ok(shown.into_iter().take(limit).collect());
            }
            requested = requested.saturating_mul(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::Harness;
    use super::*;
    use axum::http::StatusCode;
    use claudear_core::types::{
        ActivityLogEntry, AgentExecution, ErrorPattern, IssueEmbedding, PrRecord,
    };
    use claudear_storage::FixAttemptTracker;
    use serde_json::Value;
    use std::convert::Infallible;

    fn user(role: &str) -> AuthUser {
        AuthUser {
            id: 1,
            email: "person@example.com".to_string(),
            name: "Person".to_string(),
            role: role.to_string(),
            avatar_url: None,
        }
    }

    fn rows(count: usize, hidden_every: usize) -> Vec<(usize, &'static str)> {
        (0..count)
            .map(|index| {
                let source = if index % hidden_every == 0 {
                    SECURITY_FIX_SOURCE
                } else {
                    "linear"
                };
                (index, source)
            })
            .collect()
    }

    #[test]
    fn only_admins_see_security_sources() {
        assert!(Visibility::for_user(&user("admin")).shows(SECURITY_FIX_SOURCE));
        let viewer = Visibility::for_user(&user("viewer"));
        assert!(!viewer.shows(SECURITY_FIX_SOURCE));
        assert!(!viewer.shows("Security"));
        assert!(viewer.shows("linear"));
        assert!(viewer.shows_optional(None));
        assert!(viewer.hides_filter(Some(SECURITY_FIX_SOURCE)));
        assert!(!viewer.hides_filter(None));
        assert!(viewer.subtracts_hidden(None));
        assert!(!viewer.subtracts_hidden(Some("linear")));
        assert!(!viewer.shows_all(["linear", SECURITY_FIX_SOURCE].into_iter()));
    }

    #[test]
    fn only_unfiltered_non_admin_listings_exclude_the_hidden_source() {
        let viewer = Visibility::for_user(&user("viewer"));
        assert_eq!(viewer.excluded_source(None), Some(SECURITY_FIX_SOURCE));
        assert_eq!(viewer.excluded_source(Some("linear")), None);
        let admin = Visibility::for_user(&user("admin"));
        assert_eq!(admin.excluded_source(None), None);
    }

    #[test]
    fn non_admins_see_only_metrics_outside_the_hidden_source() {
        let metrics = vec![
            ProcessingMetric::new("processing_time", 1.0)
                .with_source(SECURITY_FIX_SOURCE.to_string()),
            ProcessingMetric::new("processing_time", 2.0).with_source("linear".to_string()),
            ProcessingMetric::new("processing_time", 3.0),
        ];
        let values = |visibility: Visibility| -> Vec<f64> {
            visibility
                .visible_metrics(metrics.clone())
                .into_iter()
                .map(|metric| metric.metric_value)
                .collect()
        };
        assert_eq!(values(Visibility::for_user(&user("viewer"))), [2.0, 3.0]);
        assert_eq!(
            values(Visibility::for_user(&user("admin"))),
            [1.0, 2.0, 3.0]
        );
    }

    #[test]
    fn prefix_widens_until_enough_rows_are_visible() {
        let all = rows(200, 1);
        let mut mixed = all.clone();
        mixed.extend(
            rows(5, 1000)
                .into_iter()
                .map(|(index, source)| (index + 200, source)),
        );
        let viewer = Visibility::for_user(&user("viewer"));
        let mut requests = Vec::new();
        let shown = viewer
            .prefix(
                |limit| {
                    requests.push(limit);
                    Ok::<_, Infallible>(mixed.iter().take(limit).copied().collect::<Vec<_>>())
                },
                |row| viewer.shows(row.1),
                3,
            )
            .unwrap_or_else(|never| match never {});
        assert_eq!(
            shown.iter().map(|row| row.0).collect::<Vec<_>>(),
            [201, 202, 203]
        );
        assert_eq!(requests.first(), Some(&3));
        assert!(requests.last().is_some_and(|&last| last > mixed.len()));
    }

    const OTHER_SOURCE: &str = "linear";

    struct Seeded {
        security_attempt: i64,
        other_attempt: i64,
        security_execution: i64,
    }

    fn pull_request(number: i64, source: &str) -> PrRecord {
        PrRecord {
            id: 0,
            pr_url: format!("https://github.com/org/repo/pull/{number}"),
            scm_repo: "org/repo".to_string(),
            pr_number: number,
            attempt_id: None,
            issue_id: Some(format!("{source}-{number}")),
            issue_source: Some(source.to_string()),
            title: Some(format!("fix: change {number}")),
            description: None,
            author: None,
            head_branch: None,
            base_branch: None,
            status: "open".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: None,
            merged_at: None,
            closed_at: None,
            approvals_count: 0,
            changes_requested_count: 0,
            comments_count: 0,
            last_review_at: None,
            time_to_first_review_mins: None,
            time_to_merge_mins: None,
            review_cycles: 0,
            files_changed: None,
            lines_added: None,
            lines_removed: None,
        }
    }

    fn issue(source: &str, issue_id: &str) -> IssueEmbedding {
        IssueEmbedding {
            id: 0,
            source: source.to_string(),
            issue_id: issue_id.to_string(),
            short_id: Some(issue_id.to_string()),
            title: Some(format!("Issue {issue_id}")),
            description: None,
            url: None,
            priority: None,
            status: Some("open".to_string()),
            labels: None,
            embedding: None,
            embedding_model: None,
            created_at: chrono::Utc::now(),
            updated_at: None,
        }
    }

    fn error_pattern(hash: &str, source: &str) -> ErrorPattern {
        ErrorPattern {
            id: 0,
            pattern_hash: hash.to_string(),
            error_type: Some("test_failure".to_string()),
            error_message: Some(format!("{source} failure")),
            first_seen: chrono::Utc::now(),
            last_seen: chrono::Utc::now(),
            occurrence_count: 1,
            sources: Some(vec![source.to_string()]),
            example_issue_ids: None,
            resolution_hints: None,
        }
    }

    fn feedback(
        attempt_id: i64,
        source: &str,
        issue_id: &str,
    ) -> claudear_analysis::feedback::FixOutcome {
        claudear_analysis::feedback::FixOutcome {
            id: 0,
            attempt_id,
            source: source.to_string(),
            issue_id: issue_id.to_string(),
            issue_text: format!("{source} issue"),
            prompt_used: "prompt".to_string(),
            outcome: claudear_analysis::feedback::Outcome::Failed,
            error_type: None,
            learnings: None,
            keywords: Vec::new(),
            embedding: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn seed(tracker: &dyn FixAttemptTracker) -> Seeded {
        let attempt_id = |source: &str, issue_id: &str| {
            tracker
                .record_attempt(source, issue_id, issue_id)
                .expect("attempt");
            tracker
                .mark_failed(source, issue_id, "boom")
                .expect("failed");
            tracker
                .get_attempt(source, issue_id)
                .expect("read")
                .expect("attempt exists")
                .id
        };
        let security_attempt = attempt_id(SECURITY_FIX_SOURCE, "SEC-1");
        let other_attempt = attempt_id(OTHER_SOURCE, "PROJ-1");

        let mut execution = AgentExecution::new();
        execution.attempt_id = Some(security_attempt);
        execution.stdout_preview = Some("security fix output".to_string());
        let security_execution = tracker.record_execution(&execution).expect("execution");

        for (source, issue_id) in [(SECURITY_FIX_SOURCE, "SEC-1"), (OTHER_SOURCE, "PROJ-1")] {
            tracker
                .record_activity(
                    &ActivityLogEntry::new("processing_started", format!("Started {issue_id}"))
                        .with_source(source)
                        .with_issue(issue_id, issue_id),
                )
                .expect("activity");
            tracker
                .store_issue(&issue(source, issue_id))
                .expect("issue");
        }
        tracker
            .upsert_pr(&pull_request(1, SECURITY_FIX_SOURCE))
            .expect("pull request");
        tracker
            .upsert_pr(&pull_request(2, OTHER_SOURCE))
            .expect("pull request");
        tracker
            .record_error_pattern(&error_pattern("security-hash", SECURITY_FIX_SOURCE))
            .expect("error pattern");
        tracker
            .record_error_pattern(&error_pattern("other-hash", OTHER_SOURCE))
            .expect("error pattern");
        tracker
            .store_feedback_outcome(&feedback(security_attempt, SECURITY_FIX_SOURCE, "SEC-1"))
            .expect("feedback");
        tracker
            .store_feedback_outcome(&feedback(other_attempt, OTHER_SOURCE, "PROJ-1"))
            .expect("feedback");

        Seeded {
            security_attempt,
            other_attempt,
            security_execution,
        }
    }

    fn mentions_security(value: &Value) -> bool {
        value.to_string().contains("SEC-1")
            || value
                .to_string()
                .contains(&format!("\"{SECURITY_FIX_SOURCE}\""))
    }

    async fn listings(harness: &Harness, session: &str) -> Vec<(&'static str, Value)> {
        let mut listings = Vec::new();
        for uri in [
            "/api/attempts",
            "/api/stats",
            "/api/stats/overview",
            "/api/retries",
            "/api/activity",
            "/api/issues",
            "/api/prs",
            "/api/feedback",
            "/api/errors",
        ] {
            let (status, body) = harness.get(uri, Some(session)).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            listings.push((uri, body));
        }
        listings
    }

    #[tokio::test]
    async fn non_admin_listings_contain_no_security_work() {
        let harness = Harness::start();
        seed(harness.tracker.as_ref());

        for (uri, body) in listings(&harness, &harness.viewer).await {
            assert!(!mentions_security(&body), "{uri} leaked: {body}");
            assert!(
                body.to_string().contains(OTHER_SOURCE),
                "{uri} lost other work: {body}"
            );
        }
        for (uri, body) in listings(&harness, &harness.admin).await {
            assert!(
                mentions_security(&body),
                "{uri} hid security work from an admin: {body}"
            );
        }

        let (_, attempts) = harness.get("/api/attempts", Some(&harness.viewer)).await;
        assert_eq!(attempts["total"], 1);
        let (_, attempts) = harness.get("/api/attempts", Some(&harness.admin)).await;
        assert_eq!(attempts["total"], 2);
        let (_, issues) = harness.get("/api/issues", Some(&harness.viewer)).await;
        assert_eq!(issues["total"], 1);
        let (_, stats) = harness.get("/api/stats", Some(&harness.viewer)).await;
        assert_eq!(stats["total"], 1);
        assert_eq!(stats["failed"], 1);
        let (_, stats) = harness.get("/api/stats", Some(&harness.admin)).await;
        assert_eq!(stats["total"], 2);
    }

    fn seed_metrics(tracker: &dyn FixAttemptTracker) {
        for (source, seconds, fetched) in
            [(SECURITY_FIX_SOURCE, 40.0, 5.0), (OTHER_SOURCE, 20.0, 3.0)]
        {
            tracker
                .record_metric(
                    &ProcessingMetric::new("processing_time", seconds)
                        .with_source(source)
                        .with_tags(serde_json::json!({ "status": "failed" })),
                )
                .expect("processing metric");
            tracker
                .record_metric(
                    &ProcessingMetric::new("issues_fetched", fetched).with_source(source),
                )
                .expect("pipeline metric");
        }
    }

    fn sources_in(rows: &Value) -> Vec<String> {
        rows.as_array()
            .expect("rows")
            .iter()
            .filter_map(|row| row["source"].as_str().map(str::to_string))
            .collect()
    }

    fn timeseries_total(body: &Value) -> i64 {
        body["points"]
            .as_array()
            .expect("points")
            .iter()
            .filter_map(|point| point["total"].as_i64())
            .sum()
    }

    #[tokio::test]
    async fn non_admin_aggregates_count_no_security_work() {
        let harness = Harness::start();
        seed(harness.tracker.as_ref());
        seed_metrics(harness.tracker.as_ref());

        let (_, viewer) = harness
            .get("/api/telemetry/overview", Some(&harness.viewer))
            .await;
        let (_, admin) = harness
            .get("/api/telemetry/overview", Some(&harness.admin))
            .await;
        assert_eq!(sources_in(&viewer["source_breakdown"]), [OTHER_SOURCE]);
        assert!(sources_in(&admin["source_breakdown"]).contains(&SECURITY_FIX_SOURCE.to_string()));
        assert_eq!(viewer["windows"][2]["processed"], 1);
        assert_eq!(admin["windows"][2]["processed"], 2);
        assert_eq!(viewer["processing_time"]["all_time"]["samples"], 1);
        assert_eq!(admin["processing_time"]["all_time"]["samples"], 2);
        assert!(!mentions_security(&viewer["top_errors"]), "{viewer}");
        assert!(mentions_security(&admin["top_errors"]));
        assert_eq!(viewer["queue"]["retryable_attempts"], 1);

        let (_, viewer) = harness
            .get("/api/telemetry/pipeline", Some(&harness.viewer))
            .await;
        let (_, admin) = harness
            .get("/api/telemetry/pipeline", Some(&harness.admin))
            .await;
        assert_eq!(sources_in(&viewer["per_source"]), [OTHER_SOURCE]);
        assert_eq!(admin["per_source"].as_array().map(Vec::len), Some(2));

        let (_, viewer) = harness
            .get("/api/telemetry/latency", Some(&harness.viewer))
            .await;
        let (_, admin) = harness
            .get("/api/telemetry/latency", Some(&harness.admin))
            .await;
        assert_eq!(viewer["overall"]["samples"], 1);
        assert_eq!(admin["overall"]["samples"], 2);

        let (_, viewer) = harness
            .get("/api/telemetry/timeseries", Some(&harness.viewer))
            .await;
        let (_, admin) = harness
            .get("/api/telemetry/timeseries", Some(&harness.admin))
            .await;
        assert_eq!(timeseries_total(&viewer), 1);
        assert_eq!(timeseries_total(&admin), 2);

        let uri = "/api/metrics?name=processing_time";
        let (_, viewer) = harness.get(uri, Some(&harness.viewer)).await;
        let (_, admin) = harness.get(uri, Some(&harness.admin)).await;
        assert_eq!(sources_in(&viewer), [OTHER_SOURCE]);
        assert_eq!(admin.as_array().map(Vec::len), Some(2));

        let (_, viewer) = harness
            .get("/api/analytics/summary", Some(&harness.viewer))
            .await;
        let (_, admin) = harness
            .get("/api/analytics/summary", Some(&harness.admin))
            .await;
        assert!(viewer["success_rate_by_source"]
            .get(SECURITY_FIX_SOURCE)
            .is_none());
        assert!(admin["success_rate_by_source"]
            .get(SECURITY_FIX_SOURCE)
            .is_some());
    }

    #[tokio::test]
    async fn non_admin_source_filters_for_security_come_back_empty() {
        let harness = Harness::start();
        seed(harness.tracker.as_ref());

        for uri in [
            "/api/attempts?source=security",
            "/api/attempts?source=SECURITY",
            "/api/activity?source=security",
            "/api/issues?source=security",
            "/api/feedback?source=security",
        ] {
            let (status, body) = harness.get(uri, Some(&harness.viewer)).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert!(!mentions_security(&body), "{uri} leaked: {body}");
            let (_, admin_body) = harness.get(uri, Some(&harness.admin)).await;
            assert!(
                mentions_security(&admin_body),
                "{uri} hid security work from an admin"
            );
        }
    }

    #[tokio::test]
    async fn non_admin_security_attempt_details_are_not_found() {
        let harness = Harness::start();
        let seeded = seed(harness.tracker.as_ref());
        let security = seeded.security_attempt;
        let hidden = [
            format!("/api/attempts/{security}"),
            format!("/api/attempts/{security}/detail"),
            format!("/api/attempts/{security}/retrieval"),
            format!(
                "/api/attempts/{security}/logs/{}/stdout",
                seeded.security_execution
            ),
            "/api/issues/security/SEC-1/timeline".to_string(),
        ];
        for uri in &hidden {
            assert_eq!(
                harness.get(uri, Some(&harness.viewer)).await.0,
                StatusCode::NOT_FOUND,
                "{uri}"
            );
            assert_eq!(
                harness.get(uri, Some(&harness.admin)).await.0,
                StatusCode::OK,
                "{uri}"
            );
        }

        let other = seeded.other_attempt;
        for uri in [
            format!("/api/attempts/{other}"),
            format!("/api/attempts/{other}/detail"),
            format!("/api/attempts/{other}/retrieval"),
            "/api/issues/linear/PROJ-1/timeline".to_string(),
        ] {
            assert_eq!(
                harness.get(&uri, Some(&harness.viewer)).await.0,
                StatusCode::OK,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn non_admin_issue_pages_skip_security_issues_without_losing_rows() {
        let harness = Harness::start();
        for index in 0..30 {
            let source = if index % 2 == 0 {
                SECURITY_FIX_SOURCE
            } else {
                OTHER_SOURCE
            };
            harness
                .tracker
                .store_issue(&issue(source, &format!("ISSUE-{index}")))
                .expect("issue");
        }

        let mut seen = Vec::new();
        for page in 1..=3 {
            let (_, body) = harness
                .get(
                    &format!("/api/issues?per_page=5&page={page}"),
                    Some(&harness.viewer),
                )
                .await;
            assert_eq!(body["total"], 15);
            let issues = body["issues"].as_array().expect("issues").clone();
            assert_eq!(issues.len(), 5, "page {page} is full");
            assert!(issues.iter().all(|issue| issue["source"] == OTHER_SOURCE));
            seen.extend(issues.into_iter().map(|issue| issue["issue_id"].clone()));
        }
        seen.sort_by_key(Value::to_string);
        seen.dedup();
        assert_eq!(seen.len(), 15);
    }

    #[tokio::test]
    async fn non_admin_pages_skip_security_attempts_without_losing_rows() {
        let harness = Harness::start();
        for index in 0..30 {
            let source = if index % 2 == 0 {
                SECURITY_FIX_SOURCE
            } else {
                OTHER_SOURCE
            };
            harness
                .tracker
                .record_attempt(source, &format!("issue-{index}"), &format!("ISSUE-{index}"))
                .expect("attempt");
        }

        let mut seen = Vec::new();
        for page in 1..=3 {
            let (_, body) = harness
                .get(
                    &format!("/api/attempts?per_page=5&page={page}"),
                    Some(&harness.viewer),
                )
                .await;
            assert_eq!(body["total"], 15);
            let attempts = body["attempts"].as_array().expect("attempts").clone();
            assert_eq!(attempts.len(), 5, "page {page} is full");
            seen.extend(
                attempts
                    .into_iter()
                    .map(|attempt| attempt["short_id"].clone()),
            );
        }
        seen.sort_by_key(Value::to_string);
        seen.dedup();
        assert_eq!(seen.len(), 15);
    }
}
