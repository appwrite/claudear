//! Messages telling developers which PRs they are responsible for.

use crate::distribution::Developer;
use crate::distribution::PullRequest;
use chrono::DateTime;
use chrono::Utc;
use claudear_core::types::AssignmentReason;

/// How a channel mentions a developer and links a URL.
pub trait Markup {
    /// Text that mentions `developer`.
    fn mention(&self, developer: &Developer) -> String;
    /// Text linking `url` as `text`.
    fn link(&self, text: &str, url: &str) -> String;
}

/// Markup for channels with no native mentions or links.
pub struct PlainText;

impl Markup for PlainText {
    fn mention(&self, developer: &Developer) -> String {
        developer.handle()
    }

    fn link(&self, text: &str, url: &str) -> String {
        format!("{text} ({url})")
    }
}

/// The parts of a PR a message shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSummary {
    /// Full PR URL.
    pub url: String,
    /// `owner/repo`.
    pub repo: String,
    /// PR number.
    pub number: i64,
    /// PR title.
    pub title: String,
    /// When the PR was opened.
    pub opened_at: Option<DateTime<Utc>>,
}

impl PullRequestSummary {
    /// `owner/repo#number`.
    pub fn label(&self) -> String {
        format!("{}#{}", self.repo, self.number)
    }

    /// One digest line: the linked PR, its title, and how long it has been open.
    pub fn line(&self, markup: &dyn Markup, now: DateTime<Utc>) -> String {
        let link = markup.link(&self.label(), &self.url);
        match self.opened_at {
            Some(opened_at) => {
                let days = (now - opened_at).num_days().max(0);
                format!("• {link} {} (open {days}d)", self.title)
            }
            None => format!("• {link} {}", self.title),
        }
    }
}

impl From<&PullRequest> for PullRequestSummary {
    fn from(pull_request: &PullRequest) -> Self {
        Self {
            url: pull_request.url.clone(),
            repo: pull_request.repo.clone(),
            number: pull_request.number,
            title: pull_request.title.clone(),
            opened_at: pull_request.created_at,
        }
    }
}

/// A developer was just made responsible for a PR.
#[derive(Debug, Clone, PartialEq)]
pub struct AssignmentNotice {
    /// The new assignee.
    pub developer: Developer,
    /// The PR they now own.
    pub pull_request: PullRequestSummary,
    /// Why they were chosen.
    pub reason: AssignmentReason,
    /// Login of the assignee they took over from, when the PR was reassigned.
    pub previous: Option<String>,
}

impl AssignmentNotice {
    /// The notice as `markup` text.
    pub fn render(&self, markup: &dyn Markup) -> String {
        let mut text = format!(
            "{} you're responsible for getting {} over the line: {}\n{}",
            markup.mention(&self.developer),
            markup.link(&self.pull_request.label(), &self.pull_request.url),
            self.pull_request.title,
            explain(self.reason),
        );
        if let Some(previous) = &self.previous {
            text.push_str(&format!(" Taking over from @{previous}."));
        }
        text
    }

    /// The notice as plain text.
    pub fn format_text(&self) -> String {
        self.render(&PlainText)
    }
}

/// The PRs one developer is responsible for that still need them.
#[derive(Debug, Clone, PartialEq)]
pub struct DigestGroup {
    /// The assignee.
    pub developer: Developer,
    /// Their PRs, oldest first.
    pub pull_requests: Vec<PullRequestSummary>,
}

impl DigestGroup {
    /// The line that mentions the assignee.
    pub fn heading(&self, markup: &dyn Markup) -> String {
        let count = self.pull_requests.len();
        format!(
            "{} {count} Claudear PR{} waiting on you:",
            markup.mention(&self.developer),
            if count == 1 { "" } else { "s" }
        )
    }

    /// One line per PR.
    pub fn lines(&self, markup: &dyn Markup, now: DateTime<Utc>) -> Vec<String> {
        self.pull_requests
            .iter()
            .map(|pull_request| pull_request.line(markup, now))
            .collect()
    }

    /// The group as `markup` text.
    pub fn render(&self, markup: &dyn Markup, now: DateTime<Utc>) -> String {
        let mut lines = vec![self.heading(markup)];
        lines.extend(self.lines(markup, now));
        lines.join("\n")
    }

    /// The group as `markup` text split into pages of at most `limit`
    /// characters, for channels that cap message length. The first page opens
    /// with the heading; a single line longer than `limit` is cut short.
    pub fn pages(&self, markup: &dyn Markup, now: DateTime<Utc>, limit: usize) -> Vec<String> {
        let mut pages = Vec::new();
        let mut page = String::new();
        let mut length = 0;
        for line in std::iter::once(self.heading(markup)).chain(self.lines(markup, now)) {
            let line: String = line.chars().take(limit).collect();
            let line_length = line.chars().count();
            if length > 0 && length + 1 + line_length > limit {
                pages.push(std::mem::take(&mut page));
                length = 0;
            }
            if length > 0 {
                page.push('\n');
                length += 1;
            }
            page.push_str(&line);
            length += line_length;
        }
        if length > 0 {
            pages.push(page);
        }
        pages
    }
}

/// Every assignee's open Claudear PRs that still need them.
#[derive(Debug, Clone, PartialEq)]
pub struct Digest {
    /// One group per assignee, by login.
    pub groups: Vec<DigestGroup>,
    /// When the digest was built; PR ages are measured from it.
    pub generated_at: DateTime<Utc>,
}

impl Digest {
    /// The line that opens the digest.
    pub const TITLE: &str =
        "Open Claudear PRs: if you're tagged, it's your responsibility to get the PR over the line.";

