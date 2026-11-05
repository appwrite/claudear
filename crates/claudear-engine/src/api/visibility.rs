//! Keeps security-fix work out of the generic dashboard endpoints for
//! everyone but admins. Every endpoint decides what a user sees through the
//! [`AttemptScope`] here: aggregates pass it to the store, and rows a store
//! cannot scope are filtered with the same scope.

use super::auth::AuthUser;
use claudear_core::types::ProcessingMetric;
use claudear_storage::AttemptScope;

#[derive(Debug, Clone, Copy)]
pub(super) struct Visibility {
    scope: AttemptScope,
}

impl Visibility {
    pub(super) fn for_user(user: &AuthUser) -> Self {
        let scope = if user.is_admin() {
            AttemptScope::All
        } else {
            AttemptScope::WithoutSecurity
        };
        Self { scope }
    }

    pub(super) fn scope(self) -> AttemptScope {
        self.scope
    }

    pub(super) fn shows(self, source: &str) -> bool {
        self.scope.includes(source)
    }

    pub(super) fn shows_optional(self, source: Option<&str>) -> bool {
        self.scope.includes_optional(source)
    }

    pub(super) fn shows_all<'a>(self, sources: impl Iterator<Item = &'a str>) -> bool {
        self.scope.includes_all(sources)
    }

    /// Whether a listing filtered to `source` is hidden as a whole.
    pub(super) fn hides_filter(self, source: Option<&str>) -> bool {
        !self.shows_optional(source)
    }

    /// Whether raw per-table row counts, which no scope can narrow, are shown.
    pub(super) fn shows_diagnostics(self) -> bool {
        self.scope == AttemptScope::All
    }

    pub(super) fn shows_metric(self, metric: &ProcessingMetric) -> bool {
        self.scope.includes_metric(metric.source.as_deref())
    }

    pub(super) fn visible_metrics(
        self,
        mut metrics: Vec<ProcessingMetric>,
    ) -> Vec<ProcessingMetric> {
        metrics.retain(|metric| self.shows_metric(metric));
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
        if self.scope == AttemptScope::All {
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
    use claudear_core::security::SECURITY_FIX_SOURCE;
    use claudear_core::types::{
        ActivityLogEntry, AgentExecution, ErrorPattern, Issue, IssueEmbedding, PrRecord,
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
        assert!(!viewer.shows_all(["linear", SECURITY_FIX_SOURCE].into_iter()));
    }

    #[test]
    fn only_admins_read_every_attempt_and_the_diagnostics() {
        let admin = Visibility::for_user(&user("admin"));
        assert_eq!(admin.scope(), AttemptScope::All);
        assert!(admin.shows_diagnostics());
        let viewer = Visibility::for_user(&user("viewer"));
        assert_eq!(viewer.scope(), AttemptScope::WithoutSecurity);
        assert!(!viewer.shows_diagnostics());
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
        assert_eq!(values(Visibility::for_user(&user("viewer"))), [2.0]);
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

    const AGGREGATES: [&str; 14] = [
        "/api/activity",
        "/api/stats",
        "/api/stats/overview",
        "/api/attempts",
        "/api/issues",
        "/api/analytics/summary",
        "/api/prs/analytics",
        "/api/metrics?name=processing_time",
        "/api/metrics?name=pr_status_merged",
        "/api/telemetry/overview",
        "/api/telemetry/timeseries",
        "/api/telemetry/pipeline",
        "/api/telemetry/latency",
        "/api/telemetry/pipeline?period=day",
    ];

    fn seed_merged_fix(tracker: &dyn FixAttemptTracker, source: &str, number: i64) {
        let issue_id = format!("{source}-{number}");
        let pr_url = format!("https://github.com/org/{source}/pull/{number}");
        tracker
            .record_attempt(source, &issue_id, &issue_id)
            .expect("attempt");
        tracker
            .mark_success(source, &issue_id, &pr_url)
            .expect("success");
        tracker.mark_merged(source, &issue_id).expect("merged");
        let attempt = tracker
            .get_attempt(source, &issue_id)
            .expect("read")
            .expect("attempt exists");

        let mut execution = AgentExecution::new();
        execution.attempt_id = Some(attempt.id);
        execution.duration_secs = Some(90.0);
        execution.total_cost_usd = Some(2.5);
        execution.git_commit_before = Some("before".to_string());
        execution.git_commit_after = Some("after".to_string());
        tracker.record_execution(&execution).expect("execution");

        let mut record = pull_request(number, source);
        record.pr_url = pr_url;
        record.scm_repo = format!("org/{source}");
        record.attempt_id = Some(attempt.id);
        record.issue_id = Some(issue_id.clone());
        record.status = "merged".to_string();
        record.time_to_first_review_mins = Some(10);
        record.time_to_merge_mins = Some(45);
        record.review_cycles = 2;
        tracker.upsert_pr(&record).expect("pull request");

        tracker
            .record_activity(
                &ActivityLogEntry::new("pr_merged", format!("Merged {issue_id}"))
                    .with_source(source)
                    .with_issue(&issue_id, &issue_id),
            )
            .expect("activity");
        for (name, value) in [
            ("processing_time", 75.0),
            ("issues_fetched", 4.0),
            ("batch_processed", 1.0),
            ("pr_created", 1.0),
        ] {
            tracker
                .record_metric(
                    &ProcessingMetric::new(name, value)
                        .with_source(source)
                        .with_tags(serde_json::json!({ "status": "merged" })),
                )
                .expect("metric");
        }
        tracker
            .record_error_pattern(&error_pattern(&format!("{source}-hash"), source))
            .expect("error pattern");
        tracker
            .store_issue(&issue(source, &issue_id))
            .expect("issue");
    }

    fn seed_security_fix(tracker: &dyn FixAttemptTracker) {
        seed_merged_fix(tracker, SECURITY_FIX_SOURCE, 7);
        let issue = Issue::new("security-7", "SEC-7", "Hardening", "", SECURITY_FIX_SOURCE);
        tracker
            .record_activity(
                &ActivityLogEntry::new("claude_started", "Claude execution started for SEC-7")
                    .attributed("claude", Some(&issue))
                    .with_metadata(serde_json::json!({
                        "component": "claude",
                        "working_dir": "/worktrees/security-7",
                        "label": "SEC-7",
                    })),
            )
            .expect("runner activity");
        for (name, value) in [
            ("pr_status_merged", 1.0),
            ("pending_attempts", 9.0),
            ("total_attempts", 9.0),
        ] {
            tracker
                .record_metric(&ProcessingMetric::new(name, value))
                .expect("unattributed metric");
        }
    }

    fn without_volatile_fields(mut value: Value) -> Value {
        match &mut value {
            Value::Object(fields) => {
                fields.remove("generated_at");
                fields.remove("uptime_secs");
                if let Some(Value::Array(points)) = fields.get_mut("points") {
                    points.retain(|point| point["total"].as_i64() != Some(0));
                }
                for field in fields.values_mut() {
                    *field = without_volatile_fields(field.take());
                }
            }
            Value::Array(items) => {
                for item in items.iter_mut() {
                    *item = without_volatile_fields(item.take());
                }
            }
            _ => {}
        }
        value
    }

    async fn aggregates(harness: &Harness, session: &str) -> Vec<Value> {
        let mut bodies = Vec::new();
        for uri in AGGREGATES {
            let (status, body) = harness.get(uri, Some(session)).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            bodies.push(without_volatile_fields(body));
        }
        bodies
    }

    #[tokio::test]
    async fn a_security_fix_changes_no_non_admin_aggregate() {
        let harness = Harness::start();
        seed_merged_fix(harness.tracker.as_ref(), OTHER_SOURCE, 1);
        let viewer_before = aggregates(&harness, &harness.viewer).await;
        let admin_before = aggregates(&harness, &harness.admin).await;

        seed_security_fix(harness.tracker.as_ref());
        let viewer_after = aggregates(&harness, &harness.viewer).await;
        let admin_after = aggregates(&harness, &harness.admin).await;

        for (index, uri) in AGGREGATES.iter().enumerate() {
            assert_eq!(
                viewer_before[index], viewer_after[index],
                "{uri} changed for a non-admin when a security fix landed"
            );
            assert_ne!(
                admin_before[index], admin_after[index],
                "{uri} left the security fix out for an admin"
            );
        }
    }

    #[tokio::test]
    async fn non_admin_pull_requests_leave_out_those_of_security_attempts() {
        let harness = Harness::start();
        let seeded = seed(harness.tracker.as_ref());
        let mut record = pull_request(3, OTHER_SOURCE);
        record.attempt_id = Some(seeded.security_attempt);
        harness.tracker.upsert_pr(&record).expect("pull request");

        let urls = |body: &Value| -> Vec<String> {
            let mut urls: Vec<String> = body
                .as_array()
                .expect("pull requests")
                .iter()
                .filter_map(|row| row["pr_url"].as_str().map(str::to_string))
                .collect();
            urls.sort();
            urls
        };
        let (_, viewer) = harness.get("/api/prs", Some(&harness.viewer)).await;
        assert_eq!(urls(&viewer), [pull_request(2, OTHER_SOURCE).pr_url]);
        let (_, admin) = harness.get("/api/prs", Some(&harness.admin)).await;
        assert_eq!(urls(&admin).len(), 3);
    }

    #[tokio::test]
    async fn non_admin_aggregates_still_count_other_work() {
        let harness = Harness::start();
        seed_merged_fix(harness.tracker.as_ref(), OTHER_SOURCE, 1);
        seed_security_fix(harness.tracker.as_ref());

        let (_, viewer) = harness
            .get("/api/analytics/summary", Some(&harness.viewer))
            .await;
        let (_, admin) = harness
            .get("/api/analytics/summary", Some(&harness.admin))
            .await;
        assert_eq!(viewer["total_processed"], 1);
        assert_eq!(admin["total_processed"], 2);
        assert_eq!(viewer["cost_estimate"]["total_cost"], 2.5);
        assert_eq!(admin["cost_estimate"]["total_cost"], 5.0);
        assert_eq!(viewer["repo_leaderboard"].as_array().map(Vec::len), Some(1));
        assert_eq!(viewer["mttr_trend"][0]["sample_count"], 1);
        assert_eq!(admin["mttr_trend"][0]["sample_count"], 2);
        assert_eq!(viewer["commit_trend"][0]["commits"], 1);
        assert_eq!(admin["commit_trend"][0]["commits"], 2);

        let (_, viewer) = harness
            .get("/api/prs/analytics", Some(&harness.viewer))
            .await;
        let (_, admin) = harness
            .get("/api/prs/analytics", Some(&harness.admin))
            .await;
        assert_eq!(viewer["total"], 1);
        assert_eq!(viewer["merged"], 1);
        assert_eq!(admin["total"], 2);
        assert_eq!(
            viewer["by_repo"].as_object().map(|repos| repos.len()),
            Some(1)
        );

        let (_, viewer) = harness
            .get("/api/telemetry/overview", Some(&harness.viewer))
            .await;
        let (_, admin) = harness
            .get("/api/telemetry/overview", Some(&harness.admin))
            .await;
        assert_eq!(viewer["agent_spawns_today"], 1);
        assert_eq!(admin["agent_spawns_today"], 2);
        assert_eq!(viewer["pr_analytics"]["total"], 1);
        assert_eq!(viewer["activity_last_hour"]["pr_merged"], 1);
        assert_eq!(admin["activity_last_hour"]["pr_merged"], 2);
        assert_eq!(viewer["metric_counts_last_24h"]["pr_status_merged"], 0);
        assert_eq!(admin["metric_counts_last_24h"]["pr_status_merged"], 1);
        assert!(viewer["diagnostics"].is_null());
        assert!(admin["diagnostics"].is_object());

        let (_, viewer) = harness
            .get("/api/telemetry/pipeline", Some(&harness.viewer))
            .await;
        let (_, admin) = harness
            .get("/api/telemetry/pipeline", Some(&harness.admin))
            .await;
        assert_eq!(viewer["totals"]["fetched"], 4.0);
        assert_eq!(admin["totals"]["fetched"], 8.0);
        assert_eq!(viewer["totals"]["pr_status_merged"], 0.0);
        assert_eq!(admin["totals"]["pr_status_merged"], 1.0);
        assert!(viewer["poll_load"]["total_latest"].is_null());
        assert_eq!(admin["poll_load"]["total_latest"], 9.0);

        let (_, viewer) = harness
            .get("/api/stats/overview", Some(&harness.viewer))
            .await;
        let (_, admin) = harness
            .get("/api/stats/overview", Some(&harness.admin))
            .await;
        assert_eq!(viewer["agent_spawns_today"], 1);
        assert_eq!(viewer["time_savings"]["merged_count"], 1);
        assert_eq!(admin["time_savings"]["merged_count"], 2);
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
