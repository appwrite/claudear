use super::SqliteTracker;
use crate::vector_index::{embedding_blob, embedding_from_blob};
use chrono::{DateTime, NaiveDate, SecondsFormat, TimeDelta, Utc};
use claudear_core::error::{Error, Result};
use claudear_core::security::{
    AwardStatus, DeliveryStatus, NewSecurityAward, NewSecurityEvent, NewSecurityReport,
    NewSecurityVulnerability, SecurityAward, SecurityEvent, SecurityEventKind, SecurityReply,
    SecurityReplyKind, SecurityReport, SecurityReportChanges, SecurityReportFilter,
    SecurityReportState, SecurityVulnerability, SecurityVulnerabilityChanges, VulnerabilityState,
};
use rusqlite::types::{Type, Value};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row, TransactionBehavior};
use serde_json::json;
use std::str::FromStr;
use std::time::Duration;

const REPORTS: &str = "security_reports";
const VULNERABILITIES: &str = "security_vulnerabilities";
const DEFAULT_REPORT_LIMIT: usize = 100;
const DATE_FORMAT: &str = "%Y-%m-%d";
const NOT_DELIVERED_ERROR: &str = "confirmed not delivered by an admin";

const REPORT_COLUMNS: &[&str] = &[
    "id",
    "source",
    "conversation_id",
    "short_id",
    "mailbox_id",
    "url",
    "subject",
    "reporter_email",
    "reporter_customer_id",
    "reported_at",
    "state",
    "outcome",
    "public_reason",
    "assessment_summary",
    "affected_repository",
    "fixed_by_commit",
    "fixed_on",
    "spam_confidence",
    "assessment_attempts",
    "customer_thread_count",
    "vulnerability_id",
    "created_at",
    "updated_at",
];

const VULNERABILITY_COLUMNS: &[&str] = &[
    "id",
    "title",
    "summary",
    "embedding",
    "repository",
    "severity",
    "state",
    "first_report_id",
    "first_reported_at",
    "fix_attempt_id",
    "pull_request_url",
    "merge_commit",
    "merged_at",
    "deployed_version",
    "live_at",
    "created_at",
    "updated_at",
];

const EVENT_COLUMNS: &[&str] = &[
    "id",
    "report_id",
    "vulnerability_id",
    "kind",
    "from_state",
    "to_state",
    "actor",
    "detail",
    "created_at",
];

const REPLY_COLUMNS: &[&str] = &[
    "id",
    "report_id",
    "kind",
    "status",
    "error",
    "claimed_at",
    "completed_at",
];

const AWARD_COLUMNS: &[&str] = &[
    "id",
    "vulnerability_id",
    "report_id",
    "recipient_email",
    "campaign",
    "credits",
    "status",
    "actor",
    "note",
    "prepared_at",
    "updated_at",
];

type Assignment = (&'static str, Value);

#[derive(Debug, Clone, Copy, Default)]
struct EventRow<'a> {
    report_id: Option<i64>,
    vulnerability_id: Option<i64>,
    from_state: Option<&'a str>,
    to_state: Option<&'a str>,
}

