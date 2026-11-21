//! Digest of Discord support forum threads that need a reply.
//!
//! Ranking is heuristic: who spoke last, how long the poster has waited, bumps,
//! impact keywords and other users reporting the same problem. Every signal that
//! fires is kept in `reasons` so the ranking can be checked at a glance.

use chrono::{DateTime, Duration, Utc};
use regex_lite::Regex;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::LazyLock;

/// Who wrote a message, relative to the thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    /// The thread's author.
    Op,
    /// Someone listed as team.
    Team,
    /// Anyone else.
    Community,
}

/// Where a support thread stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportStatus {
    /// The poster or the community spoke last, or the team only promised a look.
    NeedsReply,
    /// The poster's last message reads like thanks or "fixed".
    LikelyResolved,
    /// The team replied last, or the poster said they'd follow up.
    WaitingOnUser,
}

/// One message in a support thread.
#[derive(Debug, Clone)]
pub struct SupportMessage {
    pub id: String,
    pub author_id: String,
    pub author: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

/// A support thread and its messages, oldest first.
#[derive(Debug, Clone)]
pub struct SupportThread {
    pub id: String,
    pub title: String,
    pub owner_id: String,
    /// Forum tag names applied to the thread.
    pub tags: Vec<String>,
    pub url: String,
    pub messages: Vec<SupportMessage>,
}

/// A triaged support thread.
#[derive(Debug, Clone, Serialize)]
pub struct SupportEntry {
    pub thread_id: String,
    pub title: String,
    pub url: String,
    pub status: SupportStatus,
    /// Urgency; only meaningful for `NeedsReply`.
    pub score: u32,
    /// The signals behind the status and score.
    pub reasons: Vec<String>,
    pub last_author: String,
    pub last_message: String,
    /// Hours since the poster started waiting, or since the last message.
    pub waiting_hours: i64,
    /// Whether the thread was missing from the previous digest.
    pub is_new: bool,
}

/// Support threads that need a reply, ranked.
#[derive(Debug, Clone, Serialize)]
pub struct SupportDigest {
    /// Window of thread activity, in days.
    pub days: i64,
    /// The most urgent threads that need a reply.
    pub needs_reply: Vec<SupportEntry>,
    /// All threads that need a reply, including those not listed.
    pub needs_reply_total: usize,
    /// Threads that look resolved and can be closed.
    pub likely_resolved: Vec<SupportEntry>,
    /// Threads where the team replied last.
    pub waiting_on_user: usize,
    /// Suggested answers waiting for review.
    pub drafts_to_review: usize,
}

impl SupportDigest {
    /// Whether a listed thread was missing from the previous digest.
    pub fn has_new(&self) -> bool {
        self.needs_reply.iter().any(|entry| entry.is_new)
    }
}

// Matched against the title and every message from the poster, lowercased.
static IMPACT: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        (
            "production or paying users",
            r"\b(prod|production|live app|paid|pro plan|scale plan|enterprise|customers?|clients?|launch(ed|ing)?)\b",
        ),
        (
            "an outage",
            r"\b(down|outage|not loading|unreachable|timed? ?out|50[0234]|internal server error)\b",
        ),
        (
            "data loss",
            r"\b(data loss|lost (all |my )?(data|rows|documents|files)|missing (rows|documents|data|files)|corrupt(ed)?|wiped)\b",
        ),
        (
            "billing",
            r"\b(billing|charged|invoice|refund|payment|overcharged|credit card)\b",
        ),
        (
            "being locked out",
            r"\b((can['’]?t|cannot|unable to) (log ?in|sign ?in|access)|locked out|suspended|paused|blocked)\b",
        ),
        (
            "security",
            r"\b(security|vulnerab\w*|leak(ed)?|exposed|breach)\b",
        ),
    ]
    .into_iter()
    .map(|(label, pattern)| (label, Regex::new(pattern).expect("valid impact pattern")))
    .collect()
});

static ALSO_AFFECTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(same (issue|problem|error|here)|me too|also (facing|having|getting|seeing)|happening to me)\b|\+1")
        .expect("valid pattern")
});

static FIXED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(solved|fixed|works now|it works|working now|resolved|figured it out|sorted|that (worked|did it|fixed it)|you['’]?re (right|correct)|(returns?|returned|got|now (get|see)) the expected results?)\b",
    )
    .expect("valid pattern")
});

// Bare thanks only reads as resolved in a short message ("thanks!", "ty that
// was it"); longer ones are usually thanks for looking.
static THANKS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(thanks?|thank you|thx|ty)\b").expect("valid pattern"));
const SHORT_THANKS_WORDS: usize = 6;

// The poster promises a next step, so the thread is still open.
static FOLLOW_UP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(tomorrow|later|(will|i['’]?ll|going to) (send|share|try|test|check|update|get back|post)|get back to you)\b")
        .expect("valid pattern")
});

static STILL_BROKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\?|\b(not|still|doesn['’]?t|don['’]?t|can['’]?t|cannot|but|however|again)\b")
        .expect("valid pattern")
});

// A team message that hands off or promises a look is not an answer.
static PROMISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*cc\b|\b(looking (into|at)|will (check|look|fix|update)|checking|investigating|on it|let me check)\b")
        .expect("valid pattern")
});

