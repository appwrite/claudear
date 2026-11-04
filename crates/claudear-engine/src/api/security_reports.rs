//! Admin-only security inbox endpoints: report triage, overrides, fix links,
//! go-live confirmation and awards.

use super::auth::{AdminUser, AuthUser};
use super::routes::ApiState;
use crate::security::SecurityInbox;
use axum::{
    extract::{rejection::JsonRejection, Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use claudear_core::error::Error;
use claudear_core::security::{
    AwardStatus, NewSecurityEvent, SecurityAward, SecurityEvent, SecurityEventKind,
    SecurityOutcome, SecurityOverride, SecurityReply, SecurityReport, SecurityReportChanges,
    SecurityReportFilter, SecurityReportState, SecuritySeverity, SecurityVulnerability,
    SecurityVulnerabilityChanges, VulnerabilityState,
};
use claudear_storage::FixAttemptTracker;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

const DEFAULT_REPORT_LIMIT: usize = 100;
const MAX_REPORT_LIMIT: usize = 500;
const MAX_PUBLIC_REASON_CHARS: usize = 1000;
const MAX_AWARD_NOTE_CHARS: usize = 1000;
const MAX_DEPLOYED_VERSION_CHARS: usize = 100;
const LINKABLE_STATES: [VulnerabilityState; 2] = [
    VulnerabilityState::AwaitingFix,
    VulnerabilityState::NeedsHuman,
];
const LIVE_SOURCE_STATES: [VulnerabilityState; 2] =
    [VulnerabilityState::Merged, VulnerabilityState::NeedsHuman];
const AWARDABLE_STATUSES: [AwardStatus; 2] = [AwardStatus::Prepared, AwardStatus::OwnerNotified];

type ApiResult<T> = Result<Json<T>, StatusCode>;

#[derive(Debug, Serialize)]
pub struct SecuritySummary {
    pub reports_by_state: HashMap<SecurityReportState, usize>,
    pub reports_by_outcome: HashMap<SecurityOutcome, usize>,
    pub vulnerabilities_by_state: HashMap<VulnerabilityState, usize>,
    pub pending_awards: usize,
    pub unconfirmed_replies: usize,
}

#[derive(Debug, Serialize)]
pub struct SecurityReportRow {
    pub id: i64,
    pub short_id: String,
    pub url: String,
    pub subject: String,
    pub reporter_email: Option<String>,
    pub reported_at: DateTime<Utc>,
    pub state: SecurityReportState,
    pub outcome: Option<SecurityOutcome>,
    pub vulnerability_id: Option<i64>,
    pub is_first_reporter: bool,
    pub vulnerability_state: Option<VulnerabilityState>,
    pub severity: Option<SecuritySeverity>,
    pub pull_request_url: Option<String>,
    pub deployed_version: Option<String>,
    pub live_at: Option<DateTime<Utc>>,
    pub award_status: Option<AwardStatus>,
    pub unconfirmed_reply: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct SecurityReportDetail {
    pub report: SecurityReport,
    pub vulnerability: Option<SecurityVulnerability>,
    pub events: Vec<SecurityEvent>,
    pub replies: Vec<SecurityReply>,
    pub award: Option<SecurityAward>,
    pub related_reports: Vec<SecurityReportRow>,
}

#[derive(Debug, Deserialize)]
pub struct ReportsQuery {
    pub state: Option<SecurityReportState>,
    pub outcome: Option<SecurityOutcome>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct AwardRequest {
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PullRequestLink {
    pub pull_request_url: String,
}

#[derive(Debug, Deserialize)]
pub struct LiveConfirmation {
    pub deployed_version: String,
}

fn internal_error(error: Error) -> StatusCode {
    tracing::error!(error = %error, "Security inbox request failed");
    sentry::capture_error(&error);
    StatusCode::INTERNAL_SERVER_ERROR
}

fn count_by<T, K: Eq + Hash>(
    items: impl IntoIterator<Item = T>,
    key: impl Fn(T) -> Option<K>,
) -> HashMap<K, usize> {
    let mut counts = HashMap::new();
    for name in items.into_iter().filter_map(key) {
        *counts.entry(name).or_insert(0) += 1;
    }
    counts
}

fn report_row(
    report: SecurityReport,
    vulnerability: Option<&SecurityVulnerability>,
    award: Option<&SecurityAward>,
    unconfirmed_reply: bool,
) -> SecurityReportRow {
    let is_first_reporter =
        vulnerability.is_some_and(|vulnerability| vulnerability.first_report_id == report.id);
    let award_status = award
        .filter(|award| award.report_id == report.id)
        .map(|award| award.status);
    SecurityReportRow {
        id: report.id,
        short_id: report.short_id,
        url: report.url,
        subject: report.subject,
        reporter_email: report.reporter_email,
        reported_at: report.reported_at,
        state: report.state,
        outcome: report.outcome,
        vulnerability_id: report.vulnerability_id,
        is_first_reporter,
        vulnerability_state: vulnerability.map(|vulnerability| vulnerability.state),
        severity: vulnerability.and_then(|vulnerability| vulnerability.severity),
        pull_request_url: vulnerability
            .and_then(|vulnerability| vulnerability.pull_request_url.clone()),
        deployed_version: vulnerability
            .and_then(|vulnerability| vulnerability.deployed_version.clone()),
        live_at: vulnerability.and_then(|vulnerability| vulnerability.live_at),
        award_status,
        unconfirmed_reply,
        updated_at: report.updated_at,
    }
}

fn report_rows(
    tracker: &dyn FixAttemptTracker,
    reports: Vec<SecurityReport>,
    vulnerabilities: &HashMap<i64, SecurityVulnerability>,
    awards: &HashMap<i64, SecurityAward>,
) -> Result<Vec<SecurityReportRow>, Error> {
    let unconfirmed: HashSet<i64> = tracker
        .list_reports_with_unconfirmed_replies()?
        .into_iter()
        .collect();
    Ok(reports
        .into_iter()
        .map(|report| {
            let unconfirmed_reply = unconfirmed.contains(&report.id);
            let vulnerability = report
                .vulnerability_id
                .and_then(|id| vulnerabilities.get(&id));
            let award = report.vulnerability_id.and_then(|id| awards.get(&id));
            report_row(report, vulnerability, award, unconfirmed_reply)
        })
        .collect())
}

fn vulnerabilities_by_id(
    tracker: &dyn FixAttemptTracker,
) -> Result<HashMap<i64, SecurityVulnerability>, Error> {
    Ok(tracker
        .list_security_vulnerabilities(None)?
        .into_iter()
        .map(|vulnerability| (vulnerability.id, vulnerability))
        .collect())
}

fn awards_by_vulnerability(
    tracker: &dyn FixAttemptTracker,
) -> Result<HashMap<i64, SecurityAward>, Error> {
    Ok(tracker
        .list_security_awards(None)?
        .into_iter()
        .map(|award| (award.vulnerability_id, award))
        .collect())
}

fn all_reports(tracker: &dyn FixAttemptTracker) -> Result<Vec<SecurityReport>, Error> {
    tracker.list_security_reports(&SecurityReportFilter {
        limit: Some(usize::MAX),
        ..SecurityReportFilter::default()
    })
}

/// `GET /api/security/summary`
pub async fn summary_handler(
    _admin: AdminUser,
    State(state): State<ApiState>,
) -> ApiResult<SecuritySummary> {
    let tracker = state.tracker.as_ref();
    let reports = all_reports(tracker).map_err(internal_error)?;
    let vulnerabilities = tracker
        .list_security_vulnerabilities(None)
        .map_err(internal_error)?;
    let awards = tracker.list_security_awards(None).map_err(internal_error)?;

    let unconfirmed_replies = tracker
        .list_reports_with_unconfirmed_replies()
        .map_err(internal_error)?
        .len();

    Ok(Json(SecuritySummary {
        reports_by_state: count_by(&reports, |report| Some(report.state)),
        reports_by_outcome: count_by(&reports, |report| report.outcome),
        vulnerabilities_by_state: count_by(&vulnerabilities, |vulnerability| {
            Some(vulnerability.state)
        }),
        pending_awards: awards
            .iter()
            .filter(|award| AWARDABLE_STATUSES.contains(&award.status))
            .count(),
        unconfirmed_replies,
    }))
}

/// `GET /api/security/reports?state=&outcome=&limit=`
pub async fn reports_handler(
    _admin: AdminUser,
    State(state): State<ApiState>,
    Query(query): Query<ReportsQuery>,
) -> ApiResult<Vec<SecurityReportRow>> {
    let tracker = state.tracker.as_ref();
    let filter = SecurityReportFilter {
        state: query.state,
        outcome: query.outcome,
        limit: Some(
            query
                .limit
                .unwrap_or(DEFAULT_REPORT_LIMIT)
                .clamp(1, MAX_REPORT_LIMIT),
        ),
    };
    let reports = tracker
        .list_security_reports(&filter)
        .map_err(internal_error)?;
    let vulnerabilities = vulnerabilities_by_id(tracker).map_err(internal_error)?;
    let awards = awards_by_vulnerability(tracker).map_err(internal_error)?;
    report_rows(tracker, reports, &vulnerabilities, &awards)
        .map(Json)
        .map_err(internal_error)
}

/// `GET /api/security/reports/{id}`
pub async fn report_handler(
    _admin: AdminUser,
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> ApiResult<SecurityReportDetail> {
    let tracker = state.tracker.as_ref();
    let report = tracker
        .get_security_report(id)
        .map_err(internal_error)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let vulnerability = match report.vulnerability_id {
        Some(vulnerability_id) => tracker
            .get_security_vulnerability(vulnerability_id)
            .map_err(internal_error)?,
        None => None,
    };
    let award = match &vulnerability {
        Some(vulnerability) => tracker
            .get_security_award(vulnerability.id)
            .map_err(internal_error)?,
        None => None,
    };
    let related_reports = match &vulnerability {
        Some(vulnerability) => {
            let reports = tracker
                .list_reports_for_vulnerability(vulnerability.id)
                .map_err(internal_error)?;
            let vulnerabilities = HashMap::from([(vulnerability.id, vulnerability.clone())]);
            let awards = award
                .iter()
                .map(|award| (award.vulnerability_id, award.clone()))
                .collect();
            report_rows(tracker, reports, &vulnerabilities, &awards).map_err(internal_error)?
        }
        None => Vec::new(),
    };
    let events = tracker.list_security_timeline(id).map_err(internal_error)?;
    let replies = tracker.list_security_replies(id).map_err(internal_error)?;

    Ok(Json(SecurityReportDetail {
        report,
        vulnerability,
        events,
        replies,
        award,
        related_reports,
    }))
}

fn clean_public_reason(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(|reason| reason.chars().take(MAX_PUBLIC_REASON_CHARS).collect())
}

fn overridden(admin: &AuthUser, action: &SecurityOverride) -> NewSecurityEvent {
    NewSecurityEvent::new(SecurityEventKind::Overridden)
        .with_actor(admin.email.clone())
        .with_detail(serde_json::to_value(action).unwrap_or_else(|_| json!({})))
}

fn apply_override(
    tracker: &dyn FixAttemptTracker,
    admin: &AuthUser,
    id: i64,
    action: SecurityOverride,
) -> Result<bool, Error> {
    match action {
        SecurityOverride::Reassess => tracker.transition_security_report(
            id,
            SecurityReportState::NeedsHuman,
            SecurityReportState::Received,
            &SecurityReportChanges {
                assessment_attempts: Some(0),
                ..SecurityReportChanges::default()
            },
            &overridden(admin, &SecurityOverride::Reassess),
        ),
        SecurityOverride::Outcome {
            outcome,
            public_reason,
            fixed_on,
        } => {
            let public_reason = clean_public_reason(public_reason.as_deref());
            let next = if outcome == SecurityOutcome::Spam {
                SecurityReportState::Spam
            } else {
                SecurityReportState::Replying
            };
            let event = overridden(
                admin,
                &SecurityOverride::Outcome {
                    outcome,
                    public_reason: public_reason.clone(),
                    fixed_on,
                },
            );
            tracker.transition_security_report(
                id,
                SecurityReportState::NeedsHuman,
                next,
                &SecurityReportChanges {
                    outcome: Some(outcome),
                    public_reason,
                    fixed_on,
                    ..SecurityReportChanges::default()
                },
                &event,
            )
        }
        SecurityOverride::ConfirmDelivery { kind, delivered } => {
            tracker.confirm_security_reply(id, kind, delivered, &admin.email)
        }
        SecurityOverride::ConfirmValid {
            title,
            summary,
            repository,
            severity,
        } => tracker.transition_security_report(
            id,
            SecurityReportState::NeedsHuman,
            SecurityReportState::Registering,
            &SecurityReportChanges::default(),
            &NewSecurityEvent::new(SecurityEventKind::AdminConfirmed)
                .with_actor(admin.email.clone())
                .with_detail(json!({
                    "title": title.trim(),
                    "summary": summary.trim(),
                    "repository": repository,
                    "severity": severity,
                })),
        ),
    }
}

/// `POST /api/security/reports/{id}/override`
pub async fn override_handler(
    AdminUser(admin): AdminUser,
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    body: Result<Json<SecurityOverride>, JsonRejection>,
) -> ApiResult<SecurityReport> {
    let Json(action) = body.map_err(|_| StatusCode::BAD_REQUEST)?;
    action.validate().map_err(|_| StatusCode::BAD_REQUEST)?;
    let tracker = state.tracker.as_ref();
    if tracker
        .get_security_report(id)
        .map_err(internal_error)?
        .is_none()
    {
        return Err(StatusCode::NOT_FOUND);
    }
    if !apply_override(tracker, &admin, id, action).map_err(internal_error)? {
        return Err(StatusCode::CONFLICT);
    }
    tracker
        .get_security_report(id)
        .map_err(internal_error)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// `POST /api/security/vulnerabilities/{id}/award`
pub async fn award_handler(
    AdminUser(admin): AdminUser,
    State(state): State<ApiState>,
    Path(vulnerability_id): Path<i64>,
    body: Result<Json<AwardRequest>, JsonRejection>,
) -> ApiResult<SecurityAward> {
    let Json(request) = body.map_err(|_| StatusCode::BAD_REQUEST)?;
    let note = request
        .note
        .as_deref()
        .map(str::trim)
        .filter(|note| !note.is_empty());
    if note.is_some_and(|note| note.chars().count() > MAX_AWARD_NOTE_CHARS) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let tracker = state.tracker.as_ref();
    if tracker
        .get_security_award(vulnerability_id)
        .map_err(internal_error)?
        .is_none()
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let awarded = tracker
        .update_security_award_status(
            vulnerability_id,
            &AWARDABLE_STATUSES,
            AwardStatus::Awarded,
            Some(&admin.email),
            note,
        )
        .map_err(internal_error)?;
    if !awarded {
        return Err(StatusCode::CONFLICT);
    }
    tracker
        .get_security_award(vulnerability_id)
        .map_err(internal_error)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

fn transition_vulnerability(
    tracker: &dyn FixAttemptTracker,
    vulnerability_id: i64,
    allowed: &[VulnerabilityState],
    next: VulnerabilityState,
    changes: &SecurityVulnerabilityChanges,
    event: &NewSecurityEvent,
) -> ApiResult<SecurityVulnerability> {
    let vulnerability = tracker
        .get_security_vulnerability(vulnerability_id)
        .map_err(internal_error)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if !allowed.contains(&vulnerability.state) {
        return Err(StatusCode::CONFLICT);
    }
    let moved = tracker
        .transition_security_vulnerability(
            vulnerability_id,
            vulnerability.state,
            next,
            changes,
            event,
        )
        .map_err(internal_error)?;
    if !moved {
        return Err(StatusCode::CONFLICT);
    }
    tracker
        .get_security_vulnerability(vulnerability_id)
        .map_err(internal_error)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// `POST /api/security/vulnerabilities/{id}/pull-request`
pub async fn pull_request_handler(
    AdminUser(admin): AdminUser,
    State(state): State<ApiState>,
    Path(vulnerability_id): Path<i64>,
    body: Result<Json<PullRequestLink>, JsonRejection>,
) -> ApiResult<SecurityVulnerability> {
    let Json(link) = body.map_err(|_| StatusCode::BAD_REQUEST)?;
    let pull_request_url = link.pull_request_url.trim().to_string();
    let tracker = state.tracker.as_ref();
    let vulnerability = tracker
        .get_security_vulnerability(vulnerability_id)
        .map_err(internal_error)?
        .ok_or(StatusCode::NOT_FOUND)?;
    SecurityInbox::linkable_pull_request(&vulnerability, &pull_request_url)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    transition_vulnerability(
        tracker,
        vulnerability_id,
        &LINKABLE_STATES,
        VulnerabilityState::Fixing,
        &SecurityVulnerabilityChanges {
            pull_request_url: Some(pull_request_url.clone()),
            ..SecurityVulnerabilityChanges::default()
        },
        &NewSecurityEvent::new(SecurityEventKind::PullRequestLinked)
            .with_actor(admin.email)
            .with_detail(json!({ "pull_request_url": pull_request_url })),
    )
}

/// `POST /api/security/vulnerabilities/{id}/live`
pub async fn live_handler(
    AdminUser(admin): AdminUser,
    State(state): State<ApiState>,
    Path(vulnerability_id): Path<i64>,
    body: Result<Json<LiveConfirmation>, JsonRejection>,
) -> ApiResult<SecurityVulnerability> {
    let Json(confirmation) = body.map_err(|_| StatusCode::BAD_REQUEST)?;
    let deployed_version = confirmation.deployed_version.trim();
    if deployed_version.is_empty() || deployed_version.chars().count() > MAX_DEPLOYED_VERSION_CHARS
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    transition_vulnerability(
        state.tracker.as_ref(),
        vulnerability_id,
        &LIVE_SOURCE_STATES,
        VulnerabilityState::Live,
        &SecurityVulnerabilityChanges {
            deployed_version: Some(deployed_version.to_string()),
            live_at: Some(Utc::now()),
            ..SecurityVulnerabilityChanges::default()
        },
        &NewSecurityEvent::new(SecurityEventKind::Live)
            .with_actor(admin.email)
            .with_detail(json!({ "deployed_version": deployed_version })),
    )
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{Harness, ADMIN_EMAIL};
    use super::*;
    use chrono::TimeDelta;
    use claudear_core::security::{
        DeliveryStatus, NewSecurityAward, NewSecurityReport, NewSecurityVulnerability,
        SecurityReplyKind,
    };
    use serde_json::Value;

    struct Seed<'a> {
        tracker: &'a dyn FixAttemptTracker,
    }

    impl Seed<'_> {
        fn report(&self, conversation: &str, hours_ago: i64) -> SecurityReport {
            self.tracker
                .upsert_security_report(&NewSecurityReport {
                    source: "helpscout".to_string(),
                    conversation_id: conversation.to_string(),
                    short_id: format!("HS-{conversation}"),
                    mailbox_id: "249891".to_string(),
                    url: format!("https://secure.helpscout.net/conversation/{conversation}"),
                    subject: format!("Report {conversation}"),
                    reporter_email: Some(format!("{conversation}@reporter.example")),
                    reporter_customer_id: Some(conversation.to_string()),
                    reported_at: Utc::now() - TimeDelta::hours(hours_ago),
                    customer_thread_count: 1,
                })
                .expect("report")
        }

        fn advance(&self, id: i64, path: &[SecurityReportState], changes: &SecurityReportChanges) {
            for pair in path.windows(2) {
                let moved = self
                    .tracker
                    .transition_security_report(
                        id,
                        pair[0],
                        pair[1],
                        changes,
                        &NewSecurityEvent::new(SecurityEventKind::Assessed),
                    )
                    .expect("transition");
                assert!(moved, "{} -> {}", pair[0], pair[1]);
            }
        }

        fn needs_human(&self, conversation: &str) -> SecurityReport {
            let report = self.report(conversation, 1);
            self.advance(
                report.id,
                &[
                    SecurityReportState::Received,
                    SecurityReportState::NeedsHuman,
                ],
                &SecurityReportChanges::default(),
            );
            report
        }

        fn vulnerability(
            &self,
            first: &SecurityReport,
            state: VulnerabilityState,
        ) -> SecurityVulnerability {
            self.vulnerability_in(first, state, Some("appwrite/appwrite"))
        }

        fn vulnerability_in(
            &self,
            first: &SecurityReport,
            state: VulnerabilityState,
            repository: Option<&str>,
        ) -> SecurityVulnerability {
            let vulnerability = self
                .tracker
                .create_security_vulnerability(&NewSecurityVulnerability {
                    title: "Session fixation".to_string(),
                    summary: "Sessions survive a password change.".to_string(),
                    embedding: Vec::new(),
                    repository: repository.map(str::to_string),
                    severity: Some(SecuritySeverity::High),
                    state,
                    first_report_id: first.id,
                    first_reported_at: first.reported_at,
                    merge_commit: None,
                    merged_at: None,
                })
                .expect("vulnerability");
            self.advance(
                first.id,
                &[
                    SecurityReportState::Received,
                    SecurityReportState::Assessing,
                    SecurityReportState::Tracking,
                ],
                &SecurityReportChanges {
                    outcome: Some(SecurityOutcome::Valid),
                    vulnerability_id: Some(vulnerability.id),
                    ..SecurityReportChanges::default()
                },
            );
            vulnerability
        }

        fn move_vulnerability(&self, id: i64, from: VulnerabilityState, to: VulnerabilityState) {
            let moved = self
                .tracker
                .transition_security_vulnerability(
                    id,
                    from,
                    to,
                    &SecurityVulnerabilityChanges::default(),
                    &NewSecurityEvent::new(SecurityEventKind::FixStarted),
                )
                .expect("vulnerability transition");
            assert!(moved);
        }

        fn award(&self, vulnerability: &SecurityVulnerability) {
            let prepared = self
                .tracker
                .prepare_security_award(
                    vulnerability.id,
                    vulnerability.first_report_id,
                    &NewSecurityAward {
                        campaign: "SecurityResearchers".to_string(),
                        credits: 100,
                    },
                )
                .expect("award");
            assert!(prepared);
        }

        fn unconfirmed_reply(&self, report_id: i64, kind: SecurityReplyKind) {
            assert!(self
                .tracker
                .claim_security_reply(
                    report_id,
                    kind,
                    &NewSecurityEvent::new(SecurityEventKind::ReplyClaimed),
                )
                .expect("claim"));
            self.tracker
                .complete_security_reply(
                    report_id,
                    kind,
                    DeliveryStatus::Unconfirmed,
                    Some("timeout"),
                    &NewSecurityEvent::new(SecurityEventKind::ReplyUnconfirmed),
                    None,
                )
                .expect("complete");
        }
    }

    fn report_state(harness: &Harness, id: i64) -> SecurityReport {
        harness
            .tracker
            .get_security_report(id)
            .expect("read")
            .expect("report exists")
    }

    fn timeline_kinds(harness: &Harness, id: i64) -> Vec<SecurityEventKind> {
        harness
            .tracker
            .list_security_timeline(id)
            .expect("timeline")
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    fn last_event(harness: &Harness, id: i64) -> SecurityEvent {
        harness
            .tracker
            .list_security_timeline(id)
            .expect("timeline")
            .pop()
            .expect("an event")
    }

    #[tokio::test]
    async fn every_security_endpoint_is_admin_only() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let needs_human = seed.needs_human("100");
        let awaiting = seed.vulnerability(&seed.report("101", 2), VulnerabilityState::AwaitingFix);
        let merged = seed.vulnerability(&seed.report("102", 3), VulnerabilityState::Merged);
        seed.award(&merged);

        let reads = [
            "/api/security/summary".to_string(),
            "/api/security/reports".to_string(),
            format!("/api/security/reports/{}", needs_human.id),
        ];
        for uri in &reads {
            assert_eq!(
                harness.get(uri, None).await.0,
                StatusCode::UNAUTHORIZED,
                "{uri}"
            );
            assert_eq!(
                harness.get(uri, Some(&harness.viewer)).await.0,
                StatusCode::FORBIDDEN,
                "{uri}"
            );
            assert_eq!(
                harness.get(uri, Some(&harness.admin)).await.0,
                StatusCode::OK,
                "{uri}"
            );
        }

        let writes = [
            (
                format!("/api/security/reports/{}/override", needs_human.id),
                r#"{"action":"reassess"}"#.to_string(),
            ),
            (
                format!("/api/security/vulnerabilities/{}/award", merged.id),
                "{}".to_string(),
            ),
            (
                format!("/api/security/vulnerabilities/{}/pull-request", awaiting.id),
                r#"{"pull_request_url":"https://github.com/appwrite/appwrite/pull/42"}"#
                    .to_string(),
            ),
            (
                format!("/api/security/vulnerabilities/{}/live", merged.id),
                r#"{"deployed_version":"1.53.31"}"#.to_string(),
            ),
        ];
        for (uri, body) in &writes {
            assert_eq!(
                harness.post(uri, None, body).await.0,
                StatusCode::UNAUTHORIZED,
                "{uri}"
            );
            assert_eq!(
                harness.post(uri, Some(&harness.viewer), body).await.0,
                StatusCode::FORBIDDEN,
                "{uri}"
            );
        }
        assert_eq!(
            report_state(&harness, needs_human.id).state,
            SecurityReportState::NeedsHuman,
            "a rejected caller changes nothing"
        );
        for (uri, body) in &writes {
            assert_eq!(
                harness.post(uri, Some(&harness.admin), body).await.0,
                StatusCode::OK,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn security_writes_without_the_csrf_token_are_rejected_by_the_dashboard_stack() {
        let harness = Harness::with_dashboard_middleware();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let report = seed.needs_human("150");
        let awaiting = seed.vulnerability(&seed.report("151", 2), VulnerabilityState::AwaitingFix);
        let override_uri = format!("/api/security/reports/{}/override", report.id);
        let link_uri = format!("/api/security/vulnerabilities/{}/pull-request", awaiting.id);
        let link = r#"{"pull_request_url":"https://github.com/appwrite/appwrite/pull/42"}"#;
        let reassess = r#"{"action":"reassess"}"#;

        for (uri, body) in [(&override_uri, reassess), (&link_uri, link)] {
            assert_eq!(
                harness.post(uri, Some(&harness.admin), body).await.0,
                StatusCode::FORBIDDEN,
                "{uri}"
            );
        }
        assert_eq!(
            report_state(&harness, report.id).state,
            SecurityReportState::NeedsHuman,
            "a forged request changes nothing"
        );
        assert!(harness
            .tracker
            .get_security_vulnerability(awaiting.id)
            .expect("read")
            .expect("vulnerability")
            .pull_request_url
            .is_none());

        let token = "a-matching-csrf-token";
        for (uri, body) in [(&override_uri, reassess), (&link_uri, link)] {
            assert_eq!(
                harness
                    .post_with_csrf(uri, &harness.admin, token, body)
                    .await
                    .0,
                StatusCode::OK,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn reports_list_rows_with_vulnerability_award_and_reply_state() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let first = seed.report("200", 5);
        let vulnerability = seed.vulnerability(&first, VulnerabilityState::Merged);
        seed.award(&vulnerability);
        let duplicate = seed.report("201", 1);
        seed.advance(
            duplicate.id,
            &[
                SecurityReportState::Received,
                SecurityReportState::Assessing,
                SecurityReportState::Replying,
            ],
            &SecurityReportChanges {
                outcome: Some(SecurityOutcome::Duplicate),
                vulnerability_id: Some(vulnerability.id),
                ..SecurityReportChanges::default()
            },
        );
        seed.unconfirmed_reply(duplicate.id, SecurityReplyKind::Duplicate);
        seed.needs_human("202");

        let (status, rows) = harness
            .get("/api/security/reports", Some(&harness.admin))
            .await;
        assert_eq!(status, StatusCode::OK);
        let rows = rows.as_array().expect("rows");
        assert_eq!(rows.len(), 3);
        let row = |id: i64| -> &Value {
            rows.iter()
                .find(|row| row["id"] == id)
                .expect("row for report")
        };

        let first_row = row(first.id);
        assert_eq!(first_row["state"], "tracking");
        assert_eq!(first_row["outcome"], "valid");
        assert_eq!(first_row["is_first_reporter"], true);
        assert_eq!(first_row["vulnerability_id"], vulnerability.id);
        assert_eq!(first_row["vulnerability_state"], "merged");
        assert_eq!(first_row["severity"], "high");
        assert_eq!(first_row["award_status"], "prepared");
        assert_eq!(first_row["unconfirmed_reply"], false);
        assert_eq!(first_row["short_id"], "HS-200");
        assert!(first_row["reported_at"].is_string());
        assert!(first_row["updated_at"].is_string());
        for field in ["pull_request_url", "deployed_version", "live_at"] {
            assert!(first_row[field].is_null(), "{field}");
        }

        let duplicate_row = row(duplicate.id);
        assert_eq!(duplicate_row["outcome"], "duplicate");
        assert_eq!(duplicate_row["is_first_reporter"], false);
        assert!(
            duplicate_row["award_status"].is_null(),
            "the award belongs to the first reporter only"
        );
        assert_eq!(duplicate_row["unconfirmed_reply"], true);

        let (_, filtered) = harness
            .get(
                "/api/security/reports?state=needs_human",
                Some(&harness.admin),
            )
            .await;
        assert_eq!(filtered.as_array().map(Vec::len), Some(1));
        let (_, by_outcome) = harness
            .get(
                "/api/security/reports?outcome=duplicate",
                Some(&harness.admin),
            )
            .await;
        assert_eq!(by_outcome[0]["id"], duplicate.id);
        let (_, limited) = harness
            .get("/api/security/reports?limit=1", Some(&harness.admin))
            .await;
        assert_eq!(limited.as_array().map(Vec::len), Some(1));
        assert_eq!(
            harness
                .get("/api/security/reports?state=bogus", Some(&harness.admin))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn summary_counts_reports_vulnerabilities_awards_and_unconfirmed_replies() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let first = seed.report("300", 4);
        let vulnerability = seed.vulnerability(&first, VulnerabilityState::Merged);
        seed.award(&vulnerability);
        seed.unconfirmed_reply(first.id, SecurityReplyKind::Confirmed);
        seed.needs_human("301");
        seed.needs_human("302");
        seed.report("303", 1);

        let (status, summary) = harness
            .get("/api/security/summary", Some(&harness.admin))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(summary["reports_by_state"]["needs_human"], 2);
        assert_eq!(summary["reports_by_state"]["tracking"], 1);
        assert_eq!(summary["reports_by_state"]["received"], 1);
        assert_eq!(summary["reports_by_outcome"]["valid"], 1);
        assert_eq!(summary["vulnerabilities_by_state"]["merged"], 1);
        assert_eq!(summary["pending_awards"], 1);
        assert_eq!(summary["unconfirmed_replies"], 1);
    }

    #[tokio::test]
    async fn report_detail_includes_timeline_replies_award_and_related_reports() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let first = seed.report("400", 6);
        let vulnerability = seed.vulnerability(&first, VulnerabilityState::Merged);
        seed.award(&vulnerability);
        seed.unconfirmed_reply(first.id, SecurityReplyKind::Confirmed);
        let duplicate = seed.report("401", 2);
        seed.advance(
            duplicate.id,
            &[
                SecurityReportState::Received,
                SecurityReportState::Assessing,
                SecurityReportState::Replying,
            ],
            &SecurityReportChanges {
                outcome: Some(SecurityOutcome::Duplicate),
                vulnerability_id: Some(vulnerability.id),
                ..SecurityReportChanges::default()
            },
        );

        let (status, detail) = harness
            .get(
                &format!("/api/security/reports/{}", first.id),
                Some(&harness.admin),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["report"]["id"], first.id);
        assert_eq!(detail["report"]["reporter_email"], "400@reporter.example");
        assert_eq!(detail["vulnerability"]["id"], vulnerability.id);
        assert!(
            detail["vulnerability"].get("embedding").is_none(),
            "embeddings never leave the server"
        );
        assert_eq!(detail["award"]["status"], "prepared");
        assert_eq!(detail["replies"][0]["status"], "unconfirmed");
        let events: Vec<&str> = detail["events"]
            .as_array()
            .expect("events")
            .iter()
            .filter_map(|event| event["kind"].as_str())
            .collect();
        assert_eq!(events.first(), Some(&"received"));
        assert!(events.contains(&"vulnerability_created"));
        assert!(events.contains(&"award_prepared"));
        let related: Vec<&Value> = detail["related_reports"]
            .as_array()
            .expect("related")
            .iter()
            .collect();
        assert_eq!(related.len(), 2);
        assert_eq!(related[0]["id"], first.id);
        assert_eq!(related[0]["is_first_reporter"], true);
        assert_eq!(related[1]["id"], duplicate.id);

        let (_, lone) = harness
            .get(
                &format!("/api/security/reports/{}", seed.needs_human("402").id),
                Some(&harness.admin),
            )
            .await;
        assert!(lone["vulnerability"].is_null());
        assert!(lone["award"].is_null());
        assert_eq!(lone["related_reports"], serde_json::json!([]));

        assert_eq!(
            harness
                .get("/api/security/reports/9999", Some(&harness.admin))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn override_rejects_invalid_bodies_unknown_reports_and_wrong_states() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let report = seed.needs_human("500");
        let uri = format!("/api/security/reports/{}/override", report.id);
        let too_long = "x".repeat(MAX_PUBLIC_REASON_CHARS + 1);
        let invalid = [
            "not json".to_string(),
            r#"{"action":"bogus"}"#.to_string(),
            r#"{"action":"outcome","outcome":"valid"}"#.to_string(),
            r#"{"action":"outcome","outcome":"already_fixed"}"#.to_string(),
            r#"{"action":"outcome","outcome":"not_vulnerability","public_reason":"   "}"#
                .to_string(),
            format!(
                r#"{{"action":"outcome","outcome":"out_of_scope","public_reason":"{too_long}"}}"#
            ),
            r#"{"action":"confirm_valid","title":"","summary":"s"}"#.to_string(),
            r#"{"action":"confirm_valid","title":"t","summary":"s","repository":"no-slash"}"#
                .to_string(),
        ];
        for body in &invalid {
            assert_eq!(
                harness.post(&uri, Some(&harness.admin), body).await.0,
                StatusCode::BAD_REQUEST,
                "{body}"
            );
        }
        assert_eq!(
            report_state(&harness, report.id).state,
            SecurityReportState::NeedsHuman
        );

        assert_eq!(
            harness
                .post(
                    "/api/security/reports/9999/override",
                    Some(&harness.admin),
                    r#"{"action":"reassess"}"#
                )
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        let received = seed.report("501", 1);
        let received_uri = format!("/api/security/reports/{}/override", received.id);
        for body in [
            r#"{"action":"reassess"}"#,
            r#"{"action":"outcome","outcome":"spam"}"#,
            r#"{"action":"confirm_valid","title":"t","summary":"s"}"#,
            r#"{"action":"confirm_delivery","kind":"confirmed","delivered":true}"#,
        ] {
            assert_eq!(
                harness
                    .post(&received_uri, Some(&harness.admin), body)
                    .await
                    .0,
                StatusCode::CONFLICT,
                "{body}"
            );
        }
        assert_eq!(
            report_state(&harness, received.id).state,
            SecurityReportState::Received
        );
        assert_eq!(
            timeline_kinds(&harness, received.id),
            [SecurityEventKind::Received]
        );
    }

    #[tokio::test]
    async fn reassess_returns_the_report_to_received_with_a_fresh_attempt_count() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let report = seed.needs_human("600");
        seed.advance(
            report.id,
            &[
                SecurityReportState::NeedsHuman,
                SecurityReportState::Received,
            ],
            &SecurityReportChanges {
                assessment_attempts: Some(3),
                ..SecurityReportChanges::default()
            },
        );
        seed.advance(
            report.id,
            &[
                SecurityReportState::Received,
                SecurityReportState::NeedsHuman,
            ],
            &SecurityReportChanges::default(),
        );

        let (status, body) = harness
            .post(
                &format!("/api/security/reports/{}/override", report.id),
                Some(&harness.admin),
                r#"{"action":"reassess"}"#,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "received");
        assert_eq!(body["assessment_attempts"], 0);
        let event = last_event(&harness, report.id);
        assert_eq!(event.kind, SecurityEventKind::Overridden);
        assert_eq!(event.actor.as_deref(), Some(ADMIN_EMAIL));
        assert_eq!(event.detail["action"], "reassess");
    }

    #[tokio::test]
    async fn outcome_override_moves_to_replying_with_a_trimmed_public_reason() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let report = seed.needs_human("700");

        let (status, body) = harness
            .post(
                &format!("/api/security/reports/{}/override", report.id),
                Some(&harness.admin),
                r#"{"action":"outcome","outcome":"out_of_scope","public_reason":"  Self-XSS only.  "}"#,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "replying");
        assert_eq!(body["outcome"], "out_of_scope");
        assert_eq!(body["public_reason"], "Self-XSS only.");
        let event = last_event(&harness, report.id);
        assert_eq!(event.kind, SecurityEventKind::Overridden);
        assert_eq!(event.actor.as_deref(), Some(ADMIN_EMAIL));
        assert_eq!(event.detail["outcome"], "out_of_scope");
        assert_eq!(event.detail["public_reason"], "Self-XSS only.");
        assert_eq!(event.from_state.as_deref(), Some("needs_human"));
        assert_eq!(event.to_state.as_deref(), Some("replying"));

        let fixed = seed.needs_human("701");
        let (status, body) = harness
            .post(
                &format!("/api/security/reports/{}/override", fixed.id),
                Some(&harness.admin),
                r#"{"action":"outcome","outcome":"already_fixed","fixed_on":"2026-10-01"}"#,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["fixed_on"], "2026-10-01");
        assert!(body["public_reason"].is_null());
    }

    #[tokio::test]
    async fn spam_outcome_override_moves_to_spam() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let report = seed.needs_human("800");

        let (status, body) = harness
            .post(
                &format!("/api/security/reports/{}/override", report.id),
                Some(&harness.admin),
                r#"{"action":"outcome","outcome":"spam"}"#,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "spam");
        assert_eq!(body["outcome"], "spam");
        assert!(
            !timeline_kinds(&harness, report.id).contains(&SecurityEventKind::ConversationClosed)
        );
    }

    #[tokio::test]
    async fn confirm_delivery_resolves_an_unconfirmed_reply() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let report = seed.needs_human("900");
        seed.advance(
            report.id,
            &[
                SecurityReportState::NeedsHuman,
                SecurityReportState::Replying,
            ],
            &SecurityReportChanges {
                outcome: Some(SecurityOutcome::NotReproducible),
                ..SecurityReportChanges::default()
            },
        );
        seed.unconfirmed_reply(report.id, SecurityReplyKind::NotReproducible);
        let uri = format!("/api/security/reports/{}/override", report.id);
        let body = r#"{"action":"confirm_delivery","kind":"not_reproducible","delivered":true}"#;

        let (status, returned) = harness.post(&uri, Some(&harness.admin), body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(returned["id"], report.id);
        let replies = harness
            .tracker
            .list_security_replies(report.id)
            .expect("replies");
        assert_eq!(replies[0].status, DeliveryStatus::Sent);
        let event = last_event(&harness, report.id);
        assert_eq!(event.kind, SecurityEventKind::DeliveryConfirmed);
        assert_eq!(event.actor.as_deref(), Some(ADMIN_EMAIL));

        assert_eq!(
            harness.post(&uri, Some(&harness.admin), body).await.0,
            StatusCode::CONFLICT,
            "a reply is confirmed once"
        );
    }

    #[tokio::test]
    async fn confirm_valid_hands_the_report_back_for_registration() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let report = seed.needs_human("1000");

        let (status, body) = harness
            .post(
                &format!("/api/security/reports/{}/override", report.id),
                Some(&harness.admin),
                r#"{"action":"confirm_valid","title":"  Token leak  ","summary":"Tokens are logged.","repository":"appwrite/appwrite","severity":"critical"}"#,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "registering");
        assert!(body["vulnerability_id"].is_null());
        assert!(harness
            .tracker
            .list_security_vulnerabilities(None)
            .expect("vulnerabilities")
            .is_empty());

        let event = last_event(&harness, report.id);
        assert_eq!(event.kind, SecurityEventKind::AdminConfirmed);
        assert_eq!(event.actor.as_deref(), Some(ADMIN_EMAIL));
        assert_eq!(
            event.detail,
            serde_json::json!({
                "title": "Token leak",
                "summary": "Tokens are logged.",
                "repository": "appwrite/appwrite",
                "severity": "critical",
            })
        );
        let confirmed = harness
            .tracker
            .list_admin_confirmed_reports()
            .expect("confirmed");
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].0.id, report.id);
        assert_eq!(confirmed[0].1.id, event.id);
        assert_eq!(
            harness
                .tracker
                .release_stale_security_assessments(std::time::Duration::ZERO)
                .expect("sweep"),
            0,
            "the stale assessment sweep cannot undo a confirmation"
        );
        assert_eq!(
            report_state(&harness, report.id).state,
            SecurityReportState::Registering
        );
    }

    #[tokio::test]
    async fn award_is_granted_once_and_records_the_admin() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let vulnerability = seed.vulnerability(&seed.report("1100", 3), VulnerabilityState::Merged);
        let uri = format!("/api/security/vulnerabilities/{}/award", vulnerability.id);

        assert_eq!(
            harness.post(&uri, Some(&harness.admin), "{}").await.0,
            StatusCode::NOT_FOUND,
            "no award is prepared yet"
        );
        assert_eq!(
            harness
                .post(
                    "/api/security/vulnerabilities/9999/award",
                    Some(&harness.admin),
                    "{}"
                )
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        seed.award(&vulnerability);
        let too_long =
            serde_json::json!({ "note": "n".repeat(MAX_AWARD_NOTE_CHARS + 1) }).to_string();
        assert_eq!(
            harness.post(&uri, Some(&harness.admin), &too_long).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            harness
                .tracker
                .get_security_award(vulnerability.id)
                .expect("read")
                .expect("award")
                .status,
            AwardStatus::Prepared,
            "an oversized note changes nothing"
        );
        let (status, award) = harness
            .post(
                &uri,
                Some(&harness.admin),
                r#"{"note":"  Sent code ABC  "}"#,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(award["status"], "awarded");
        assert_eq!(award["actor"], ADMIN_EMAIL);
        assert_eq!(award["note"], "Sent code ABC");
        let awarded = harness
            .tracker
            .last_security_event(vulnerability.id, SecurityEventKind::Awarded)
            .expect("event")
            .expect("awarded event");
        assert_eq!(awarded.actor.as_deref(), Some(ADMIN_EMAIL));

        assert_eq!(
            harness.post(&uri, Some(&harness.admin), "{}").await.0,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn pull_request_link_validates_the_url_and_moves_to_fixing() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let vulnerability =
            seed.vulnerability(&seed.report("1200", 3), VulnerabilityState::AwaitingFix);
        let uri = format!(
            "/api/security/vulnerabilities/{}/pull-request",
            vulnerability.id
        );
        for url in [
            "",
            "not a url",
            "https://github.com/appwrite/appwrite/issues/1",
            "http://github.com/appwrite/appwrite/pull/1",
            "javascript:alert(1)//github.com/appwrite/appwrite/pull/1",
            "https://evil.example/github.com/appwrite/appwrite/pull/1",
            "https://evil.example/appwrite/appwrite/pull/1",
            "https://user@github.com/appwrite/appwrite/pull/1",
            "https://github.com/appwrite/appwrite/pull/1?x=1",
            "https://github.com/appwrite/appwrite/pull/1#x",
            "https://github.com/attacker/repo/pull/1",
            "https://github.com/appwrite/appwrite-fork/pull/1",
        ] {
            let body = serde_json::json!({ "pull_request_url": url }).to_string();
            assert_eq!(
                harness.post(&uri, Some(&harness.admin), &body).await.0,
                StatusCode::BAD_REQUEST,
                "{url}"
            );
        }
        assert_eq!(
            harness
                .post(&uri, Some(&harness.admin), r#"{"url":"x"}"#)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let valid = r#"{"pull_request_url":" https://github.com/appwrite/appwrite/pull/7 "}"#;
        assert_eq!(
            harness
                .post(
                    "/api/security/vulnerabilities/9999/pull-request",
                    Some(&harness.admin),
                    valid
                )
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        assert_eq!(
            harness
                .tracker
                .get_security_vulnerability(vulnerability.id)
                .expect("read")
                .expect("vulnerability")
                .state,
            VulnerabilityState::AwaitingFix,
            "a rejected link changes nothing"
        );

        let (status, body) = harness.post(&uri, Some(&harness.admin), valid).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "fixing");
        assert_eq!(
            body["pull_request_url"],
            "https://github.com/appwrite/appwrite/pull/7"
        );
        let event = harness
            .tracker
            .last_security_event(vulnerability.id, SecurityEventKind::PullRequestLinked)
            .expect("event")
            .expect("linked event");
        assert_eq!(event.actor.as_deref(), Some(ADMIN_EMAIL));

        assert_eq!(
            harness.post(&uri, Some(&harness.admin), valid).await.0,
            StatusCode::CONFLICT,
            "a fixing vulnerability is not relinked"
        );
    }

    #[tokio::test]
    async fn pull_request_link_without_a_known_repository_accepts_any_github_repository() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let vulnerability = seed.vulnerability_in(
            &seed.report("1250", 3),
            VulnerabilityState::AwaitingFix,
            None,
        );
        let (status, body) = harness
            .post(
                &format!(
                    "/api/security/vulnerabilities/{}/pull-request",
                    vulnerability.id
                ),
                Some(&harness.admin),
                r#"{"pull_request_url":"https://github.com/appwrite/console/pull/9"}"#,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["pull_request_url"],
            "https://github.com/appwrite/console/pull/9"
        );
    }

    #[tokio::test]
    async fn live_requires_a_merged_or_stuck_fix_and_a_version() {
        let harness = Harness::start();
        let seed = Seed {
            tracker: harness.tracker.as_ref(),
        };
        let fixing = seed.vulnerability(&seed.report("1300", 3), VulnerabilityState::AwaitingFix);
        seed.move_vulnerability(
            fixing.id,
            VulnerabilityState::AwaitingFix,
            VulnerabilityState::Fixing,
        );
        let body = r#"{"deployed_version":"1.53.31"}"#;
        assert_eq!(
            harness
                .post(
                    &format!("/api/security/vulnerabilities/{}/live", fixing.id),
                    Some(&harness.admin),
                    body
                )
                .await
                .0,
            StatusCode::CONFLICT
        );

        let merged = seed.vulnerability(&seed.report("1301", 2), VulnerabilityState::Merged);
        let uri = format!("/api/security/vulnerabilities/{}/live", merged.id);
        let too_long =
            serde_json::json!({ "deployed_version": "1".repeat(MAX_DEPLOYED_VERSION_CHARS + 1) })
                .to_string();
        for invalid in [r#"{"deployed_version":"   "}"#, too_long.as_str(), "{}"] {
            assert_eq!(
                harness.post(&uri, Some(&harness.admin), invalid).await.0,
                StatusCode::BAD_REQUEST,
                "{invalid}"
            );
        }
        assert_eq!(
            harness
                .post(
                    "/api/security/vulnerabilities/9999/live",
                    Some(&harness.admin),
                    body
                )
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        let (status, live) = harness.post(&uri, Some(&harness.admin), body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(live["state"], "live");
        assert_eq!(live["deployed_version"], "1.53.31");
        assert!(live["live_at"].is_string());
        let event = harness
            .tracker
            .last_security_event(merged.id, SecurityEventKind::Live)
            .expect("event")
            .expect("live event");
        assert_eq!(event.actor.as_deref(), Some(ADMIN_EMAIL));
        assert_eq!(event.detail["deployed_version"], "1.53.31");

        let stuck = seed.vulnerability(&seed.report("1302", 1), VulnerabilityState::Merged);
        seed.move_vulnerability(
            stuck.id,
            VulnerabilityState::Merged,
            VulnerabilityState::NeedsHuman,
        );
        assert_eq!(
            harness
                .post(
                    &format!("/api/security/vulnerabilities/{}/live", stuck.id),
                    Some(&harness.admin),
                    body
                )
                .await
                .0,
            StatusCode::OK
        );
    }
}