    /// Whether no PR needs anyone.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Number of PRs across all groups.
    pub fn count(&self) -> usize {
        self.groups
            .iter()
            .map(|group| group.pull_requests.len())
            .sum()
    }

    /// The digest as `markup` text.
    pub fn render(&self, markup: &dyn Markup) -> String {
        let mut sections = vec![Self::TITLE.to_string()];
        sections.extend(
            self.groups
                .iter()
                .map(|group| group.render(markup, self.generated_at)),
        );
        sections.join("\n\n")
    }

    /// The digest as plain text.
    pub fn format_text(&self) -> String {
        self.render(&PlainText)
    }
}

fn explain(reason: AssignmentReason) -> &'static str {
    match reason {
        AssignmentReason::Requester => "You asked for this work.",
        AssignmentReason::CodeOwner => "You own the changed code.",
        AssignmentReason::Author => "You recently worked on the changed files.",
        AssignmentReason::Activity => "You're active in this repository.",
        AssignmentReason::Load => "Shared out to balance the review load.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use chrono::TimeZone;

    struct Chat;

    impl Markup for Chat {
        fn mention(&self, developer: &Developer) -> String {
            format!("<@{}>", developer.discord_id.as_deref().unwrap_or("?"))
        }

        fn link(&self, text: &str, url: &str) -> String {
            format!("[{text}]({url})")
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap()
    }

    fn developer(login: &str, discord_id: &str) -> Developer {
        Developer {
            login: login.into(),
            discord_id: Some(discord_id.into()),
            slack_id: None,
        }
    }

    fn summary(number: i64, days_open: Option<i64>) -> PullRequestSummary {
        PullRequestSummary {
            url: format!("https://github.com/org/repo/pull/{number}"),
            repo: "org/repo".into(),
            number,
            title: format!("Fix {number}"),
            opened_at: days_open.map(|days| now() - Duration::days(days)),
        }
    }

    #[test]
    fn notice_mentions_the_assignee_and_links_the_pr() {
        let notice = AssignmentNotice {
            developer: developer("amy", "42"),
            pull_request: summary(7, None),
            reason: AssignmentReason::CodeOwner,
            previous: None,
        };

        assert_eq!(
            notice.render(&Chat),
            "<@42> you're responsible for getting \
             [org/repo#7](https://github.com/org/repo/pull/7) over the line: Fix 7\n\
             You own the changed code."
        );
        assert_eq!(
            notice.format_text(),
            "@amy you're responsible for getting \
             org/repo#7 (https://github.com/org/repo/pull/7) over the line: Fix 7\n\
             You own the changed code."
        );
    }

    #[test]
    fn reassigned_notice_names_the_previous_assignee() {
        let notice = AssignmentNotice {
            developer: developer("amy", "42"),
            pull_request: summary(7, None),
            reason: AssignmentReason::Load,
            previous: Some("bob".into()),
        };

        assert!(notice
            .format_text()
            .ends_with("Shared out to balance the review load. Taking over from @bob."));
    }

    #[test]
    fn digest_groups_mention_each_assignee_with_their_prs_and_ages() {
        let digest = Digest {
            groups: vec![
                DigestGroup {
                    developer: developer("amy", "1"),
                    pull_requests: vec![summary(1, Some(3)), summary(2, None)],
                },
                DigestGroup {
                    developer: developer("bob", "2"),
                    pull_requests: vec![summary(3, Some(0))],
                },
            ],
            generated_at: now(),
        };

        assert_eq!(digest.count(), 3);
        assert_eq!(
            digest.render(&Chat),
            format!(
                "{}\n\n\
                 <@1> 2 Claudear PRs waiting on you:\n\
                 • [org/repo#1](https://github.com/org/repo/pull/1) Fix 1 (open 3d)\n\
                 • [org/repo#2](https://github.com/org/repo/pull/2) Fix 2\n\n\
                 <@2> 1 Claudear PR waiting on you:\n\
                 • [org/repo#3](https://github.com/org/repo/pull/3) Fix 3 (open 0d)",
                Digest::TITLE
            )
        );
        assert!(digest
            .format_text()
            .contains("@bob 1 Claudear PR waiting on you:"));
    }

    #[test]
    fn pages_split_on_lines_and_keep_the_heading_first() {
        let group = DigestGroup {
            developer: developer("amy", "1"),
            pull_requests: (1..=3).map(|number| summary(number, None)).collect(),
        };
        let heading = group.heading(&PlainText);
        let lines = group.lines(&PlainText, now());
        let limit = heading.chars().count() + 1 + lines[0].chars().count();

        let pages = group.pages(&PlainText, now(), limit);

        assert_eq!(
            pages,
            vec![
                format!("{heading}\n{}", lines[0]),
                lines[1].clone(),
                lines[2].clone(),
            ]
        );
        assert!(pages.iter().all(|page| page.chars().count() <= limit));
        assert_eq!(pages.join("\n"), group.render(&PlainText, now()));
    }

    #[test]
    fn pages_cut_a_line_longer_than_the_limit() {
        let group = DigestGroup {
            developer: developer("amy", "1"),
            pull_requests: vec![summary(1, None)],
        };

        let pages = group.pages(&PlainText, now(), 10);

        assert!(pages.iter().all(|page| page.chars().count() <= 10));
        assert_eq!(pages[0], "@amy 1 Cla");
    }

    #[test]
    fn empty_digest_is_empty() {
        let digest = Digest {
            groups: Vec::new(),
            generated_at: now(),
        };
        assert!(digest.is_empty());
        assert_eq!(digest.count(), 0);
    }
}
