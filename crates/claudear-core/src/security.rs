//! Security inbox domain: report and vulnerability state machines, replies,
//! awards and the admin overrides applied to them.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

pub const SECURITY_FIX_SOURCE: &str = "security";

const SECURITY_FIX_SHORT_ID_PREFIX: &str = "SEC-";
const MAX_PUBLIC_REASON_CHARS: usize = 1000;
const MAX_CONFIRMED_TITLE_CHARS: usize = 200;
const MAX_CONFIRMED_SUMMARY_CHARS: usize = 5000;
const MAX_REPOSITORY_SEGMENT_CHARS: usize = 100;

pub fn security_fix_short_id(vulnerability_id: i64) -> String {
    format!("{SECURITY_FIX_SHORT_ID_PREFIX}{vulnerability_id}")
}

/// Parses only the canonical `SEC-<id>` form, so every accepted value maps
/// back to exactly one vulnerability.
pub fn parse_security_fix_short_id(value: &str) -> Option<i64> {
    let digits = value.strip_prefix(SECURITY_FIX_SHORT_ID_PREFIX)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let id: i64 = digits.parse().ok()?;
    (id > 0 && security_fix_short_id(id) == value).then_some(id)
}

macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $name:ident { $($variant:ident => $value:literal),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    other => Err(format!("unknown {} value: {other}", stringify!($name))),
                }
            }
        }
    };
}

string_enum! {
    SecurityReportState {
        Received => "received",
        Assessing => "assessing",
        NeedsHuman => "needs_human",
        Spam => "spam",
        Replying => "replying",
        Closed => "closed",
        Tracking => "tracking",
        Resolved => "resolved",
    }
}

impl SecurityReportState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Spam | Self::Closed | Self::Resolved)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Received => matches!(next, Self::Assessing | Self::NeedsHuman),
            Self::Assessing => matches!(
                next,
                Self::Received | Self::NeedsHuman | Self::Spam | Self::Replying | Self::Tracking
            ),
            Self::NeedsHuman => matches!(
                next,
                Self::Received | Self::Assessing | Self::Replying | Self::Spam
            ),
            Self::Replying => next == Self::Closed,
            Self::Tracking => next == Self::Resolved,
            Self::Spam | Self::Closed | Self::Resolved => false,
        }
    }
}

string_enum! {
    VulnerabilityState {
        AwaitingFix => "awaiting_fix",
        Fixing => "fixing",
        Merged => "merged",
        Live => "live",
        NeedsHuman => "needs_human",
    }
}

impl VulnerabilityState {
    /// A vulnerability starts waiting for a fix, or already merged when the
    /// fix landed after the report but before it was assessed.
    pub fn is_initial(self) -> bool {
        matches!(self, Self::AwaitingFix | Self::Merged)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::AwaitingFix => matches!(next, Self::Fixing | Self::NeedsHuman),
            Self::Fixing => matches!(next, Self::Merged | Self::NeedsHuman),
            Self::Merged => matches!(next, Self::Live | Self::NeedsHuman),
            Self::NeedsHuman => matches!(next, Self::Fixing | Self::Live),
            Self::Live => false,
        }
    }
}

string_enum! {
    SecurityOutcome {
        Spam => "spam",
        Valid => "valid",
        NotReproducible => "not_reproducible",
        AlreadyFixed => "already_fixed",
        NotVulnerability => "not_vulnerability",
        OutOfScope => "out_of_scope",
        Duplicate => "duplicate",
    }
}

string_enum! {
    SecurityReplyKind {
        Confirmed => "confirmed",
        NotReproducible => "not_reproducible",
        AlreadyFixed => "already_fixed",
        NotVulnerability => "not_vulnerability",
        OutOfScope => "out_of_scope",
        Duplicate => "duplicate",
        FixLive => "fix_live",
        FixLiveRewarded => "fix_live_rewarded",
    }
}