// Posters often mark threads solved in the title instead of with the tag.
static SOLVED_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*\[(solved|closed|fixed|resolved)\]").expect("valid pattern")
});

/// Whether the poster marked the thread solved in its title.
pub fn is_solved(title: &str) -> bool {
    SOLVED_TITLE.is_match(title)
}

/// Whether a poster's message reads like the problem is gone.
fn reads_fixed(text: &str) -> bool {
    if STILL_BROKEN.is_match(text) {
        return false;
    }
    FIXED.is_match(text)
        || (THANKS.is_match(text)
            && !FOLLOW_UP.is_match(text)
            && text.split_whitespace().count() <= SHORT_THANKS_WORDS)
}

fn excerpt(text: &str, length: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= length {
        return flat;
    }
    let cut: String = flat.chars().take(length - 1).collect();
    format!("{cut}…")
}

impl SupportThread {
    /// The conversation as plain text, oldest first, for the agent to answer.
    pub fn transcript(&self) -> String {
        let mut text = format!("Support thread: {}\n", self.title);
        if !self.tags.is_empty() {
            text.push_str(&format!("Tags: {}\n", self.tags.join(", ")));
        }
        for message in &self.messages {
            let who = if message.author_id == self.owner_id {
                " (poster)"
            } else {
                ""
            };
            text.push_str(&format!(
                "\n{}{} at {}:\n{}\n",
                message.author,
                who,
                message.timestamp.format("%Y-%m-%d %H:%M UTC"),
                message.content
            ));
        }
        text
    }

    fn speaker(&self, message: &SupportMessage, team: &HashSet<String>) -> Speaker {
        if message.author_id == self.owner_id {
            Speaker::Op
        } else if team.contains(&message.author_id) {
            Speaker::Team
        } else {
            Speaker::Community
        }
    }