fn select_list(columns: &[&str], table: &str) -> String {
    columns
        .iter()
        .map(|column| format!("{table}.{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn now() -> String {
    timestamp(Utc::now())
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn conversion_error(index: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, Type::Text, message.into())
}

fn parse_enum<T: FromStr<Err = String>>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let value: String = row.get(index)?;
    value
        .parse()
        .map_err(|error| conversion_error(index, error))
}

fn parse_optional_enum<T: FromStr<Err = String>>(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<T>> {
    let value: Option<String> = row.get(index)?;
    value
        .map(|value| {
            value
                .parse()
                .map_err(|error| conversion_error(index, error))
        })
        .transpose()
}

fn parse_time(row: &Row<'_>, index: usize) -> rusqlite::Result<DateTime<Utc>> {
    let value: String = row.get(index)?;
    SqliteTracker::parse_datetime(&value)
}

fn parse_optional_time(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let value: Option<String> = row.get(index)?;
    value
        .map(|value| SqliteTracker::parse_datetime(&value))
        .transpose()
}

fn parse_optional_date(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<NaiveDate>> {
    let value: Option<String> = row.get(index)?;
    value
        .map(|value| {
            NaiveDate::parse_from_str(&value, DATE_FORMAT)
                .map_err(|error| conversion_error(index, error.to_string()))
        })
        .transpose()
}

fn row_to_report(row: &Row<'_>, offset: usize) -> rusqlite::Result<SecurityReport> {
    Ok(SecurityReport {
        id: row.get(offset)?,
        source: row.get(offset + 1)?,
        conversation_id: row.get(offset + 2)?,
        short_id: row.get(offset + 3)?,
        mailbox_id: row.get(offset + 4)?,
        url: row.get(offset + 5)?,
        subject: row.get(offset + 6)?,
        reporter_email: row.get(offset + 7)?,
        reporter_customer_id: row.get(offset + 8)?,
        reported_at: parse_time(row, offset + 9)?,
        state: parse_enum(row, offset + 10)?,
        outcome: parse_optional_enum(row, offset + 11)?,
        public_reason: row.get(offset + 12)?,
        assessment_summary: row.get(offset + 13)?,
        affected_repository: row.get(offset + 14)?,
        fixed_by_commit: row.get(offset + 15)?,
        fixed_on: parse_optional_date(row, offset + 16)?,
        spam_confidence: row.get(offset + 17)?,
        assessment_attempts: row.get(offset + 18)?,
        customer_thread_count: row.get(offset + 19)?,
        vulnerability_id: row.get(offset + 20)?,
        created_at: parse_time(row, offset + 21)?,
        updated_at: parse_time(row, offset + 22)?,
    })
}

fn row_to_vulnerability(row: &Row<'_>) -> rusqlite::Result<SecurityVulnerability> {
    let embedding: Option<Vec<u8>> = row.get(3)?;
    let embedding = match embedding {
        None => Vec::new(),
        Some(blob) => embedding_from_blob(&blob)
            .ok_or_else(|| conversion_error(3, "malformed embedding blob".to_string()))?,
    };
    Ok(SecurityVulnerability {
        id: row.get(0)?,
        title: row.get(1)?,
        summary: row.get(2)?,
        embedding,
        repository: row.get(4)?,
        severity: parse_optional_enum(row, 5)?,
        state: parse_enum(row, 6)?,
        first_report_id: row.get(7)?,
        first_reported_at: parse_time(row, 8)?,
        fix_attempt_id: row.get(9)?,
        pull_request_url: row.get(10)?,
        merge_commit: row.get(11)?,
        merged_at: parse_optional_time(row, 12)?,
        deployed_version: row.get(13)?,
        live_at: parse_optional_time(row, 14)?,
        created_at: parse_time(row, 15)?,
        updated_at: parse_time(row, 16)?,
    })
}

fn row_to_event(row: &Row<'_>, offset: usize) -> rusqlite::Result<SecurityEvent> {
    let detail: String = row.get(offset + 7)?;
    Ok(SecurityEvent {
        id: row.get(offset)?,
        report_id: row.get(offset + 1)?,
        vulnerability_id: row.get(offset + 2)?,
        kind: parse_enum(row, offset + 3)?,
        from_state: row.get(offset + 4)?,
        to_state: row.get(offset + 5)?,
        actor: row.get(offset + 6)?,
        detail: serde_json::from_str(&detail)
            .map_err(|error| conversion_error(offset + 7, error.to_string()))?,
        created_at: parse_time(row, offset + 8)?,
    })
}

fn row_to_reply(row: &Row<'_>) -> rusqlite::Result<SecurityReply> {
    Ok(SecurityReply {
        id: row.get(0)?,
        report_id: row.get(1)?,
        kind: parse_enum(row, 2)?,
        status: parse_enum(row, 3)?,
        error: row.get(4)?,
        claimed_at: parse_time(row, 5)?,
        completed_at: parse_optional_time(row, 6)?,
    })
}

fn row_to_award(row: &Row<'_>) -> rusqlite::Result<SecurityAward> {
    Ok(SecurityAward {
        id: row.get(0)?,
        vulnerability_id: row.get(1)?,
        report_id: row.get(2)?,
        recipient_email: row.get(3)?,
        campaign: row.get(4)?,
        credits: row.get(5)?,
        status: parse_enum(row, 6)?,
        actor: row.get(7)?,
        note: row.get(8)?,
        prepared_at: parse_time(row, 9)?,
        updated_at: parse_time(row, 10)?,
    })
}

fn report_assignments(changes: &SecurityReportChanges) -> Vec<Assignment> {
    let mut assignments = Vec::new();
    if let Some(outcome) = changes.outcome {
        assignments.push(("outcome", text(outcome.as_str())));
    }
    if let Some(reason) = &changes.public_reason {
        assignments.push(("public_reason", text(reason)));
    }
    if let Some(summary) = &changes.assessment_summary {
        assignments.push(("assessment_summary", text(summary)));
    }
    if let Some(repository) = &changes.affected_repository {
        assignments.push(("affected_repository", text(repository)));
    }
    if let Some(commit) = &changes.fixed_by_commit {
        assignments.push(("fixed_by_commit", text(commit)));
    }
    if let Some(fixed_on) = changes.fixed_on {
        assignments.push(("fixed_on", text(&fixed_on.format(DATE_FORMAT).to_string())));
    }
    if let Some(confidence) = changes.spam_confidence {
        assignments.push(("spam_confidence", Value::Real(confidence)));
    }
    if let Some(attempts) = changes.assessment_attempts {
        assignments.push(("assessment_attempts", Value::Integer(i64::from(attempts))));
    }
    if let Some(vulnerability_id) = changes.vulnerability_id {
        assignments.push(("vulnerability_id", Value::Integer(vulnerability_id)));
    }
    assignments
}

fn vulnerability_assignments(changes: &SecurityVulnerabilityChanges) -> Vec<Assignment> {
    let mut assignments = Vec::new();
    if let Some(repository) = &changes.repository {
        assignments.push(("repository", text(repository)));
    }
    if let Some(attempt_id) = changes.fix_attempt_id {
        assignments.push(("fix_attempt_id", Value::Integer(attempt_id)));
    }
    if let Some(url) = &changes.pull_request_url {
        assignments.push(("pull_request_url", text(url)));
    }
    if let Some(commit) = &changes.merge_commit {
        assignments.push(("merge_commit", text(commit)));
    }
    if let Some(merged_at) = changes.merged_at {
        assignments.push(("merged_at", text(&timestamp(merged_at))));
    }
    if let Some(version) = &changes.deployed_version {
        assignments.push(("deployed_version", text(version)));
    }
    if let Some(live_at) = changes.live_at {
        assignments.push(("live_at", text(&timestamp(live_at))));
    }
    assignments
}

/// `table` and every assignment column are compile-time constants, never input.
fn update_in_state(
    connection: &Connection,
    table: &'static str,
    id: i64,
    expected: &str,
    assignments: Vec<Assignment>,
) -> Result<bool> {
    let set = assignments
        .iter()
        .map(|(column, _)| format!("{column} = ?"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("UPDATE {table} SET {set} WHERE id = ? AND state = ?");
    let values = assignments
        .into_iter()
        .map(|(_, value)| value)
        .chain([Value::Integer(id), text(expected)]);
    Ok(connection.execute(&sql, params_from_iter(values))? > 0)
}

fn insert_event(
    connection: &Connection,
    row: EventRow<'_>,
    event: &NewSecurityEvent,
    created_at: &str,
) -> Result<()> {
    connection.execute(
        r#"
        INSERT INTO security_events (
            report_id, vulnerability_id, kind, from_state, to_state, actor, detail, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        "#,
        params![
            row.report_id,
            row.vulnerability_id,
            event.kind.as_str(),
            row.from_state,
            row.to_state,
            event.actor,
            serde_json::to_string(&event.detail)?,
            created_at,
        ],
    )?;
    Ok(())
}

fn query_report(connection: &Connection, id: i64) -> Result<Option<SecurityReport>> {
    let sql = format!(
        "SELECT {} FROM security_reports WHERE security_reports.id = ?1",
        select_list(REPORT_COLUMNS, "security_reports")
    );
    Ok(connection
        .query_row(&sql, params![id], |row| row_to_report(row, 0))
        .optional()?)
}

fn query_report_by_conversation(
    connection: &Connection,
    source: &str,
    conversation_id: &str,
) -> Result<Option<SecurityReport>> {
    let sql = format!(
        "SELECT {} FROM security_reports WHERE security_reports.source = ?1 AND security_reports.conversation_id = ?2",
        select_list(REPORT_COLUMNS, "security_reports")
    );
    Ok(connection
        .query_row(&sql, params![source, conversation_id], |row| {
            row_to_report(row, 0)
        })
        .optional()?)
}

fn query_vulnerability(connection: &Connection, id: i64) -> Result<Option<SecurityVulnerability>> {
    let sql = format!(
        "SELECT {} FROM security_vulnerabilities WHERE security_vulnerabilities.id = ?1",
        select_list(VULNERABILITY_COLUMNS, "security_vulnerabilities")
    );
    Ok(connection
        .query_row(&sql, params![id], row_to_vulnerability)
        .optional()?)
}

fn query_award(connection: &Connection, vulnerability_id: i64) -> Result<Option<SecurityAward>> {
    let sql = format!(
        "SELECT {} FROM security_awards WHERE security_awards.vulnerability_id = ?1",
        select_list(AWARD_COLUMNS, "security_awards")
    );
    Ok(connection
        .query_row(&sql, params![vulnerability_id], row_to_award)
        .optional()?)
}

impl SqliteTracker {
    pub fn upsert_security_report(&self, report: &NewSecurityReport) -> Result<SecurityReport> {
        let mut connection = self.acquire_lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let created_at = now();
        let state = SecurityReportState::Received;
        let inserted = transaction.execute(
            r#"
            INSERT INTO security_reports (
                source, conversation_id, short_id, mailbox_id, url, subject, reporter_email,
                reporter_customer_id, reported_at, state, customer_thread_count,
                created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)
            ON CONFLICT(source, conversation_id) DO NOTHING
            "#,
            params![
                report.source,
                report.conversation_id,
                report.short_id,
                report.mailbox_id,
                report.url,
                report.subject,
                report.reporter_email,
                report.reporter_customer_id,
                timestamp(report.reported_at),
                state.as_str(),
                report.customer_thread_count,
                created_at,
            ],
        )?;
        if inserted > 0 {
            insert_event(
                &transaction,
                EventRow {
                    report_id: Some(transaction.last_insert_rowid()),
                    to_state: Some(state.as_str()),
                    ..EventRow::default()
                },
                &NewSecurityEvent::new(SecurityEventKind::Received),
                &created_at,
            )?;
        }
        let stored =
            query_report_by_conversation(&transaction, &report.source, &report.conversation_id)?
                .ok_or_else(|| {
                    Error::storage(format!(
                        "security report {}:{} missing after upsert",
                        report.source, report.conversation_id
                    ))
                })?;
        transaction.commit()?;
        Ok(stored)
    }

    pub fn get_security_report(&self, id: i64) -> Result<Option<SecurityReport>> {
        let connection = self.acquire_lock()?;
        query_report(&connection, id)
    }

    pub fn get_security_report_by_conversation(
        &self,
        source: &str,
        conversation_id: &str,
    ) -> Result<Option<SecurityReport>> {
        let connection = self.acquire_lock()?;
        query_report_by_conversation(&connection, source, conversation_id)
    }

    pub fn list_security_reports(
        &self,
        filter: &SecurityReportFilter,
    ) -> Result<Vec<SecurityReport>> {
        let limit = i64::try_from(filter.limit.unwrap_or(DEFAULT_REPORT_LIMIT)).unwrap_or(i64::MAX);
        let connection = self.acquire_lock()?;
        let sql = format!(
            r#"
            SELECT {} FROM security_reports
            WHERE (?1 IS NULL OR security_reports.state = ?1) AND (?2 IS NULL OR security_reports.outcome = ?2)
            ORDER BY security_reports.reported_at DESC, security_reports.id DESC
            LIMIT ?3
            "#,
            select_list(REPORT_COLUMNS, "security_reports")
        );
        let mut statement = connection.prepare(&sql)?;
        let reports = statement
            .query_map(
                params![
                    filter.state.map(SecurityReportState::as_str),
                    filter.outcome.map(|outcome| outcome.as_str()),
                    limit
                ],
                |row| row_to_report(row, 0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(reports)
    }

    pub fn list_reports_for_vulnerability(
        &self,
        vulnerability_id: i64,
    ) -> Result<Vec<SecurityReport>> {
        let connection = self.acquire_lock()?;
        let sql = format!(
            r#"
            SELECT {} FROM security_reports
            WHERE security_reports.vulnerability_id = ?1
            ORDER BY security_reports.reported_at ASC, security_reports.id ASC
            "#,
            select_list(REPORT_COLUMNS, "security_reports")
        );
        let mut statement = connection.prepare(&sql)?;
        let reports = statement
            .query_map(params![vulnerability_id], |row| row_to_report(row, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(reports)
    }

    pub fn transition_security_report(
        &self,
        id: i64,
        expected: SecurityReportState,
        next: SecurityReportState,
        changes: &SecurityReportChanges,
        event: &NewSecurityEvent,
    ) -> Result<bool> {
        if !expected.can_transition_to(next) {
            return Ok(false);
        }
        let mut connection = self.acquire_lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated_at = now();
        let mut assignments = vec![
            ("state", text(next.as_str())),
            ("updated_at", text(&updated_at)),
        ];
        assignments.extend(report_assignments(changes));
        if !update_in_state(&transaction, REPORTS, id, expected.as_str(), assignments)? {
            return Ok(false);
        }
        insert_event(
            &transaction,
            EventRow {
                report_id: Some(id),
                from_state: Some(expected.as_str()),
                to_state: Some(next.as_str()),
                ..EventRow::default()
            },
            event,
            &updated_at,
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn release_stale_security_assessments(&self, stale_after: Duration) -> Result<usize> {
        let age = TimeDelta::from_std(stale_after).unwrap_or(TimeDelta::MAX);
        let Some(cutoff) = Utc::now().checked_sub_signed(age) else {
            return Ok(0);
        };
        let from = SecurityReportState::Assessing.as_str();
        let to = SecurityReportState::Received.as_str();
        let mut connection = self.acquire_lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "SELECT id FROM security_reports WHERE state = ?1 AND updated_at < ?2 ORDER BY id",
        )?;
        let stale = statement
            .query_map(params![from, timestamp(cutoff)], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let updated_at = now();
        let event = NewSecurityEvent::new(SecurityEventKind::AssessmentReleased)
            .with_detail(json!({ "stale_after_secs": stale_after.as_secs() }));
        for &id in &stale {
            transaction.execute(
                r#"
                UPDATE security_reports
                SET state = ?1, assessment_attempts = assessment_attempts + 1, updated_at = ?2
                WHERE id = ?3 AND state = ?4
                "#,
                params![to, updated_at, id, from],
            )?;
            insert_event(
                &transaction,
                EventRow {
                    report_id: Some(id),
                    from_state: Some(from),
                    to_state: Some(to),
                    ..EventRow::default()
                },
                &event,
                &updated_at,
            )?;
        }
        transaction.commit()?;
        Ok(stale.len())
    }

    pub fn record_security_follow_up(
        &self,
        id: i64,
        expected_count: u32,
        new_count: u32,
    ) -> Result<bool> {
        if new_count <= expected_count {
            return Ok(false);
        }
        let mut connection = self.acquire_lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated_at = now();
        let changed = transaction.execute(
            r#"
            UPDATE security_reports SET customer_thread_count = ?1, updated_at = ?2
            WHERE id = ?3 AND customer_thread_count = ?4
            "#,
            params![new_count, updated_at, id, expected_count],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        insert_event(
            &transaction,
            EventRow {
                report_id: Some(id),
                ..EventRow::default()
            },
            &NewSecurityEvent::new(SecurityEventKind::ReporterFollowUp).with_detail(json!({
                "previous_count": expected_count,
                "customer_thread_count": new_count,
            })),
            &updated_at,
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn record_security_event(
        &self,
        report_id: Option<i64>,
        vulnerability_id: Option<i64>,
        event: &NewSecurityEvent,
    ) -> Result<()> {
        if report_id.is_none() && vulnerability_id.is_none() {
            return Err(Error::storage(
                "a security event needs a report or a vulnerability",
            ));
        }
        let connection = self.acquire_lock()?;
        insert_event(
            &connection,
            EventRow {
                report_id,
                vulnerability_id,
                ..EventRow::default()
            },
            event,
            &now(),
        )
    }

    pub fn list_security_timeline(&self, report_id: i64) -> Result<Vec<SecurityEvent>> {
        let connection = self.acquire_lock()?;
        let sql = format!(
            r#"
            SELECT {} FROM security_events
            WHERE security_events.report_id = ?1
               OR security_events.vulnerability_id = (SELECT vulnerability_id FROM security_reports WHERE id = ?1)
            ORDER BY security_events.created_at ASC, security_events.id ASC
            "#,
            select_list(EVENT_COLUMNS, "security_events")
        );
        let mut statement = connection.prepare(&sql)?;
        let events = statement
            .query_map(params![report_id], |row| row_to_event(row, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(events)
    }

    pub fn last_security_event(
        &self,
        vulnerability_id: i64,
        kind: SecurityEventKind,
    ) -> Result<Option<SecurityEvent>> {
        let connection = self.acquire_lock()?;
        let sql = format!(
            r#"
            SELECT {} FROM security_events
            WHERE security_events.vulnerability_id = ?1 AND security_events.kind = ?2
            ORDER BY security_events.created_at DESC, security_events.id DESC
            LIMIT 1
            "#,
            select_list(EVENT_COLUMNS, "security_events")
        );
        Ok(connection
            .query_row(&sql, params![vulnerability_id, kind.as_str()], |row| {
                row_to_event(row, 0)
            })
            .optional()?)
    }

    pub fn list_admin_confirmed_reports(&self) -> Result<Vec<(SecurityReport, SecurityEvent)>> {
        let connection = self.acquire_lock()?;
        let sql = format!(
            r#"
            SELECT {}, {} FROM security_reports
            JOIN security_events
              ON security_events.id = (
                  SELECT MAX(latest.id) FROM security_events AS latest
                  WHERE latest.report_id = security_reports.id
              )
            WHERE security_reports.state = ?1 AND security_events.kind = ?2
            ORDER BY security_events.id ASC
            "#,
            select_list(REPORT_COLUMNS, "security_reports"),
            select_list(EVENT_COLUMNS, "security_events")
        );
        let mut statement = connection.prepare(&sql)?;
        let confirmed = statement
            .query_map(
                params![
                    SecurityReportState::Assessing.as_str(),
                    SecurityEventKind::AdminConfirmed.as_str()
                ],
                |row| {
                    Ok((
                        row_to_report(row, 0)?,
                        row_to_event(row, REPORT_COLUMNS.len())?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(confirmed)
    }

    pub fn list_security_reports_without_event(
        &self,
        state: SecurityReportState,
        kind: SecurityEventKind,
    ) -> Result<Vec<SecurityReport>> {
        let connection = self.acquire_lock()?;
        let sql = format!(
            r#"
            SELECT {} FROM security_reports
            WHERE security_reports.state = ?1
              AND NOT EXISTS (
                  SELECT 1 FROM security_events
                  WHERE security_events.report_id = security_reports.id AND security_events.kind = ?2
              )
            ORDER BY security_reports.reported_at ASC, security_reports.id ASC
            "#,
            select_list(REPORT_COLUMNS, "security_reports")
        );
        let mut statement = connection.prepare(&sql)?;
        let reports = statement
            .query_map(params![state.as_str(), kind.as_str()], |row| {
                row_to_report(row, 0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(reports)
    }

    pub fn create_security_vulnerability(
        &self,
        vulnerability: &NewSecurityVulnerability,
    ) -> Result<SecurityVulnerability> {
        if !vulnerability.state.is_initial() {
            return Err(Error::storage(format!(
                "a vulnerability cannot start in {}",
                vulnerability.state
            )));
        }
        let embedding =
            (!vulnerability.embedding.is_empty()).then(|| embedding_blob(&vulnerability.embedding));
        let mut connection = self.acquire_lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let created_at = now();
        transaction.execute(
            r#"
            INSERT INTO security_vulnerabilities (
                title, summary, embedding, repository, severity, state, first_report_id,
                first_reported_at, merge_commit, merged_at, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
            "#,
            params![
                vulnerability.title,
                vulnerability.summary,
                embedding,
                vulnerability.repository,
                vulnerability.severity.map(|severity| severity.as_str()),
                vulnerability.state.as_str(),
                vulnerability.first_report_id,
                timestamp(vulnerability.first_reported_at),
                vulnerability.merge_commit,
                vulnerability.merged_at.map(timestamp),
                created_at,
            ],
        )?;
        let id = transaction.last_insert_rowid();
        insert_event(
            &transaction,
            EventRow {
                vulnerability_id: Some(id),
                to_state: Some(vulnerability.state.as_str()),
                ..EventRow::default()
            },
            &NewSecurityEvent::new(SecurityEventKind::VulnerabilityCreated)
                .with_detail(json!({ "first_report_id": vulnerability.first_report_id })),
            &created_at,
        )?;
        let created = query_vulnerability(&transaction, id)?.ok_or_else(|| {
            Error::storage(format!("security vulnerability {id} missing after insert"))
        })?;
        transaction.commit()?;
        Ok(created)
    }

    pub fn get_security_vulnerability(&self, id: i64) -> Result<Option<SecurityVulnerability>> {
        let connection = self.acquire_lock()?;
        query_vulnerability(&connection, id)
    }

    pub fn list_security_vulnerabilities(
        &self,
        state: Option<VulnerabilityState>,
    ) -> Result<Vec<SecurityVulnerability>> {
        let connection = self.acquire_lock()?;
        let sql = format!(
            r#"
            SELECT {} FROM security_vulnerabilities
            WHERE ?1 IS NULL OR security_vulnerabilities.state = ?1
            ORDER BY security_vulnerabilities.created_at ASC, security_vulnerabilities.id ASC
            "#,
            select_list(VULNERABILITY_COLUMNS, "security_vulnerabilities")
        );
        let mut statement = connection.prepare(&sql)?;
        let vulnerabilities = statement
            .query_map(
                params![state.map(VulnerabilityState::as_str)],
                row_to_vulnerability,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(vulnerabilities)
    }

    pub fn transition_security_vulnerability(
        &self,
        id: i64,
        expected: VulnerabilityState,
        next: VulnerabilityState,
        changes: &SecurityVulnerabilityChanges,
        event: &NewSecurityEvent,
    ) -> Result<bool> {
        if !expected.can_transition_to(next) {
            return Ok(false);
        }
        let mut connection = self.acquire_lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated_at = now();
        let mut assignments = vec![
            ("state", text(next.as_str())),
            ("updated_at", text(&updated_at)),
        ];
        assignments.extend(vulnerability_assignments(changes));
        if !update_in_state(
            &transaction,
            VULNERABILITIES,
            id,
            expected.as_str(),
            assignments,
        )? {
            return Ok(false);
        }
        insert_event(
            &transaction,
            EventRow {
                vulnerability_id: Some(id),
                from_state: Some(expected.as_str()),
                to_state: Some(next.as_str()),
                ..EventRow::default()
            },
            event,
            &updated_at,
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn replace_first_reporter(
        &self,
        vulnerability_id: i64,
        expected_first_report_id: i64,
        report_id: i64,
        reported_at: DateTime<Utc>,
    ) -> Result<bool> {
        if report_id == expected_first_report_id {
            return Ok(false);
        }
        let mut connection = self.acquire_lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated_at = now();
        let replaced = transaction.execute(
            r#"
            UPDATE security_vulnerabilities
            SET first_report_id = ?1, first_reported_at = ?2, updated_at = ?3
            WHERE id = ?4 AND first_report_id = ?5 AND first_reported_at > ?2
              AND NOT EXISTS (SELECT 1 FROM security_awards WHERE vulnerability_id = ?4)
            "#,
            params![
                report_id,
                timestamp(reported_at),
                updated_at,
                vulnerability_id,
                expected_first_report_id
            ],
        )?;
        if replaced == 0 {
            return Ok(false);
        }
        insert_event(
            &transaction,
            EventRow {
                vulnerability_id: Some(vulnerability_id),
                ..EventRow::default()
            },
            &NewSecurityEvent::new(SecurityEventKind::FirstReporterReplaced).with_detail(json!({
                "previous_report_id": expected_first_report_id,
                "report_id": report_id,
            })),
            &updated_at,
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn claim_security_reply(&self, report_id: i64, kind: SecurityReplyKind) -> Result<bool> {
        let connection = self.acquire_lock()?;
        let claimed = connection.execute(
            r#"
            INSERT INTO security_replies (report_id, kind, status, claimed_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(report_id, kind) DO UPDATE SET
                status = excluded.status,
                error = NULL,
                claimed_at = excluded.claimed_at,
                completed_at = NULL
            WHERE security_replies.status = ?5
            "#,
            params![
                report_id,
                kind.as_str(),
                DeliveryStatus::Sending.as_str(),
                now(),
                DeliveryStatus::Failed.as_str()
            ],
        )?;
        Ok(claimed > 0)
    }

    pub fn complete_security_reply(
        &self,
        report_id: i64,
        kind: SecurityReplyKind,
        status: DeliveryStatus,
        error: Option<&str>,
    ) -> Result<()> {
        if !status.is_completion() {
            return Err(Error::storage(format!(
                "a reply cannot complete as {status}"
            )));
        }
        let connection = self.acquire_lock()?;
        let completed = connection.execute(
            r#"
            UPDATE security_replies SET status = ?1, error = ?2, completed_at = ?3
            WHERE report_id = ?4 AND kind = ?5 AND status = ?6
            "#,
            params![
                status.as_str(),
                error,
                now(),
                report_id,
                kind.as_str(),
                DeliveryStatus::Sending.as_str()
            ],
        )?;
        if completed == 0 {
            return Err(Error::storage(format!(
                "no {kind} reply is being sent for security report {report_id}"
            )));
        }
        Ok(())
    }

    pub fn confirm_security_reply(
        &self,
        report_id: i64,
        kind: SecurityReplyKind,
        delivered: bool,
        actor: &str,
    ) -> Result<bool> {
        let (status, error) = if delivered {
            (DeliveryStatus::Sent, None)
        } else {
            (DeliveryStatus::Failed, Some(NOT_DELIVERED_ERROR))
        };
        let from = DeliveryStatus::Unconfirmed.as_str();
        let mut connection = self.acquire_lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let completed_at = now();
        let confirmed = transaction.execute(
            r#"
            UPDATE security_replies SET status = ?1, error = ?2, completed_at = ?3
            WHERE report_id = ?4 AND kind = ?5 AND status = ?6
            "#,
            params![
                status.as_str(),
                error,
                completed_at,
                report_id,
                kind.as_str(),
                from
            ],
        )?;
        if confirmed == 0 {
            return Ok(false);
        }
        insert_event(
            &transaction,
            EventRow {
                report_id: Some(report_id),
                from_state: Some(from),
                to_state: Some(status.as_str()),
                ..EventRow::default()
            },
            &NewSecurityEvent::new(SecurityEventKind::DeliveryConfirmed)
                .with_actor(actor)
                .with_detail(json!({ "kind": kind, "delivered": delivered })),
            &completed_at,
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn list_security_replies(&self, report_id: i64) -> Result<Vec<SecurityReply>> {
        let connection = self.acquire_lock()?;
        let sql = format!(
            "SELECT {} FROM security_replies WHERE security_replies.report_id = ?1 ORDER BY security_replies.claimed_at, security_replies.id",
            select_list(REPLY_COLUMNS, "security_replies")
        );
        let mut statement = connection.prepare(&sql)?;
        let replies = statement
            .query_map(params![report_id], row_to_reply)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(replies)
    }

    pub fn prepare_security_award(
        &self,
        vulnerability_id: i64,
        report_id: i64,
        award: &NewSecurityAward,
    ) -> Result<bool> {
        let status = AwardStatus::Prepared;
        let mut connection = self.acquire_lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prepared_at = now();
        let prepared = transaction.execute(
            r#"
            INSERT INTO security_awards (
                vulnerability_id, report_id, recipient_email, campaign, credits, status,
                prepared_at, updated_at
            )
            SELECT id, ?2, ?3, ?4, ?5, ?6, ?7, ?7 FROM security_vulnerabilities
            WHERE id = ?1 AND first_report_id = ?2
            ON CONFLICT(vulnerability_id) DO NOTHING
            "#,
            params![
                vulnerability_id,
                report_id,
                award.recipient_email,
                award.campaign,
                award.credits,
                status.as_str(),
                prepared_at
            ],
        )?;
        if prepared == 0 {
            return Ok(false);
        }
        insert_event(
            &transaction,
            EventRow {
                vulnerability_id: Some(vulnerability_id),
                to_state: Some(status.as_str()),
                ..EventRow::default()
            },
            &NewSecurityEvent::new(status.event_kind()).with_detail(json!({
                "report_id": report_id,
                "campaign": award.campaign,
                "credits": award.credits,
            })),
            &prepared_at,
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn update_security_award_status(
        &self,
        vulnerability_id: i64,
        expected: &[AwardStatus],
        next: AwardStatus,
        actor: Option<&str>,
        note: Option<&str>,
    ) -> Result<bool> {
        let mut connection = self.acquire_lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(award) = query_award(&transaction, vulnerability_id)? else {
            return Ok(false);
        };
        let current = award.status;
        if !expected.contains(&current) || !current.can_transition_to(next) {
            return Ok(false);
        }
        let updated_at = now();
        let updated = transaction.execute(
            r#"
            UPDATE security_awards
            SET status = ?1, actor = COALESCE(?2, actor), note = COALESCE(?3, note), updated_at = ?4
            WHERE vulnerability_id = ?5 AND status = ?6
            "#,
            params![
                next.as_str(),
                actor,
                note,
                updated_at,
                vulnerability_id,
                current.as_str()
            ],
        )?;
        if updated == 0 {
            return Ok(false);
        }
        let mut event = NewSecurityEvent::new(next.event_kind())
            .with_detail(json!({ "report_id": award.report_id }));
        event.actor = actor.map(str::to_string);
        insert_event(
            &transaction,
            EventRow {
                vulnerability_id: Some(vulnerability_id),
                from_state: Some(current.as_str()),
                to_state: Some(next.as_str()),
                ..EventRow::default()
            },
            &event,
            &updated_at,
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn get_security_award(&self, vulnerability_id: i64) -> Result<Option<SecurityAward>> {
        let connection = self.acquire_lock()?;
        query_award(&connection, vulnerability_id)
    }

    pub fn list_security_awards(&self, status: Option<AwardStatus>) -> Result<Vec<SecurityAward>> {
        let connection = self.acquire_lock()?;
        let sql = format!(
            r#"
            SELECT {} FROM security_awards
            WHERE ?1 IS NULL OR security_awards.status = ?1
            ORDER BY security_awards.prepared_at ASC, security_awards.id ASC
            "#,
            select_list(AWARD_COLUMNS, "security_awards")
        );
        let mut statement = connection.prepare(&sql)?;
        let awards = statement
            .query_map(params![status.map(AwardStatus::as_str)], row_to_award)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(awards)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::{Arc, Barrier};

    const RFC3339_SECONDS: &str = r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$";

    fn tracker() -> SqliteTracker {
        SqliteTracker::in_memory().unwrap()
    }

    fn reported_at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 9, minute, 0).unwrap()
    }

    fn new_report(conversation_id: &str, reported_at: DateTime<Utc>) -> NewSecurityReport {
        NewSecurityReport {
            source: "helpscout".to_string(),
            conversation_id: conversation_id.to_string(),
            short_id: format!("HS-{conversation_id}"),
            mailbox_id: "249891".to_string(),
            url: format!("https://secure.helpscout.net/conversation/{conversation_id}"),
            subject: "Vulnerability report".to_string(),
            reporter_email: Some("researcher@example.com".to_string()),
            reporter_customer_id: Some("42".to_string()),
            reported_at,
            customer_thread_count: 1,
        }
    }

    fn new_vulnerability(first_report: &SecurityReport) -> NewSecurityVulnerability {
        NewSecurityVulnerability {
            title: "Session fixation".to_string(),
            summary: "Sessions survive logout.".to_string(),
            embedding: vec![0.25, -0.5, 1.0],
            repository: Some("appwrite/appwrite".to_string()),
            severity: Some(claudear_core::security::SecuritySeverity::High),
            state: VulnerabilityState::AwaitingFix,
            first_report_id: first_report.id,
            first_reported_at: first_report.reported_at,
            merge_commit: None,
            merged_at: None,
        }
    }

    fn new_award() -> NewSecurityAward {
        NewSecurityAward {
            recipient_email: Some("researcher@example.com".to_string()),
            campaign: "SecurityResearchers".to_string(),
            credits: 100,
        }
    }

    fn event(kind: SecurityEventKind) -> NewSecurityEvent {
        NewSecurityEvent::new(kind)
    }

    fn event_count(tracker: &SqliteTracker) -> i64 {
        let connection = tracker.acquire_lock().unwrap();
        connection
            .query_row("SELECT COUNT(*) FROM security_events", [], |row| row.get(0))
            .unwrap()
    }

    fn execute(tracker: &SqliteTracker, sql: &str, values: impl rusqlite::Params) {
        let connection = tracker.acquire_lock().unwrap();
        connection.execute(sql, values).unwrap();
    }

    fn kinds(events: &[SecurityEvent]) -> Vec<SecurityEventKind> {
        events.iter().map(|event| event.kind).collect()
    }

    fn assess(tracker: &SqliteTracker, report_id: i64) {
        assert!(tracker
            .transition_security_report(
                report_id,
                SecurityReportState::Received,
                SecurityReportState::Assessing,
                &SecurityReportChanges::default(),
                &event(SecurityEventKind::AssessmentStarted),
            )
            .unwrap());
    }

    fn track(tracker: &SqliteTracker, report_id: i64, vulnerability_id: i64) {
        assert!(tracker
            .transition_security_report(
                report_id,
                SecurityReportState::Assessing,
                SecurityReportState::Tracking,
                &SecurityReportChanges {
                    outcome: Some(claudear_core::security::SecurityOutcome::Valid),
                    vulnerability_id: Some(vulnerability_id),
                    ..SecurityReportChanges::default()
                },
                &event(SecurityEventKind::Assessed),
            )
            .unwrap());
    }

    #[test]
    fn upsert_creates_a_received_report_with_its_event() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("100", reported_at(0)))
            .unwrap();

        assert_eq!(report.state, SecurityReportState::Received);
        assert_eq!(report.conversation_id, "100");
        assert_eq!(report.reported_at, reported_at(0));
        assert_eq!(report.customer_thread_count, 1);
        assert_eq!(report.assessment_attempts, 0);
        assert_eq!(report.vulnerability_id, None);

        let timeline = tracker.list_security_timeline(report.id).unwrap();
        assert_eq!(kinds(&timeline), vec![SecurityEventKind::Received]);
        assert_eq!(timeline[0].from_state, None);
        assert_eq!(timeline[0].to_state.as_deref(), Some("received"));
        assert_eq!(timeline[0].detail, json!({}));
    }

    #[test]
    fn upsert_is_idempotent_and_returns_the_existing_row_unchanged() {
        let tracker = tracker();
        let first = tracker
            .upsert_security_report(&new_report("100", reported_at(0)))
            .unwrap();
        assess(&tracker, first.id);

        let mut changed = new_report("100", reported_at(30));
        changed.subject = "Different subject".to_string();
        changed.customer_thread_count = 9;
        let second = tracker.upsert_security_report(&changed).unwrap();

        assert_eq!(second.id, first.id);
        assert_eq!(second.subject, first.subject);
        assert_eq!(second.reported_at, first.reported_at);
        assert_eq!(second.customer_thread_count, 1);
        assert_eq!(second.state, SecurityReportState::Assessing);
        assert_eq!(event_count(&tracker), 2);

        let other_source = NewSecurityReport {
            source: "other".to_string(),
            ..new_report("100", reported_at(0))
        };
        assert_ne!(
            tracker.upsert_security_report(&other_source).unwrap().id,
            first.id
        );
    }

    #[test]
    fn lookups_by_id_and_conversation() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("100", reported_at(0)))
            .unwrap();

        assert_eq!(
            tracker.get_security_report(report.id).unwrap(),
            Some(report.clone())
        );
        assert_eq!(
            tracker
                .get_security_report_by_conversation("helpscout", "100")
                .unwrap(),
            Some(report)
        );
        assert_eq!(tracker.get_security_report(999).unwrap(), None);
        assert_eq!(
            tracker
                .get_security_report_by_conversation("helpscout", "999")
                .unwrap(),
            None
        );
    }

    #[test]
    fn timestamps_round_trip_in_rfc3339_z_format() {
        let tracker = tracker();
        let precise = reported_at(5) + TimeDelta::milliseconds(750);
        let report = tracker
            .upsert_security_report(&new_report("100", precise))
            .unwrap();
        assert_eq!(report.reported_at, reported_at(5));

        let fixed_on = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        assess(&tracker, report.id);
        assert!(tracker
            .transition_security_report(
                report.id,
                SecurityReportState::Assessing,
                SecurityReportState::Replying,
                &SecurityReportChanges {
                    fixed_on: Some(fixed_on),
                    ..SecurityReportChanges::default()
                },
                &event(SecurityEventKind::Assessed),
            )
            .unwrap());
        let report = tracker.get_security_report(report.id).unwrap().unwrap();
        assert_eq!(report.fixed_on, Some(fixed_on));

        let vulnerability = tracker
            .create_security_vulnerability(&NewSecurityVulnerability {
                state: VulnerabilityState::Merged,
                merge_commit: Some("a".repeat(40)),
                merged_at: Some(reported_at(7)),
                ..new_vulnerability(&report)
            })
            .unwrap();
        assert!(tracker
            .transition_security_vulnerability(
                vulnerability.id,
                VulnerabilityState::Merged,
                VulnerabilityState::Live,
                &SecurityVulnerabilityChanges {
                    live_at: Some(reported_at(9)),
                    deployed_version: Some("1.53.30".to_string()),
                    ..SecurityVulnerabilityChanges::default()
                },
                &event(SecurityEventKind::Live),
            )
            .unwrap());
        let vulnerability = tracker
            .get_security_vulnerability(vulnerability.id)
            .unwrap()
            .unwrap();
        assert_eq!(vulnerability.merged_at, Some(reported_at(7)));
        assert_eq!(vulnerability.live_at, Some(reported_at(9)));
        assert_eq!(vulnerability.first_reported_at, reported_at(5));

        tracker
            .prepare_security_award(vulnerability.id, report.id, &new_award())
            .unwrap();
        assert!(tracker
            .claim_security_reply(report.id, SecurityReplyKind::Confirmed)
            .unwrap());
        tracker
            .complete_security_reply(
                report.id,
                SecurityReplyKind::Confirmed,
                DeliveryStatus::Sent,
                None,
            )
            .unwrap();

        let pattern = regex_lite::Regex::new(RFC3339_SECONDS).unwrap();
        let connection = tracker.acquire_lock().unwrap();
        let mut values: Vec<String> = Vec::new();
        for (table, columns) in [
            (
                "security_reports",
                "reported_at || '|' || created_at || '|' || updated_at",
            ),
            (
                "security_vulnerabilities",
                "first_reported_at || '|' || merged_at || '|' || live_at || '|' || created_at || '|' || updated_at",
            ),
            ("security_events", "created_at"),
            ("security_replies", "claimed_at || '|' || completed_at"),
            ("security_awards", "prepared_at || '|' || updated_at"),
        ] {
            let mut statement = connection
                .prepare(&format!("SELECT {columns} FROM {table}"))
                .unwrap();
            let rows: Vec<String> = statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(!rows.is_empty(), "{table} has no rows");
            values.extend(rows.iter().flat_map(|row| row.split('|').map(str::to_string)));
        }
        for value in values {
            assert!(pattern.is_match(&value), "{value} is not RFC3339 Z");
        }
        let fixed_on_text: String = connection
            .query_row("SELECT fixed_on FROM security_reports", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fixed_on_text, "2026-10-01");
    }

    #[test]
    fn transition_applies_only_the_given_changes_and_records_the_edge() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("100", reported_at(0)))
            .unwrap();
        assert!(tracker
            .transition_security_report(
                report.id,
                SecurityReportState::Received,
                SecurityReportState::Assessing,
                &SecurityReportChanges {
                    assessment_summary: Some("Initial summary".to_string()),
                    spam_confidence: Some(0.1),
                    ..SecurityReportChanges::default()
                },
                &event(SecurityEventKind::AssessmentStarted)
                    .with_actor("claudear")
                    .with_detail(json!({ "attempt": 1 })),
            )
            .unwrap());
        assert!(tracker
            .transition_security_report(
                report.id,
                SecurityReportState::Assessing,
                SecurityReportState::Replying,
                &SecurityReportChanges {
                    outcome: Some(claudear_core::security::SecurityOutcome::OutOfScope),
                    public_reason: Some("Self-XSS.".to_string()),
                    affected_repository: Some("appwrite/appwrite".to_string()),
                    fixed_by_commit: Some("abcdef1".to_string()),
                    assessment_attempts: Some(2),
                    ..SecurityReportChanges::default()
                },
                &event(SecurityEventKind::Assessed),
            )
            .unwrap());

        let stored = tracker.get_security_report(report.id).unwrap().unwrap();
        assert_eq!(stored.state, SecurityReportState::Replying);
        assert_eq!(
            stored.assessment_summary.as_deref(),
            Some("Initial summary")
        );
        assert_eq!(stored.spam_confidence, Some(0.1));
        assert_eq!(
            stored.outcome,
            Some(claudear_core::security::SecurityOutcome::OutOfScope)
        );
        assert_eq!(stored.public_reason.as_deref(), Some("Self-XSS."));
        assert_eq!(
            stored.affected_repository.as_deref(),
            Some("appwrite/appwrite")
        );
        assert_eq!(stored.fixed_by_commit.as_deref(), Some("abcdef1"));
        assert_eq!(stored.assessment_attempts, 2);
        assert_eq!(stored.subject, report.subject);

        let timeline = tracker.list_security_timeline(report.id).unwrap();
        assert_eq!(
            kinds(&timeline),
            vec![
                SecurityEventKind::Received,
                SecurityEventKind::AssessmentStarted,
                SecurityEventKind::Assessed,
            ]
        );
        let started = &timeline[1];
        assert_eq!(started.report_id, Some(report.id));
        assert_eq!(started.vulnerability_id, None);
        assert_eq!(started.from_state.as_deref(), Some("received"));
        assert_eq!(started.to_state.as_deref(), Some("assessing"));
        assert_eq!(started.actor.as_deref(), Some("claudear"));
        assert_eq!(started.detail, json!({ "attempt": 1 }));
    }

    #[test]
    fn transition_refuses_a_disallowed_edge_without_writing() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("100", reported_at(0)))
            .unwrap();
        let before = event_count(&tracker);

        for next in [
            SecurityReportState::Closed,
            SecurityReportState::Resolved,
            SecurityReportState::Tracking,
            SecurityReportState::Received,
        ] {
            assert!(!tracker
                .transition_security_report(
                    report.id,
                    SecurityReportState::Received,
                    next,
                    &SecurityReportChanges {
                        outcome: Some(claudear_core::security::SecurityOutcome::Valid),
                        ..SecurityReportChanges::default()
                    },
                    &event(SecurityEventKind::Overridden),
                )
                .unwrap());
        }

        assert_eq!(
            tracker.get_security_report(report.id).unwrap().unwrap(),
            report
        );
        assert_eq!(event_count(&tracker), before);
    }

    #[test]
    fn transition_refuses_a_stale_expected_state_without_writing() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("100", reported_at(0)))
            .unwrap();
        let before = event_count(&tracker);

        assert!(!tracker
            .transition_security_report(
                report.id,
                SecurityReportState::Assessing,
                SecurityReportState::Replying,
                &SecurityReportChanges::default(),
                &event(SecurityEventKind::Assessed),
            )
            .unwrap());
        assert!(!tracker
            .transition_security_report(
                999,
                SecurityReportState::Received,
                SecurityReportState::Assessing,
                &SecurityReportChanges::default(),
                &event(SecurityEventKind::AssessmentStarted),
            )
            .unwrap());

        assert_eq!(
            tracker.get_security_report(report.id).unwrap().unwrap(),
            report
        );
        assert_eq!(event_count(&tracker), before);
    }

    #[test]
    fn transition_with_an_unknown_vulnerability_fails_and_writes_nothing() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("100", reported_at(0)))
            .unwrap();
        assess(&tracker, report.id);
        let before = event_count(&tracker);

        assert!(tracker
            .transition_security_report(
                report.id,
                SecurityReportState::Assessing,
                SecurityReportState::Tracking,
                &SecurityReportChanges {
                    vulnerability_id: Some(999),
                    ..SecurityReportChanges::default()
                },
                &event(SecurityEventKind::Assessed),
            )
            .is_err());

        let stored = tracker.get_security_report(report.id).unwrap().unwrap();
        assert_eq!(stored.state, SecurityReportState::Assessing);
        assert_eq!(event_count(&tracker), before);
    }

    #[test]
    fn racing_transitions_on_separate_connections_produce_one_winner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("security.db");
        let trackers = [
            Arc::new(SqliteTracker::new(&path).unwrap()),
            Arc::new(SqliteTracker::new(&path).unwrap()),
        ];
        let ids: Vec<i64> = (0..20)
            .map(|index| {
                trackers[0]
                    .upsert_security_report(&new_report(&index.to_string(), reported_at(0)))
                    .unwrap()
                    .id
            })
            .collect();

        for &id in &ids {
            let barrier = Arc::new(Barrier::new(trackers.len()));
            let winners: usize = std::thread::scope(|scope| {
                let handles: Vec<_> = trackers
                    .iter()
                    .map(|tracker| {
                        let barrier = Arc::clone(&barrier);
                        scope.spawn(move || {
                            barrier.wait();
                            tracker
                                .transition_security_report(
                                    id,
                                    SecurityReportState::Received,
                                    SecurityReportState::Assessing,
                                    &SecurityReportChanges::default(),
                                    &event(SecurityEventKind::AssessmentStarted),
                                )
                                .unwrap()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| usize::from(handle.join().unwrap()))
                    .sum()
            });
            assert_eq!(winners, 1, "report {id}");
            let started = trackers[1]
                .list_security_timeline(id)
                .unwrap()
                .into_iter()
                .filter(|event| event.kind == SecurityEventKind::AssessmentStarted)
                .count();
            assert_eq!(started, 1, "report {id}");
        }
    }

    #[test]
    fn list_security_reports_filters_orders_and_limits() {
        let tracker = tracker();
        let oldest = tracker
            .upsert_security_report(&new_report("1", reported_at(1)))
            .unwrap();
        let newest = tracker
            .upsert_security_report(&new_report("3", reported_at(3)))
            .unwrap();
        let middle = tracker
            .upsert_security_report(&new_report("2", reported_at(2)))
            .unwrap();
        assess(&tracker, middle.id);
        assert!(tracker
            .transition_security_report(
                middle.id,
                SecurityReportState::Assessing,
                SecurityReportState::Spam,
                &SecurityReportChanges {
                    outcome: Some(claudear_core::security::SecurityOutcome::Spam),
                    ..SecurityReportChanges::default()
                },
                &event(SecurityEventKind::ScreenedSpam),
            )
            .unwrap());

        let ids = |filter: SecurityReportFilter| -> Vec<i64> {
            tracker
                .list_security_reports(&filter)
                .unwrap()
                .into_iter()
                .map(|report| report.id)
                .collect()
        };
        assert_eq!(
            ids(SecurityReportFilter::default()),
            vec![newest.id, middle.id, oldest.id]
        );
        assert_eq!(
            ids(SecurityReportFilter {
                state: Some(SecurityReportState::Received),
                ..SecurityReportFilter::default()
            }),
            vec![newest.id, oldest.id]
        );
        assert_eq!(
            ids(SecurityReportFilter {
                outcome: Some(claudear_core::security::SecurityOutcome::Spam),
                ..SecurityReportFilter::default()
            }),
            vec![middle.id]
        );
        assert_eq!(
            ids(SecurityReportFilter {
                limit: Some(2),
                ..SecurityReportFilter::default()
            }),
            vec![newest.id, middle.id]
        );
    }

    #[test]
    fn list_security_reports_defaults_to_one_hundred() {
        let tracker = tracker();
        for index in 0..(DEFAULT_REPORT_LIMIT + 5) {
            tracker
                .upsert_security_report(&new_report(&index.to_string(), reported_at(0)))
                .unwrap();
        }
        assert_eq!(
            tracker
                .list_security_reports(&SecurityReportFilter::default())
                .unwrap()
                .len(),
            DEFAULT_REPORT_LIMIT
        );
    }

    #[test]
    fn release_stale_assessments_uses_its_cutoff_and_counts_the_attempt() {
        let tracker = tracker();
        let stale = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let fresh = tracker
            .upsert_security_report(&new_report("2", reported_at(0)))
            .unwrap();
        let received = tracker
            .upsert_security_report(&new_report("3", reported_at(0)))
            .unwrap();
        assess(&tracker, stale.id);
        assess(&tracker, fresh.id);
        let two_hours_ago = timestamp(Utc::now() - TimeDelta::hours(2));
        execute(
            &tracker,
            "UPDATE security_reports SET updated_at = ?1 WHERE id IN (?2, ?3)",
            params![two_hours_ago, stale.id, received.id],
        );
        execute(
            &tracker,
            "UPDATE security_reports SET updated_at = ?1 WHERE id = ?2",
            params![timestamp(Utc::now() - TimeDelta::minutes(30)), fresh.id],
        );

        assert_eq!(
            tracker
                .release_stale_security_assessments(Duration::from_secs(3 * 3600))
                .unwrap(),
            0
        );
        assert_eq!(
            tracker
                .release_stale_security_assessments(Duration::from_secs(3600))
                .unwrap(),
            1
        );

        let released = tracker.get_security_report(stale.id).unwrap().unwrap();
        assert_eq!(released.state, SecurityReportState::Received);
        assert_eq!(released.assessment_attempts, 1);
        let fresh = tracker.get_security_report(fresh.id).unwrap().unwrap();
        assert_eq!(fresh.state, SecurityReportState::Assessing);
        assert_eq!(fresh.assessment_attempts, 0);
        let received = tracker.get_security_report(received.id).unwrap().unwrap();
        assert_eq!(received.assessment_attempts, 0);

        let last = tracker
            .list_security_timeline(stale.id)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(last.kind, SecurityEventKind::AssessmentReleased);
        assert_eq!(last.from_state.as_deref(), Some("assessing"));
        assert_eq!(last.to_state.as_deref(), Some("received"));

        assert_eq!(
            tracker
                .release_stale_security_assessments(Duration::from_secs(3600))
                .unwrap(),
            0
        );
        assert_eq!(
            tracker
                .release_stale_security_assessments(Duration::MAX)
                .unwrap(),
            0
        );
    }

    #[test]
    fn follow_up_is_a_compare_and_set_on_the_thread_count() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();

        assert!(tracker.record_security_follow_up(report.id, 1, 2).unwrap());
        assert!(!tracker.record_security_follow_up(report.id, 1, 3).unwrap());
        assert!(!tracker.record_security_follow_up(report.id, 2, 2).unwrap());
        assert!(!tracker.record_security_follow_up(report.id, 2, 1).unwrap());
        assert!(tracker.record_security_follow_up(report.id, 2, 4).unwrap());
        assert!(!tracker.record_security_follow_up(999, 0, 1).unwrap());

        let stored = tracker.get_security_report(report.id).unwrap().unwrap();
        assert_eq!(stored.customer_thread_count, 4);
        let follow_ups: Vec<SecurityEvent> = tracker
            .list_security_timeline(report.id)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == SecurityEventKind::ReporterFollowUp)
            .collect();
        assert_eq!(follow_ups.len(), 2);
        assert_eq!(
            follow_ups[1].detail,
            json!({ "previous_count": 2, "customer_thread_count": 4 })
        );
    }

    #[test]
    fn record_security_event_requires_a_subject() {
        let tracker = tracker();
        assert!(tracker
            .record_security_event(None, None, &event(SecurityEventKind::NeedsHuman))
            .is_err());
        assert_eq!(event_count(&tracker), 0);
    }

    #[test]
    fn timeline_merges_report_and_vulnerability_events_in_time_order() {
        let tracker = tracker();
        let first = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let unrelated = tracker
            .upsert_security_report(&new_report("2", reported_at(1)))
            .unwrap();
        assess(&tracker, first.id);
        let vulnerability = tracker
            .create_security_vulnerability(&new_vulnerability(&first))
            .unwrap();
        track(&tracker, first.id, vulnerability.id);
        tracker
            .record_security_event(
                None,
                Some(vulnerability.id),
                &event(SecurityEventKind::FixStarted),
            )
            .unwrap();
        tracker
            .record_security_event(
                Some(unrelated.id),
                None,
                &event(SecurityEventKind::NeedsHuman),
            )
            .unwrap();
        tracker
            .record_security_event(Some(first.id), None, &event(SecurityEventKind::ReplySent))
            .unwrap();
        execute(
            &tracker,
            "UPDATE security_events SET created_at = ?1 WHERE kind = ?2",
            params![
                timestamp(Utc::now() - TimeDelta::days(1)),
                SecurityEventKind::ReplySent.as_str()
            ],
        );

        let timeline = tracker.list_security_timeline(first.id).unwrap();
        assert_eq!(
            kinds(&timeline),
            vec![
                SecurityEventKind::ReplySent,
                SecurityEventKind::Received,
                SecurityEventKind::AssessmentStarted,
                SecurityEventKind::VulnerabilityCreated,
                SecurityEventKind::Assessed,
                SecurityEventKind::FixStarted,
            ]
        );
        let created = &timeline[3];
        assert_eq!(created.vulnerability_id, Some(vulnerability.id));
        assert_eq!(created.report_id, None);
        assert_eq!(created.to_state.as_deref(), Some("awaiting_fix"));
        assert_eq!(created.detail, json!({ "first_report_id": first.id }));

        assert_eq!(
            kinds(&tracker.list_security_timeline(unrelated.id).unwrap()),
            vec![SecurityEventKind::Received, SecurityEventKind::NeedsHuman]
        );
    }

    #[test]
    fn last_security_event_returns_the_newest_of_a_kind() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let vulnerability = tracker
            .create_security_vulnerability(&new_vulnerability(&report))
            .unwrap();
        assert_eq!(
            tracker
                .last_security_event(vulnerability.id, SecurityEventKind::DeploymentCheckFailed)
                .unwrap(),
            None
        );
        for attempt in 1..=2 {
            tracker
                .record_security_event(
                    None,
                    Some(vulnerability.id),
                    &event(SecurityEventKind::DeploymentCheckFailed)
                        .with_detail(json!({ "attempt": attempt })),
                )
                .unwrap();
        }
        let last = tracker
            .last_security_event(vulnerability.id, SecurityEventKind::DeploymentCheckFailed)
            .unwrap()
            .unwrap();
        assert_eq!(last.detail, json!({ "attempt": 2 }));
    }

    #[test]
    fn admin_confirmed_reports_are_those_whose_latest_event_is_the_confirmation() {
        let tracker = tracker();
        let confirmed = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let assessing = tracker
            .upsert_security_report(&new_report("2", reported_at(1)))
            .unwrap();
        let superseded = tracker
            .upsert_security_report(&new_report("3", reported_at(2)))
            .unwrap();
        for report in [&confirmed, &superseded] {
            assert!(tracker
                .transition_security_report(
                    report.id,
                    SecurityReportState::Received,
                    SecurityReportState::NeedsHuman,
                    &SecurityReportChanges::default(),
                    &event(SecurityEventKind::NeedsHuman),
                )
                .unwrap());
            assert!(tracker
                .transition_security_report(
                    report.id,
                    SecurityReportState::NeedsHuman,
                    SecurityReportState::Assessing,
                    &SecurityReportChanges::default(),
                    &event(SecurityEventKind::AdminConfirmed)
                        .with_actor("admin@example.com")
                        .with_detail(json!({ "title": "Session fixation" })),
                )
                .unwrap());
        }
        assess(&tracker, assessing.id);
        tracker
            .record_security_event(
                Some(superseded.id),
                None,
                &event(SecurityEventKind::AssessmentFailed),
            )
            .unwrap();

        let listed = tracker.list_admin_confirmed_reports().unwrap();
        assert_eq!(listed.len(), 1);
        let (report, confirmation) = &listed[0];
        assert_eq!(report.id, confirmed.id);
        assert_eq!(report.state, SecurityReportState::Assessing);
        assert_eq!(confirmation.kind, SecurityEventKind::AdminConfirmed);
        assert_eq!(confirmation.actor.as_deref(), Some("admin@example.com"));
        assert_eq!(confirmation.detail, json!({ "title": "Session fixation" }));
        assert_eq!(confirmation.from_state.as_deref(), Some("needs_human"));

        let vulnerability = tracker
            .create_security_vulnerability(&new_vulnerability(report))
            .unwrap();
        assert_eq!(tracker.list_admin_confirmed_reports().unwrap().len(), 1);
        track(&tracker, confirmed.id, vulnerability.id);
        assert!(tracker.list_admin_confirmed_reports().unwrap().is_empty());
    }

    #[test]
    fn reports_without_an_event_are_listed_per_state_oldest_first() {
        let tracker = tracker();
        let later = tracker
            .upsert_security_report(&new_report("1", reported_at(5)))
            .unwrap();
        let earlier = tracker
            .upsert_security_report(&new_report("2", reported_at(1)))
            .unwrap();
        let closed = tracker
            .upsert_security_report(&new_report("3", reported_at(0)))
            .unwrap();
        let unrelated = tracker
            .upsert_security_report(&new_report("4", reported_at(2)))
            .unwrap();
        tracker
            .record_security_event(
                Some(closed.id),
                None,
                &event(SecurityEventKind::ConversationClosed),
            )
            .unwrap();
        assess(&tracker, unrelated.id);

        let listed = tracker
            .list_security_reports_without_event(
                SecurityReportState::Received,
                SecurityEventKind::ConversationClosed,
            )
            .unwrap();
        let ids: Vec<i64> = listed.iter().map(|report| report.id).collect();
        assert_eq!(ids, vec![earlier.id, later.id]);
        assert!(tracker
            .list_security_reports_without_event(
                SecurityReportState::Received,
                SecurityEventKind::Received,
            )
            .unwrap()
            .is_empty());
    }

    #[test]
    fn create_vulnerability_stores_fields_and_records_creation() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let vulnerability = tracker
            .create_security_vulnerability(&new_vulnerability(&report))
            .unwrap();

        assert_eq!(vulnerability.state, VulnerabilityState::AwaitingFix);
        assert_eq!(vulnerability.embedding, vec![0.25, -0.5, 1.0]);
        assert_eq!(vulnerability.first_report_id, report.id);
        assert_eq!(
            vulnerability.repository.as_deref(),
            Some("appwrite/appwrite")
        );
        assert_eq!(
            vulnerability.severity,
            Some(claudear_core::security::SecuritySeverity::High)
        );
        assert_eq!(
            tracker
                .get_security_vulnerability(vulnerability.id)
                .unwrap(),
            Some(vulnerability.clone())
        );
        assert_eq!(
            tracker
                .last_security_event(vulnerability.id, SecurityEventKind::VulnerabilityCreated)
                .unwrap()
                .map(|event| event.to_state),
            Some(Some("awaiting_fix".to_string()))
        );

        let without_embedding = tracker
            .create_security_vulnerability(&NewSecurityVulnerability {
                embedding: Vec::new(),
                severity: None,
                ..new_vulnerability(&report)
            })
            .unwrap();
        assert!(without_embedding.embedding.is_empty());
        assert_eq!(without_embedding.severity, None);
    }

    #[test]
    fn create_vulnerability_refuses_a_non_initial_state() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        for state in [
            VulnerabilityState::Fixing,
            VulnerabilityState::Live,
            VulnerabilityState::NeedsHuman,
        ] {
            assert!(tracker
                .create_security_vulnerability(&NewSecurityVulnerability {
                    state,
                    ..new_vulnerability(&report)
                })
                .is_err());
        }
        assert!(tracker
            .list_security_vulnerabilities(None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn vulnerability_transitions_follow_the_state_machine() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let vulnerability = tracker
            .create_security_vulnerability(&new_vulnerability(&report))
            .unwrap();
        let before = event_count(&tracker);

        assert!(!tracker
            .transition_security_vulnerability(
                vulnerability.id,
                VulnerabilityState::AwaitingFix,
                VulnerabilityState::Live,
                &SecurityVulnerabilityChanges::default(),
                &event(SecurityEventKind::Live),
            )
            .unwrap());
        assert!(!tracker
            .transition_security_vulnerability(
                vulnerability.id,
                VulnerabilityState::Fixing,
                VulnerabilityState::Merged,
                &SecurityVulnerabilityChanges::default(),
                &event(SecurityEventKind::Merged),
            )
            .unwrap());
        assert_eq!(event_count(&tracker), before);

        assert!(tracker
            .transition_security_vulnerability(
                vulnerability.id,
                VulnerabilityState::AwaitingFix,
                VulnerabilityState::Fixing,
                &SecurityVulnerabilityChanges {
                    fix_attempt_id: Some(7),
                    pull_request_url: Some(
                        "https://github.com/appwrite/appwrite/pull/1".to_string()
                    ),
                    ..SecurityVulnerabilityChanges::default()
                },
                &event(SecurityEventKind::PullRequestOpened),
            )
            .unwrap());
        assert!(tracker
            .transition_security_vulnerability(
                vulnerability.id,
                VulnerabilityState::Fixing,
                VulnerabilityState::Merged,
                &SecurityVulnerabilityChanges {
                    merge_commit: Some("b".repeat(40)),
                    merged_at: Some(reported_at(10)),
                    ..SecurityVulnerabilityChanges::default()
                },
                &event(SecurityEventKind::Merged),
            )
            .unwrap());

        let stored = tracker
            .get_security_vulnerability(vulnerability.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, VulnerabilityState::Merged);
        assert_eq!(stored.fix_attempt_id, Some(7));
        assert_eq!(
            stored.pull_request_url.as_deref(),
            Some("https://github.com/appwrite/appwrite/pull/1")
        );
        assert_eq!(stored.merge_commit, Some("b".repeat(40)));
        assert_eq!(stored.repository, vulnerability.repository);
        assert_eq!(stored.title, vulnerability.title);

        let merged = tracker
            .last_security_event(vulnerability.id, SecurityEventKind::Merged)
            .unwrap()
            .unwrap();
        assert_eq!(merged.from_state.as_deref(), Some("fixing"));
        assert_eq!(merged.to_state.as_deref(), Some("merged"));

        assert_eq!(
            tracker
                .list_security_vulnerabilities(Some(VulnerabilityState::Merged))
                .unwrap()
                .len(),
            1
        );
        assert!(tracker
            .list_security_vulnerabilities(Some(VulnerabilityState::AwaitingFix))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn reports_for_a_vulnerability_are_oldest_first() {
        let tracker = tracker();
        let later = tracker
            .upsert_security_report(&new_report("2", reported_at(5)))
            .unwrap();
        let earlier = tracker
            .upsert_security_report(&new_report("1", reported_at(1)))
            .unwrap();
        tracker
            .upsert_security_report(&new_report("3", reported_at(0)))
            .unwrap();
        let vulnerability = tracker
            .create_security_vulnerability(&new_vulnerability(&later))
            .unwrap();
        for report in [&later, &earlier] {
            assess(&tracker, report.id);
            track(&tracker, report.id, vulnerability.id);
        }

        let ids: Vec<i64> = tracker
            .list_reports_for_vulnerability(vulnerability.id)
            .unwrap()
            .into_iter()
            .map(|report| report.id)
            .collect();
        assert_eq!(ids, vec![earlier.id, later.id]);
    }

    #[test]
    fn replace_first_reporter_requires_an_earlier_report_and_no_award() {
        let tracker = tracker();
        let current = tracker
            .upsert_security_report(&new_report("1", reported_at(10)))
            .unwrap();
        let earlier = tracker
            .upsert_security_report(&new_report("2", reported_at(5)))
            .unwrap();
        let earliest = tracker
            .upsert_security_report(&new_report("3", reported_at(1)))
            .unwrap();
        let vulnerability = tracker
            .create_security_vulnerability(&new_vulnerability(&current))
            .unwrap();

        assert!(!tracker
            .replace_first_reporter(vulnerability.id, current.id, earlier.id, reported_at(10))
            .unwrap());
        assert!(!tracker
            .replace_first_reporter(vulnerability.id, current.id, earlier.id, reported_at(11))
            .unwrap());
        assert!(!tracker
            .replace_first_reporter(vulnerability.id, earlier.id, earliest.id, reported_at(1))
            .unwrap());
        assert!(!tracker
            .replace_first_reporter(vulnerability.id, current.id, current.id, reported_at(1))
            .unwrap());

        assert!(tracker
            .replace_first_reporter(vulnerability.id, current.id, earlier.id, reported_at(5))
            .unwrap());
        let stored = tracker
            .get_security_vulnerability(vulnerability.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.first_report_id, earlier.id);
        assert_eq!(stored.first_reported_at, reported_at(5));
        let replaced = tracker
            .last_security_event(vulnerability.id, SecurityEventKind::FirstReporterReplaced)
            .unwrap()
            .unwrap();
        assert_eq!(
            replaced.detail,
            json!({ "previous_report_id": current.id, "report_id": earlier.id })
        );

        assert!(tracker
            .prepare_security_award(vulnerability.id, earlier.id, &new_award())
            .unwrap());
        assert!(!tracker
            .replace_first_reporter(vulnerability.id, earlier.id, earliest.id, reported_at(1))
            .unwrap());
        assert_eq!(
            tracker
                .get_security_vulnerability(vulnerability.id)
                .unwrap()
                .unwrap()
                .first_report_id,
            earlier.id
        );
    }

    #[test]
    fn reply_claims_are_exclusive_and_only_failed_replies_are_reclaimed() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let kind = SecurityReplyKind::Confirmed;

        assert!(tracker.claim_security_reply(report.id, kind).unwrap());
        assert!(!tracker.claim_security_reply(report.id, kind).unwrap());
        assert!(tracker
            .claim_security_reply(report.id, SecurityReplyKind::FixLive)
            .unwrap());

        tracker
            .complete_security_reply(report.id, kind, DeliveryStatus::Failed, Some("422"))
            .unwrap();
        let failed = tracker.list_security_replies(report.id).unwrap();
        assert_eq!(failed[0].status, DeliveryStatus::Failed);
        assert_eq!(failed[0].error.as_deref(), Some("422"));
        assert!(failed[0].completed_at.is_some());

        assert!(tracker.claim_security_reply(report.id, kind).unwrap());
        let reclaimed = &tracker.list_security_replies(report.id).unwrap()[0];
        assert_eq!(reclaimed.status, DeliveryStatus::Sending);
        assert_eq!(reclaimed.error, None);
        assert_eq!(reclaimed.completed_at, None);

        tracker
            .complete_security_reply(report.id, kind, DeliveryStatus::Unconfirmed, Some("503"))
            .unwrap();
        assert!(!tracker.claim_security_reply(report.id, kind).unwrap());

        tracker
            .complete_security_reply(
                report.id,
                SecurityReplyKind::FixLive,
                DeliveryStatus::Sent,
                None,
            )
            .unwrap();
        assert!(!tracker
            .claim_security_reply(report.id, SecurityReplyKind::FixLive)
            .unwrap());

        let replies = tracker.list_security_replies(report.id).unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].kind, kind);
        assert_eq!(replies[0].status, DeliveryStatus::Unconfirmed);
        assert_eq!(replies[1].status, DeliveryStatus::Sent);
    }

    #[test]
    fn complete_reply_requires_an_in_flight_claim_and_a_final_status() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let kind = SecurityReplyKind::Duplicate;

        assert!(tracker
            .complete_security_reply(report.id, kind, DeliveryStatus::Sent, None)
            .is_err());
        assert!(tracker.claim_security_reply(report.id, kind).unwrap());
        assert!(tracker
            .complete_security_reply(report.id, kind, DeliveryStatus::Sending, None)
            .is_err());
        tracker
            .complete_security_reply(report.id, kind, DeliveryStatus::Sent, None)
            .unwrap();
        assert!(tracker
            .complete_security_reply(report.id, kind, DeliveryStatus::Failed, None)
            .is_err());
        assert_eq!(
            tracker.list_security_replies(report.id).unwrap()[0].status,
            DeliveryStatus::Sent
        );
    }

    #[test]
    fn confirm_reply_resolves_only_unconfirmed_replies() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let delivered = SecurityReplyKind::Confirmed;
        let lost = SecurityReplyKind::FixLiveRewarded;
        for kind in [delivered, lost] {
            assert!(tracker.claim_security_reply(report.id, kind).unwrap());
            assert!(!tracker
                .confirm_security_reply(report.id, kind, true, "admin@example.com")
                .unwrap());
            tracker
                .complete_security_reply(report.id, kind, DeliveryStatus::Unconfirmed, None)
                .unwrap();
        }

        assert!(tracker
            .confirm_security_reply(report.id, delivered, true, "admin@example.com")
            .unwrap());
        assert!(!tracker
            .confirm_security_reply(report.id, delivered, false, "admin@example.com")
            .unwrap());
        assert!(!tracker.claim_security_reply(report.id, delivered).unwrap());

        assert!(tracker
            .confirm_security_reply(report.id, lost, false, "admin@example.com")
            .unwrap());
        let replies = tracker.list_security_replies(report.id).unwrap();
        assert_eq!(replies[0].status, DeliveryStatus::Sent);
        assert_eq!(replies[0].error, None);
        assert_eq!(replies[1].status, DeliveryStatus::Failed);
        assert_eq!(replies[1].error.as_deref(), Some(NOT_DELIVERED_ERROR));
        assert!(tracker.claim_security_reply(report.id, lost).unwrap());

        let confirmations: Vec<SecurityEvent> = tracker
            .list_security_timeline(report.id)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == SecurityEventKind::DeliveryConfirmed)
            .collect();
        assert_eq!(confirmations.len(), 2);
        assert_eq!(confirmations[0].actor.as_deref(), Some("admin@example.com"));
        assert_eq!(confirmations[0].from_state.as_deref(), Some("unconfirmed"));
        assert_eq!(confirmations[0].to_state.as_deref(), Some("sent"));
        assert_eq!(
            confirmations[0].detail,
            json!({ "kind": "confirmed", "delivered": true })
        );
        assert_eq!(confirmations[1].to_state.as_deref(), Some("failed"));
    }

    #[test]
    fn award_is_prepared_once_and_only_for_the_first_reporter() {
        let tracker = tracker();
        let first = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let duplicate = tracker
            .upsert_security_report(&new_report("2", reported_at(5)))
            .unwrap();
        let vulnerability = tracker
            .create_security_vulnerability(&new_vulnerability(&first))
            .unwrap();

        assert!(!tracker
            .prepare_security_award(vulnerability.id, duplicate.id, &new_award())
            .unwrap());
        assert!(!tracker
            .prepare_security_award(999, first.id, &new_award())
            .unwrap());
        assert_eq!(tracker.get_security_award(vulnerability.id).unwrap(), None);

        assert!(tracker
            .prepare_security_award(vulnerability.id, first.id, &new_award())
            .unwrap());
        assert!(!tracker
            .prepare_security_award(vulnerability.id, first.id, &new_award())
            .unwrap());

        let award = tracker
            .get_security_award(vulnerability.id)
            .unwrap()
            .unwrap();
        assert_eq!(award.report_id, first.id);
        assert_eq!(award.status, AwardStatus::Prepared);
        assert_eq!(award.campaign, "SecurityResearchers");
        assert_eq!(award.credits, 100);
        assert_eq!(
            award.recipient_email.as_deref(),
            Some("researcher@example.com")
        );

        let prepared: Vec<SecurityEvent> = tracker
            .list_security_timeline(first.id)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == SecurityEventKind::AwardPrepared)
            .collect();
        assert!(
            prepared.is_empty(),
            "the award event belongs to the vulnerability"
        );
        let prepared = tracker
            .last_security_event(vulnerability.id, SecurityEventKind::AwardPrepared)
            .unwrap()
            .unwrap();
        assert_eq!(prepared.to_state.as_deref(), Some("prepared"));
        assert_eq!(prepared.detail["report_id"], json!(first.id));
        assert!(prepared.detail.get("recipient_email").is_none());
    }

    #[test]
    fn award_status_moves_only_along_allowed_edges_from_an_expected_status() {
        let tracker = tracker();
        let report = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let vulnerability = tracker
            .create_security_vulnerability(&new_vulnerability(&report))
            .unwrap();

        assert!(!tracker
            .update_security_award_status(
                vulnerability.id,
                &[AwardStatus::Prepared],
                AwardStatus::OwnerNotified,
                None,
                None,
            )
            .unwrap());
        tracker
            .prepare_security_award(vulnerability.id, report.id, &new_award())
            .unwrap();

        assert!(!tracker
            .update_security_award_status(
                vulnerability.id,
                &[],
                AwardStatus::OwnerNotified,
                None,
                None
            )
            .unwrap());
        assert!(!tracker
            .update_security_award_status(
                vulnerability.id,
                &[AwardStatus::OwnerNotified],
                AwardStatus::Awarded,
                None,
                None,
            )
            .unwrap());
        assert!(!tracker
            .update_security_award_status(
                vulnerability.id,
                &[AwardStatus::Prepared],
                AwardStatus::Prepared,
                None,
                None,
            )
            .unwrap());

        assert!(tracker
            .update_security_award_status(
                vulnerability.id,
                &[AwardStatus::Prepared],
                AwardStatus::OwnerNotified,
                None,
                None,
            )
            .unwrap());
        assert!(tracker
            .update_security_award_status(
                vulnerability.id,
                &[AwardStatus::Prepared, AwardStatus::OwnerNotified],
                AwardStatus::Awarded,
                Some("admin@example.com"),
                Some("Sent coupon by email"),
            )
            .unwrap());
        assert!(!tracker
            .update_security_award_status(
                vulnerability.id,
                &[AwardStatus::Awarded],
                AwardStatus::Cancelled,
                Some("admin@example.com"),
                None,
            )
            .unwrap());

        let award = tracker
            .get_security_award(vulnerability.id)
            .unwrap()
            .unwrap();
        assert_eq!(award.status, AwardStatus::Awarded);
        assert_eq!(award.actor.as_deref(), Some("admin@example.com"));
        assert_eq!(award.note.as_deref(), Some("Sent coupon by email"));

        let notified = tracker
            .last_security_event(vulnerability.id, SecurityEventKind::OwnerNotified)
            .unwrap()
            .unwrap();
        assert_eq!(notified.from_state.as_deref(), Some("prepared"));
        assert_eq!(notified.to_state.as_deref(), Some("owner_notified"));
        let awarded = tracker
            .last_security_event(vulnerability.id, SecurityEventKind::Awarded)
            .unwrap()
            .unwrap();
        assert_eq!(awarded.actor.as_deref(), Some("admin@example.com"));
        assert_eq!(awarded.from_state.as_deref(), Some("owner_notified"));
        assert_eq!(awarded.detail, json!({ "report_id": report.id }));
    }

    #[test]
    fn list_security_awards_filters_by_status() {
        let tracker = tracker();
        let first = tracker
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        let second = tracker
            .upsert_security_report(&new_report("2", reported_at(1)))
            .unwrap();
        let prepared = tracker
            .create_security_vulnerability(&new_vulnerability(&first))
            .unwrap();
        let awarded = tracker
            .create_security_vulnerability(&new_vulnerability(&second))
            .unwrap();
        tracker
            .prepare_security_award(prepared.id, first.id, &new_award())
            .unwrap();
        tracker
            .prepare_security_award(awarded.id, second.id, &new_award())
            .unwrap();
        tracker
            .update_security_award_status(
                awarded.id,
                &[AwardStatus::Prepared],
                AwardStatus::Awarded,
                Some("admin@example.com"),
                None,
            )
            .unwrap();

        assert_eq!(tracker.list_security_awards(None).unwrap().len(), 2);
        let pending: Vec<i64> = tracker
            .list_security_awards(Some(AwardStatus::Prepared))
            .unwrap()
            .into_iter()
            .map(|award| award.vulnerability_id)
            .collect();
        assert_eq!(pending, vec![prepared.id]);
    }

    #[test]
    fn trait_delegates_to_the_sqlite_implementation() {
        let tracker = tracker();
        let store: &dyn crate::FixAttemptTracker = &tracker;
        let report = store
            .upsert_security_report(&new_report("1", reported_at(0)))
            .unwrap();
        assert_eq!(
            store
                .get_security_report_by_conversation("helpscout", "1")
                .unwrap()
                .map(|stored| stored.id),
            Some(report.id)
        );
        assert!(store
            .transition_security_report(
                report.id,
                SecurityReportState::Received,
                SecurityReportState::Assessing,
                &SecurityReportChanges::default(),
                &event(SecurityEventKind::AssessmentStarted),
            )
            .unwrap());
        assert!(store
            .claim_security_reply(report.id, SecurityReplyKind::Confirmed)
            .unwrap());
        assert_eq!(store.list_security_timeline(report.id).unwrap().len(), 2);
    }
}