impl SecurityReplyKind {
    pub fn for_outcome(outcome: SecurityOutcome) -> Option<Self> {
        match outcome {
            SecurityOutcome::Spam => None,
            SecurityOutcome::Valid => Some(Self::Confirmed),
            SecurityOutcome::NotReproducible => Some(Self::NotReproducible),
            SecurityOutcome::AlreadyFixed => Some(Self::AlreadyFixed),
            SecurityOutcome::NotVulnerability => Some(Self::NotVulnerability),
            SecurityOutcome::OutOfScope => Some(Self::OutOfScope),
            SecurityOutcome::Duplicate => Some(Self::Duplicate),
        }
    }

    pub fn is_terminal(self) -> bool {
        self != Self::Confirmed
    }
}

string_enum! {
    SecuritySeverity {
        Critical => "critical",
        High => "high",
        Medium => "medium",
        Low => "low",
        Informational => "informational",
    }
}

string_enum! {
    DeliveryStatus {
        Sending => "sending",
        Sent => "sent",
        Failed => "failed",
        Unconfirmed => "unconfirmed",
    }
}

impl DeliveryStatus {
    pub fn is_completion(self) -> bool {
        self != Self::Sending
    }
}

string_enum! {
    AwardStatus {
        Prepared => "prepared",
        OwnerNotified => "owner_notified",
        Awarded => "awarded",
        Cancelled => "cancelled",
    }
}

impl AwardStatus {
    pub fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Prepared => matches!(next, Self::OwnerNotified | Self::Awarded | Self::Cancelled),
            Self::OwnerNotified => matches!(next, Self::Awarded | Self::Cancelled),
            Self::Awarded | Self::Cancelled => false,
        }
    }

    /// The event recorded when an award enters this status.
    pub fn event_kind(self) -> SecurityEventKind {
        match self {
            Self::Prepared => SecurityEventKind::AwardPrepared,
            Self::OwnerNotified => SecurityEventKind::OwnerNotified,
            Self::Awarded => SecurityEventKind::Awarded,
            Self::Cancelled => SecurityEventKind::AwardCancelled,
        }
    }
}