    /// Classify and score the thread. `None` when it has no messages.
    pub fn triage(&self, team: &HashSet<String>, now: DateTime<Utc>) -> Option<SupportEntry> {
        let last = self.messages.last()?;
        let speakers: Vec<Speaker> = self
            .messages
            .iter()
            .map(|message| self.speaker(message, team))
            .collect();
        let last_speaker = speakers[speakers.len() - 1];
        let replies = speakers.iter().filter(|s| **s != Speaker::Op).count();
        let team_replied = speakers.contains(&Speaker::Team);

        // The poster's current unanswered streak: every trailing message of theirs.
        let streak = speakers
            .iter()
            .rposition(|s| *s != Speaker::Op)
            .map_or(0, |i| i + 1);
        let waiting_from = if last_speaker == Speaker::Op {
            &self.messages[streak]
        } else {
            last
        };
        let waiting_hours = (now - waiting_from.timestamp).num_hours().max(0);

        let mut reasons = Vec::new();
        let mut score = 0.0;
        let status = if last_speaker == Speaker::Team
            && !(PROMISE.is_match(&last.content) && waiting_hours >= 24)
        {
            reasons.push(format!("team replied last, {}d ago", waiting_hours / 24));
            SupportStatus::WaitingOnUser
        } else if last_speaker == Speaker::Op
            && self.messages.len() > 1
            && reads_fixed(&last.content)
        {
            reasons.push("poster's last message reads like it is fixed".to_string());
            SupportStatus::LikelyResolved
        } else if last_speaker == Speaker::Op
            && replies > 0
            && FOLLOW_UP.is_match(&last.content)
            && !STILL_BROKEN.is_match(&last.content)
        {
            reasons.push(format!(
                "poster said they'd follow up, {}d ago",
                waiting_hours / 24
            ));
            SupportStatus::WaitingOnUser
        } else {
            if last_speaker == Speaker::Team {
                score += 20.0;
                reasons.push(format!(
                    "team said \"{}\" with no follow-up",
                    excerpt(&last.content, 60)
                ));
            } else {
                score += 30.0;
                if replies == 0 {
                    score += 20.0;
                    reasons.push("no replies yet".to_string());
                } else if !team_replied {
                    score += 10.0;
                    reasons.push("no team reply".to_string());
                }
                if last_speaker == Speaker::Community {
                    reasons.push("community member replied last".to_string());
                }
            }
            SupportStatus::NeedsReply
        };

        if status == SupportStatus::NeedsReply {
            score += (waiting_hours as f64 / 24.0).min(7.0) * 3.0;
            if waiting_hours >= 24 {
                reasons.push(format!("waiting {}d", waiting_hours / 24));
            }

            let bumps = self.messages[streak..]
                .windows(2)
                .filter(|pair| pair[1].timestamp - pair[0].timestamp > Duration::hours(2))
                .count();
            if bumps > 0 {
                score += bumps.min(3) as f64 * 6.0;
                reasons.push(format!("poster bumped {bumps}x"));
            }

            let poster_text = std::iter::once(self.title.as_str())
                .chain(
                    self.messages
                        .iter()
                        .zip(&speakers)
                        .filter(|(_, s)| **s == Speaker::Op)
                        .map(|(m, _)| m.content.as_str()),
                )
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
            let impacts: Vec<&str> = IMPACT
                .iter()
                .filter(|(_, pattern)| pattern.is_match(&poster_text))
                .map(|(label, _)| *label)
                .take(3)
                .collect();
            if !impacts.is_empty() {
                score += impacts.len() as f64 * 12.0;
                reasons.push(format!("mentions {}", impacts.join(", ")));
            }

            let affected: HashSet<&str> = self
                .messages
                .iter()
                .zip(&speakers)
                .filter(|(m, s)| **s == Speaker::Community && ALSO_AFFECTED.is_match(&m.content))
                .map(|(m, _)| m.author_id.as_str())
                .collect();
            if !affected.is_empty() {
                score += affected.len().min(2) as f64 * 10.0;
                reasons.push(format!("{} other user(s) report the same", affected.len()));
            }

            if self
                .tags
                .iter()
                .any(|tag| tag.eq_ignore_ascii_case("cloud"))
            {
                score += 5.0;
                reasons.push("Cloud".to_string());
            }
        }

        Some(SupportEntry {
            thread_id: self.id.clone(),
            title: self.title.clone(),
            url: self.url.clone(),
            status,
            score: score.round() as u32,
            reasons,
            last_author: last.author.clone(),
            last_message: excerpt(&last.content, 200),
            waiting_hours,
            is_new: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-10-01T12:00:00Z".parse().unwrap()
    }

    fn team() -> HashSet<String> {
        HashSet::from(["team".to_string()])
    }

    /// A thread by `op` from `(author_id, hours_ago, content)` messages.
    fn thread(messages: &[(&str, i64, &str)]) -> SupportThread {
        SupportThread {
            id: "1".to_string(),
            title: "Function deploy fails".to_string(),
            owner_id: "op".to_string(),
            tags: Vec::new(),
            url: String::new(),
            messages: messages
                .iter()
                .enumerate()
                .map(|(i, (author_id, hours_ago, content))| SupportMessage {
                    id: i.to_string(),
                    author_id: author_id.to_string(),
                    author: author_id.to_string(),
                    content: content.to_string(),
                    timestamp: now() - Duration::hours(*hours_ago),
                })
                .collect(),
        }
    }

    #[test]
    fn test_is_solved() {
        for (title, solved) in [
            ("[SOLVED] Function deploy fails", true),
            ("  [closed] Function deploy fails", true),
            ("[Fixed] Function deploy fails", true),
            ("[RESOLVED] Function deploy fails", true),
            ("[SOLVE] Please help with an outage", false),
            ("Function deploy fails", false),
        ] {
            assert_eq!(is_solved(title), solved, "{title}");
        }
    }

    #[test]
    fn test_triage_status() {
        let asked = ("op", 30, "Deploy fails");
        let answered = ("team", 5, "Set the runtime to node-22 and redeploy");
        for (messages, status) in [
            (vec![asked], SupportStatus::NeedsReply),
            (vec![asked, answered], SupportStatus::WaitingOnUser),
            (
                vec![asked, ("team", 2, "Looking into it")],
                SupportStatus::WaitingOnUser,
            ),
            (
                vec![("op", 50, "Deploy fails"), ("team", 30, "Looking into it")],
                SupportStatus::NeedsReply,
            ),
            (
                vec![asked, answered, ("op", 2, "Thanks, that fixed it")],
                SupportStatus::LikelyResolved,
            ),
            (
                vec![
                    asked,
                    answered,
                    ("op", 2, "It works now, I'll share the solution tomorrow"),
                ],
                SupportStatus::LikelyResolved,
            ),
            (
                vec![asked, answered, ("op", 2, "Thanks, but it still fails")],
                SupportStatus::NeedsReply,
            ),
            (
                vec![asked, answered, ("op", 2, "I'll test it tomorrow")],
                SupportStatus::WaitingOnUser,
            ),
            (
                vec![asked, ("other", 2, "Same issue here")],
                SupportStatus::NeedsReply,
            ),
        ] {
            let entry = thread(&messages).triage(&team(), now()).unwrap();
            assert_eq!(entry.status, status, "{:?}", messages.last());
        }
    }

    #[test]
    fn test_triage_counts_the_unanswered_streak() {
        let entry = thread(&[
            ("op", 100, "Deploy fails"),
            ("op", 96, "Any update?"),
            ("team", 72, "Try redeploying"),
            ("op", 48, "Still failing"),
            ("op", 24, "Any update?"),
            ("op", 0, "Bump"),
        ])
        .triage(&team(), now())
        .unwrap();

        assert_eq!(entry.waiting_hours, 48);
        assert!(entry.reasons.contains(&"poster bumped 2x".to_string()));
    }

    #[test]
    fn test_triage_ranks_impact_higher() {
        let quiet = thread(&[("op", 30, "Deploy fails with an error")])
            .triage(&team(), now())
            .unwrap();
        let urgent = thread(&[("op", 30, "Production is down for our customers")])
            .triage(&team(), now())
            .unwrap();

        assert!(urgent.score > quiet.score);
    }
}