string_enum! {
    SecurityEventKind {
        Received => "received",
        AssessmentStarted => "assessment_started",
        AssessmentReleased => "assessment_released",
        AssessmentFailed => "assessment_failed",
        NeedsHuman => "needs_human",
        Overridden => "overridden",
        AdminConfirmed => "admin_confirmed",
        ScreenedSpam => "screened_spam",
        Assessed => "assessed",
        VulnerabilityCreated => "vulnerability_created",
        DuplicateLinked => "duplicate_linked",
        FirstReporterReplaced => "first_reporter_replaced",
        ReplySent => "reply_sent",
        ReplyFailed => "reply_failed",
        ReplyUnconfirmed => "reply_unconfirmed",
        DeliveryConfirmed => "delivery_confirmed",
        ConversationClosed => "conversation_closed",
        ReporterFollowUp => "reporter_follow_up",
        FixStarted => "fix_started",
        PullRequestOpened => "pull_request_opened",
        PullRequestLinked => "pull_request_linked",
        FixFailed => "fix_failed",
        Merged => "merged",
        CloudBumpRequired => "cloud_bump_required",
        Live => "live",
        DeploymentCheckFailed => "deployment_check_failed",
        AwardPrepared => "award_prepared",
        OwnerNotified => "owner_notified",
        Awarded => "awarded",
        AwardCancelled => "award_cancelled",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstReporterDecision {
    KeepExisting,
    Replace,
}

/// Ties keep the existing reporter, and a prepared award pins it for good.
pub fn select_first_reporter(
    current_first_reported_at: DateTime<Utc>,
    candidate_reported_at: DateTime<Utc>,
    award_exists: bool,
) -> FirstReporterDecision {
    if candidate_reported_at < current_first_reported_at && !award_exists {
        FirstReporterDecision::Replace
    } else {
        FirstReporterDecision::KeepExisting
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SecurityOverride {
    Reassess,
    Outcome {
        outcome: SecurityOutcome,
        #[serde(default)]
        public_reason: Option<String>,
        #[serde(default)]
        fixed_on: Option<NaiveDate>,
    },
    ConfirmDelivery {
        kind: SecurityReplyKind,
        delivered: bool,
    },
    ConfirmValid {
        title: String,
        summary: String,
        #[serde(default)]
        repository: Option<String>,
        #[serde(default)]
        severity: Option<SecuritySeverity>,
    },
}

impl SecurityOverride {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Reassess | Self::ConfirmDelivery { .. } => Ok(()),
            Self::Outcome {
                outcome,
                public_reason,
                fixed_on,
            } => validate_outcome(*outcome, public_reason.as_deref(), *fixed_on),
            Self::ConfirmValid {
                title,
                summary,
                repository,
                ..
            } => validate_confirmation(title, summary, repository.as_deref()),
        }
    }
}

fn validate_outcome(
    outcome: SecurityOutcome,
    public_reason: Option<&str>,
    fixed_on: Option<NaiveDate>,
) -> Result<(), String> {
    if outcome == SecurityOutcome::Valid {
        return Err("use confirm_valid to accept a report as valid".to_string());
    }
    if let Some(reason) = public_reason {
        if reason.chars().count() > MAX_PUBLIC_REASON_CHARS {
            return Err(format!(
                "public_reason must be at most {MAX_PUBLIC_REASON_CHARS} characters"
            ));
        }
    }
    if outcome == SecurityOutcome::AlreadyFixed && fixed_on.is_none() {
        return Err("already_fixed requires fixed_on".to_string());
    }
    let needs_reason = matches!(
        outcome,
        SecurityOutcome::NotVulnerability | SecurityOutcome::OutOfScope
    );
    if needs_reason && public_reason.is_none_or(|reason| reason.trim().is_empty()) {
        return Err(format!("{outcome} requires a public_reason"));
    }
    Ok(())
}

fn validate_confirmation(
    title: &str,
    summary: &str,
    repository: Option<&str>,
) -> Result<(), String> {
    validate_required_text("title", title, MAX_CONFIRMED_TITLE_CHARS)?;
    validate_required_text("summary", summary, MAX_CONFIRMED_SUMMARY_CHARS)?;
    match repository {
        Some(repository) if !is_repository_name(repository) => {
            Err("repository must be in owner/name form".to_string())
        }
        _ => Ok(()),
    }
}

fn validate_required_text(field: &str, value: &str, max_chars: usize) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.chars().count() > max_chars {
        return Err(format!("{field} must be at most {max_chars} characters"));
    }
    Ok(())
}

fn is_repository_name(value: &str) -> bool {
    let mut segments = value.split('/');
    let (Some(owner), Some(name), None) = (segments.next(), segments.next(), segments.next())
    else {
        return false;
    };
    is_repository_segment(owner) && is_repository_segment(name)
}

fn is_repository_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_REPOSITORY_SEGMENT_CHARS
        && segment != "."
        && segment != ".."
        && !segment.starts_with('-')
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityReport {
    pub id: i64,
    pub source: String,
    pub conversation_id: String,
    pub short_id: String,
    pub mailbox_id: String,
    pub url: String,
    pub subject: String,
    pub reporter_email: Option<String>,
    pub reporter_customer_id: Option<String>,
    pub reported_at: DateTime<Utc>,
    pub state: SecurityReportState,
    pub outcome: Option<SecurityOutcome>,
    pub public_reason: Option<String>,
    pub assessment_summary: Option<String>,
    pub affected_repository: Option<String>,
    pub fixed_by_commit: Option<String>,
    pub fixed_on: Option<NaiveDate>,
    pub spam_confidence: Option<f64>,
    pub assessment_attempts: u32,
    pub customer_thread_count: u32,
    pub vulnerability_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewSecurityReport {
    pub source: String,
    pub conversation_id: String,
    pub short_id: String,
    pub mailbox_id: String,
    pub url: String,
    pub subject: String,
    pub reporter_email: Option<String>,
    pub reporter_customer_id: Option<String>,
    pub reported_at: DateTime<Utc>,
    pub customer_thread_count: u32,
}

/// Sparse update: only `Some` fields are written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SecurityReportChanges {
    pub outcome: Option<SecurityOutcome>,
    pub public_reason: Option<String>,
    pub assessment_summary: Option<String>,
    pub affected_repository: Option<String>,
    pub fixed_by_commit: Option<String>,
    pub fixed_on: Option<NaiveDate>,
    pub spam_confidence: Option<f64>,
    pub assessment_attempts: Option<u32>,
    pub vulnerability_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityVulnerability {
    pub id: i64,
    pub title: String,
    pub summary: String,
    #[serde(skip)]
    pub embedding: Vec<f32>,
    pub repository: Option<String>,
    pub severity: Option<SecuritySeverity>,
    pub state: VulnerabilityState,
    pub first_report_id: i64,
    pub first_reported_at: DateTime<Utc>,
    pub fix_attempt_id: Option<i64>,
    pub pull_request_url: Option<String>,
    pub merge_commit: Option<String>,
    pub merged_at: Option<DateTime<Utc>>,
    pub deployed_version: Option<String>,
    pub live_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewSecurityVulnerability {
    pub title: String,
    pub summary: String,
    #[serde(skip)]
    pub embedding: Vec<f32>,
    pub repository: Option<String>,
    pub severity: Option<SecuritySeverity>,
    pub state: VulnerabilityState,
    pub first_report_id: i64,
    pub first_reported_at: DateTime<Utc>,
    pub merge_commit: Option<String>,
    pub merged_at: Option<DateTime<Utc>>,
}

/// Sparse update: only `Some` fields are written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityVulnerabilityChanges {
    pub repository: Option<String>,
    pub fix_attempt_id: Option<i64>,
    pub pull_request_url: Option<String>,
    pub merge_commit: Option<String>,
    pub merged_at: Option<DateTime<Utc>>,
    pub deployed_version: Option<String>,
    pub live_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityEvent {
    pub id: i64,
    pub report_id: Option<i64>,
    pub vulnerability_id: Option<i64>,
    pub kind: SecurityEventKind,
    pub from_state: Option<String>,
    pub to_state: Option<String>,
    pub actor: Option<String>,
    pub detail: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewSecurityEvent {
    pub kind: SecurityEventKind,
    pub actor: Option<String>,
    pub detail: serde_json::Value,
}

impl NewSecurityEvent {
    pub fn new(kind: SecurityEventKind) -> Self {
        Self {
            kind,
            actor: None,
            detail: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityReply {
    pub id: i64,
    pub report_id: i64,
    pub kind: SecurityReplyKind,
    pub status: DeliveryStatus,
    pub error: Option<String>,
    pub claimed_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityAward {
    pub id: i64,
    pub vulnerability_id: i64,
    pub report_id: i64,
    pub recipient_email: Option<String>,
    pub campaign: String,
    pub credits: u32,
    pub status: AwardStatus,
    pub actor: Option<String>,
    pub note: Option<String>,
    pub prepared_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewSecurityAward {
    pub recipient_email: Option<String>,
    pub campaign: String,
    pub credits: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityReportFilter {
    pub state: Option<SecurityReportState>,
    pub outcome: Option<SecurityOutcome>,
    pub limit: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn assert_string_round_trip<T>(variants: &[T])
    where
        T: Copy + fmt::Debug + PartialEq + Serialize + FromStr<Err = String> + fmt::Display,
        for<'de> T: Deserialize<'de>,
    {
        for &variant in variants {
            let text = variant.to_string();
            assert_eq!(
                serde_json::to_value(variant).unwrap(),
                json!(text),
                "serde name differs from Display for {variant:?}"
            );
            assert_eq!(text.parse::<T>().unwrap(), variant);
            assert_eq!(
                serde_json::from_value::<T>(json!(text)).unwrap(),
                variant,
                "serde does not parse {text}"
            );
        }
        assert!("not_a_variant".parse::<T>().is_err());
    }

    #[test]
    fn every_enum_matches_its_serde_name() {
        assert_string_round_trip(SecurityReportState::ALL);
        assert_string_round_trip(VulnerabilityState::ALL);
        assert_string_round_trip(SecurityOutcome::ALL);
        assert_string_round_trip(SecurityReplyKind::ALL);
        assert_string_round_trip(SecuritySeverity::ALL);
        assert_string_round_trip(DeliveryStatus::ALL);
        assert_string_round_trip(AwardStatus::ALL);
        assert_string_round_trip(SecurityEventKind::ALL);
    }

    #[test]
    fn report_state_transition_table_is_exact() {
        use SecurityReportState::*;
        let allowed = [
            (Received, Assessing),
            (Received, NeedsHuman),
            (Assessing, Received),
            (Assessing, NeedsHuman),
            (Assessing, Spam),
            (Assessing, Replying),
            (Assessing, Tracking),
            (NeedsHuman, Received),
            (NeedsHuman, Assessing),
            (NeedsHuman, Replying),
            (NeedsHuman, Spam),
            (Replying, Closed),
            (Tracking, Resolved),
        ];
        for &from in SecurityReportState::ALL {
            for &to in SecurityReportState::ALL {
                assert_eq!(
                    from.can_transition_to(to),
                    allowed.contains(&(from, to)),
                    "{from} -> {to}"
                );
            }
        }
    }

    #[test]
    fn terminal_report_states_have_no_exits() {
        use SecurityReportState::*;
        for &state in SecurityReportState::ALL {
            assert_eq!(
                state.is_terminal(),
                matches!(state, Spam | Closed | Resolved),
                "{state}"
            );
            if state.is_terminal() {
                assert!(SecurityReportState::ALL
                    .iter()
                    .all(|&next| !state.can_transition_to(next)));
            }
        }
    }

    #[test]
    fn vulnerability_state_transition_table_is_exact() {
        use VulnerabilityState::*;
        let allowed = [
            (AwaitingFix, Fixing),
            (AwaitingFix, NeedsHuman),
            (Fixing, Merged),
            (Fixing, NeedsHuman),
            (Merged, Live),
            (Merged, NeedsHuman),
            (NeedsHuman, Fixing),
            (NeedsHuman, Live),
        ];
        for &from in VulnerabilityState::ALL {
            for &to in VulnerabilityState::ALL {
                assert_eq!(
                    from.can_transition_to(to),
                    allowed.contains(&(from, to)),
                    "{from} -> {to}"
                );
            }
        }
    }

    #[test]
    fn vulnerability_starts_awaiting_fix_or_merged() {
        use VulnerabilityState::*;
        for &state in VulnerabilityState::ALL {
            assert_eq!(
                state.is_initial(),
                matches!(state, AwaitingFix | Merged),
                "{state}"
            );
        }
    }

    #[test]
    fn award_status_transition_table_is_exact() {
        use AwardStatus::*;
        let allowed = [
            (Prepared, OwnerNotified),
            (Prepared, Awarded),
            (Prepared, Cancelled),
            (OwnerNotified, Awarded),
            (OwnerNotified, Cancelled),
        ];
        for &from in AwardStatus::ALL {
            for &to in AwardStatus::ALL {
                assert_eq!(
                    from.can_transition_to(to),
                    allowed.contains(&(from, to)),
                    "{from} -> {to}"
                );
            }
        }
    }

    #[test]
    fn award_status_event_kinds() {
        assert_eq!(
            AwardStatus::Prepared.event_kind(),
            SecurityEventKind::AwardPrepared
        );
        assert_eq!(
            AwardStatus::OwnerNotified.event_kind(),
            SecurityEventKind::OwnerNotified
        );
        assert_eq!(
            AwardStatus::Awarded.event_kind(),
            SecurityEventKind::Awarded
        );
        assert_eq!(
            AwardStatus::Cancelled.event_kind(),
            SecurityEventKind::AwardCancelled
        );
    }

    #[test]
    fn only_sending_is_not_a_completion() {
        for &status in DeliveryStatus::ALL {
            assert_eq!(status.is_completion(), status != DeliveryStatus::Sending);
        }
    }

    #[test]
    fn reply_kind_for_outcome() {
        use SecurityOutcome as Outcome;
        use SecurityReplyKind as Kind;
        let expected = [
            (Outcome::Spam, None),
            (Outcome::Valid, Some(Kind::Confirmed)),
            (Outcome::NotReproducible, Some(Kind::NotReproducible)),
            (Outcome::AlreadyFixed, Some(Kind::AlreadyFixed)),
            (Outcome::NotVulnerability, Some(Kind::NotVulnerability)),
            (Outcome::OutOfScope, Some(Kind::OutOfScope)),
            (Outcome::Duplicate, Some(Kind::Duplicate)),
        ];
        assert_eq!(expected.len(), SecurityOutcome::ALL.len());
        for (outcome, kind) in expected {
            assert_eq!(SecurityReplyKind::for_outcome(outcome), kind, "{outcome}");
        }
    }

    #[test]
    fn only_confirmed_reply_is_not_terminal() {
        for &kind in SecurityReplyKind::ALL {
            assert_eq!(
                kind.is_terminal(),
                kind != SecurityReplyKind::Confirmed,
                "{kind}"
            );
        }
    }

    #[test]
    fn select_first_reporter_replaces_only_an_earlier_unawarded_report() {
        let current = Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap();
        let earlier = current - chrono::TimeDelta::seconds(1);
        let later = current + chrono::TimeDelta::seconds(1);

        assert_eq!(
            select_first_reporter(current, earlier, false),
            FirstReporterDecision::Replace
        );
        assert_eq!(
            select_first_reporter(current, later, false),
            FirstReporterDecision::KeepExisting
        );
        assert_eq!(
            select_first_reporter(current, current, false),
            FirstReporterDecision::KeepExisting
        );
        assert_eq!(
            select_first_reporter(current, earlier, true),
            FirstReporterDecision::KeepExisting
        );
    }

    #[test]
    fn short_id_round_trips() {
        assert_eq!(security_fix_short_id(42), "SEC-42");
        assert_eq!(parse_security_fix_short_id("SEC-42"), Some(42));
        assert_eq!(
            parse_security_fix_short_id(&security_fix_short_id(i64::MAX)),
            Some(i64::MAX)
        );
    }

    #[test]
    fn short_id_rejects_non_canonical_values() {
        for value in [
            "",
            "SEC-",
            "SEC-0",
            "SEC-007",
            "SEC--1",
            "SEC-+1",
            "SEC-1a",
            "SEC-1 ",
            " SEC-1",
            "sec-1",
            "HS-1",
            "SEC-99999999999999999999",
            "SEC-١",
        ] {
            assert_eq!(parse_security_fix_short_id(value), None, "{value:?}");
        }
    }

    #[test]
    fn override_serde_uses_action_tag() {
        let reassess: SecurityOverride =
            serde_json::from_value(json!({"action": "reassess"})).unwrap();
        assert_eq!(reassess, SecurityOverride::Reassess);

        let outcome: SecurityOverride = serde_json::from_value(json!({
            "action": "outcome",
            "outcome": "already_fixed",
            "fixed_on": "2026-11-04"
        }))
        .unwrap();
        assert_eq!(
            outcome,
            SecurityOverride::Outcome {
                outcome: SecurityOutcome::AlreadyFixed,
                public_reason: None,
                fixed_on: NaiveDate::from_ymd_opt(2026, 11, 4),
            }
        );

        let delivery: SecurityOverride = serde_json::from_value(json!({
            "action": "confirm_delivery",
            "kind": "fix_live_rewarded",
            "delivered": false
        }))
        .unwrap();
        assert_eq!(
            delivery,
            SecurityOverride::ConfirmDelivery {
                kind: SecurityReplyKind::FixLiveRewarded,
                delivered: false,
            }
        );

        let confirmation: SecurityOverride = serde_json::from_value(json!({
            "action": "confirm_valid",
            "title": "Session fixation",
            "summary": "Sessions survive logout.",
            "severity": "high"
        }))
        .unwrap();
        assert_eq!(
            confirmation,
            SecurityOverride::ConfirmValid {
                title: "Session fixation".to_string(),
                summary: "Sessions survive logout.".to_string(),
                repository: None,
                severity: Some(SecuritySeverity::High),
            }
        );

        assert_eq!(
            serde_json::to_value(&SecurityOverride::Reassess).unwrap(),
            json!({"action": "reassess"})
        );
        assert_eq!(
            serde_json::to_value(&confirmation).unwrap()["action"],
            json!("confirm_valid")
        );
    }

    #[test]
    fn override_serde_rejects_unknown_action_and_outcome() {
        assert!(serde_json::from_value::<SecurityOverride>(json!({"action": "delete"})).is_err());
        assert!(serde_json::from_value::<SecurityOverride>(json!({"outcome": "spam"})).is_err());
        assert!(serde_json::from_value::<SecurityOverride>(
            json!({"action": "outcome", "outcome": "exploited"})
        )
        .is_err());
    }

    fn outcome(
        outcome: SecurityOutcome,
        public_reason: Option<&str>,
        fixed_on: Option<NaiveDate>,
    ) -> SecurityOverride {
        SecurityOverride::Outcome {
            outcome,
            public_reason: public_reason.map(str::to_string),
            fixed_on,
        }
    }

    #[test]
    fn outcome_override_validation() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 2);

        assert!(outcome(SecurityOutcome::Valid, None, None)
            .validate()
            .is_err());
        assert!(outcome(SecurityOutcome::AlreadyFixed, None, None)
            .validate()
            .is_err());
        assert!(outcome(SecurityOutcome::AlreadyFixed, None, date)
            .validate()
            .is_ok());

        for needs_reason in [
            SecurityOutcome::NotVulnerability,
            SecurityOutcome::OutOfScope,
        ] {
            assert!(outcome(needs_reason, None, None).validate().is_err());
            assert!(outcome(needs_reason, Some("   "), None).validate().is_err());
            assert!(outcome(needs_reason, Some("Intended behaviour."), None)
                .validate()
                .is_ok());
        }

        for no_reason in [
            SecurityOutcome::Spam,
            SecurityOutcome::NotReproducible,
            SecurityOutcome::Duplicate,
        ] {
            assert!(outcome(no_reason, None, None).validate().is_ok());
        }

        let longest = "a".repeat(MAX_PUBLIC_REASON_CHARS);
        let too_long = "a".repeat(MAX_PUBLIC_REASON_CHARS + 1);
        assert!(outcome(SecurityOutcome::OutOfScope, Some(&longest), None)
            .validate()
            .is_ok());
        assert!(outcome(SecurityOutcome::OutOfScope, Some(&too_long), None)
            .validate()
            .is_err());
        assert!(outcome(SecurityOutcome::Spam, Some(&too_long), None)
            .validate()
            .is_err());
    }

    #[test]
    fn public_reason_limit_counts_characters_not_bytes() {
        let multibyte = "é".repeat(MAX_PUBLIC_REASON_CHARS);
        assert!(outcome(SecurityOutcome::OutOfScope, Some(&multibyte), None)
            .validate()
            .is_ok());
    }

    #[test]
    fn reassess_and_delivery_overrides_are_always_valid() {
        assert!(SecurityOverride::Reassess.validate().is_ok());
        assert!(SecurityOverride::ConfirmDelivery {
            kind: SecurityReplyKind::Confirmed,
            delivered: true,
        }
        .validate()
        .is_ok());
    }

    fn confirmation(title: &str, summary: &str, repository: Option<&str>) -> SecurityOverride {
        SecurityOverride::ConfirmValid {
            title: title.to_string(),
            summary: summary.to_string(),
            repository: repository.map(str::to_string),
            severity: None,
        }
    }

    #[test]
    fn confirm_valid_override_validation() {
        assert!(confirmation("Title", "Summary", None).validate().is_ok());
        assert!(confirmation("Title", "Summary", Some("appwrite/appwrite"))
            .validate()
            .is_ok());
        assert!(
            confirmation("Title", "Summary", Some("appwrite-labs/cloud"))
                .validate()
                .is_ok()
        );

        assert!(confirmation("", "Summary", None).validate().is_err());
        assert!(confirmation("  ", "Summary", None).validate().is_err());
        assert!(confirmation("Title", "", None).validate().is_err());
        assert!(confirmation("Title", "\n", None).validate().is_err());

        let longest_title = "t".repeat(MAX_CONFIRMED_TITLE_CHARS);
        let long_title = "t".repeat(MAX_CONFIRMED_TITLE_CHARS + 1);
        assert!(confirmation(&longest_title, "Summary", None)
            .validate()
            .is_ok());
        assert!(confirmation(&long_title, "Summary", None)
            .validate()
            .is_err());

        let longest_summary = "s".repeat(MAX_CONFIRMED_SUMMARY_CHARS);
        let long_summary = "s".repeat(MAX_CONFIRMED_SUMMARY_CHARS + 1);
        assert!(confirmation("Title", &longest_summary, None)
            .validate()
            .is_ok());
        assert!(confirmation("Title", &long_summary, None)
            .validate()
            .is_err());
    }

    #[test]
    fn confirm_valid_rejects_malformed_repositories() {
        for repository in [
            "",
            "appwrite",
            "/appwrite",
            "appwrite/",
            "appwrite/appwrite/extra",
            "../appwrite",
            "appwrite/..",
            "./appwrite",
            "-appwrite/cloud",
            "appwrite/-cloud",
            "app write/cloud",
            "https://github.com/appwrite/appwrite",
            "appwrite/cloud;rm",
        ] {
            assert!(
                confirmation("Title", "Summary", Some(repository))
                    .validate()
                    .is_err(),
                "{repository:?}"
            );
        }
    }

    #[test]
    fn new_security_event_builder() {
        let event = NewSecurityEvent::new(SecurityEventKind::Overridden)
            .with_actor("admin@example.com")
            .with_detail(json!({"action": "reassess"}));
        assert_eq!(event.kind, SecurityEventKind::Overridden);
        assert_eq!(event.actor.as_deref(), Some("admin@example.com"));
        assert_eq!(event.detail, json!({"action": "reassess"}));
        assert_eq!(
            NewSecurityEvent::new(SecurityEventKind::Received).detail,
            json!({})
        );
    }

    #[test]
    fn vulnerability_embedding_is_not_serialized() {
        let vulnerability = SecurityVulnerability {
            id: 1,
            title: "Title".to_string(),
            summary: "Summary".to_string(),
            embedding: vec![0.5, 0.25],
            repository: None,
            severity: None,
            state: VulnerabilityState::AwaitingFix,
            first_report_id: 1,
            first_reported_at: Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap(),
            fix_attempt_id: None,
            pull_request_url: None,
            merge_commit: None,
            merged_at: None,
            deployed_version: None,
            live_at: None,
            created_at: Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap(),
        };
        let value = serde_json::to_value(&vulnerability).unwrap();
        assert!(value.get("embedding").is_none());
        assert_eq!(value["state"], json!("awaiting_fix"));
    }
}
