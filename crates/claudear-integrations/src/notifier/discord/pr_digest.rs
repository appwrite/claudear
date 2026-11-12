use super::pr_allowed_mentions;
use super::DiscordEmbed;
use super::DiscordFooter;
use super::DiscordMarkup;
use super::DiscordMessage;
use crate::discord::AllowedMentions;
use chrono::DateTime;
use chrono::Utc;
use claudear_analysis::distribution::Digest;
use claudear_analysis::distribution::DigestGroup;
use claudear_analysis::distribution::Markup;
use claudear_analysis::distribution::PlainText;
use claudear_analysis::distribution::PullRequestSummary;
use claudear_analysis::distribution::WrappedUp;

/// Discord's cap on an embed description.
pub(super) const DESCRIPTION_LIMIT: usize = 4096;

const TITLE_LIMIT: usize = 100;
const HEADER_COLOR: u32 = 0x5865F2;
const FRESH_COLOR: u32 = 0x2ECC71;
const AGING_COLOR: u32 = 0xF1C40F;
const STALE_COLOR: u32 = 0xE74C3C;
const FRESH_DAYS: i64 = 3;
const STALE_DAYS: i64 = 7;

/// How long a PR has been waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Age {
    Unknown,
    Fresh,
    Aging,
    Stale,
}

impl Age {
    fn of(days: Option<i64>) -> Self {
        match days {
            None => Self::Unknown,
            Some(days) if days < FRESH_DAYS => Self::Fresh,
            Some(days) if days < STALE_DAYS => Self::Aging,
            Some(_) => Self::Stale,
        }
    }

    fn badge(self) -> &'static str {
        match self {
            Self::Unknown => "⚪",
            Self::Fresh => "🟢",
            Self::Aging => "🟡",
            Self::Stale => "🔴",
        }
    }

    fn color(self) -> u32 {
        match self {
            Self::Unknown | Self::Fresh => FRESH_COLOR,
            Self::Aging => AGING_COLOR,
            Self::Stale => STALE_COLOR,
        }
    }
}

/// The card that opens the digest: how many PRs wait on how many people, and
/// how many were wrapped up since the last one. It pings no one.
pub(super) fn header(digest: &Digest) -> (DiscordMessage, AllowedMentions) {
    let count = digest.count();
    let people = digest.groups.len();
    let wrapped: usize = digest
        .groups
        .iter()
        .map(|group| group.wrapped_up.len())
        .sum();
    let mut description = format!(
        "**{count} open PR{}** waiting on **{people} {}**. If you're tagged below, \
         it's yours to get over the line: review it, merge it, or close it.",
        plural(count),
        if people == 1 { "person" } else { "people" },
    );
    if wrapped > 0 {
        description.push_str(&format!(
            "\n\n🎉 **{wrapped} PR{} wrapped up** since the last digest.",
            plural(wrapped)
        ));
    }
    (
        DiscordMessage {
            content: None,
            embeds: Some(vec![DiscordEmbed {
                title: Some("📬 Claudear PR digest".to_string()),
                description: Some(description),
                color: Some(HEADER_COLOR),
                footer: Some(DiscordFooter {
                    text: format!(
                        "{} under {FRESH_DAYS} days  ·  {} under a week  ·  {} a week or more",
                        Age::Fresh.badge(),
                        Age::Aging.badge(),
                        Age::Stale.badge(),
                    ),
                }),
                timestamp: Some(digest.generated_at.to_rfc3339()),
                ..Default::default()
            }]),
        },
        pr_allowed_mentions(None),
    )
}

/// One assignee's card: the mention in the content so it pings, then their
/// waiting PRs oldest first and what they wrapped up. A card longer than an
/// embed description continues in further messages; only the first pings.
pub(super) fn group(
    group: &DigestGroup,
    now: DateTime<Utc>,
) -> Vec<(DiscordMessage, AllowedMentions)> {
    let title = title(group);
    let color = oldest(group, now).color();
    pages(lines(group, now), DESCRIPTION_LIMIT)
        .into_iter()
        .enumerate()
        .map(|(index, page)| {
            let first = index == 0;
            (
                DiscordMessage {
                    content: first.then(|| DiscordMarkup.mention(&group.developer)),
                    embeds: Some(vec![DiscordEmbed {
                        title: Some(if first {
                            title.clone()
                        } else {
                            format!("{title} (continued)")
                        }),
                        description: Some(page),
                        color: Some(color),
                        ..Default::default()
                    }]),
                },
                pr_allowed_mentions(Some(&group.developer)),
            )
        })
        .collect()
}

fn title(group: &DigestGroup) -> String {
    let waiting = group.pull_requests.len();
    if waiting == 0 {
        let wrapped = group.wrapped_up.len();
        return format!("🎉 You wrapped up {wrapped} PR{}", plural(wrapped));
    }
    format!("{waiting} PR{} waiting on you", plural(waiting))
}

fn oldest(group: &DigestGroup, now: DateTime<Utc>) -> Age {
    group
        .pull_requests
        .iter()
        .map(|pull_request| Age::of(days_open(pull_request, now)))
        .max()
        .unwrap_or(Age::Fresh)
}

fn lines(group: &DigestGroup, now: DateTime<Utc>) -> Vec<String> {
    let mut lines: Vec<String> = group
        .pull_requests
        .iter()
        .map(|pull_request| waiting(pull_request, now))
        .collect();
    if !group.wrapped_up.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
            lines.push("**🎉 Wrapped up since the last digest, nice work**".to_string());
        }
        lines.extend(group.wrapped_up.iter().map(wrapped_up));
    }
    lines
}

fn waiting(pull_request: &PullRequestSummary, now: DateTime<Utc>) -> String {
    let days = days_open(pull_request, now);
    let age = days.map_or_else(String::new, |days| format!(" `{days}d`"));
    format!(
        "{}{age} {} {}",
        Age::of(days).badge(),
        link(pull_request),
        title_text(&pull_request.title),
    )
}

fn wrapped_up(wrapped: &WrappedUp) -> String {
    let (badge, outcome) = if wrapped.merged {
        ("✅", "")
    } else {
        ("🚫", " (closed)")
    };
    format!(
        "{badge} {} {}{outcome}",
        link(&wrapped.pull_request),
        title_text(&wrapped.pull_request.title),
    )
}

fn days_open(pull_request: &PullRequestSummary, now: DateTime<Utc>) -> Option<i64> {
    pull_request
        .opened_at
        .map(|opened_at| (now - opened_at).num_days().max(0))
}

fn link(pull_request: &PullRequestSummary) -> String {
    format!("[**{}**]({})", pull_request.label(), pull_request.url)
}

fn title_text(title: &str) -> String {
    let title = PlainText.text(title);
    if title.chars().count() <= TITLE_LIMIT {
        return title;
    }
    let cut: String = title.chars().take(TITLE_LIMIT - 1).collect();
    format!("{}…", cut.trim_end())
}

/// `lines` joined into pages of at most `limit` characters, breaking only
/// between lines; a single line longer than `limit` is cut short.
fn pages(lines: Vec<String>, limit: usize) -> Vec<String> {
    let mut pages = Vec::new();
    let mut page = String::new();
    let mut length = 0;
    for line in lines {
        let line: String = line.chars().take(limit).collect();
        let line_length = line.chars().count();
        if length > 0 && length + 1 + line_length > limit {
            pages.push(std::mem::take(&mut page).trim_end().to_string());
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

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use claudear_analysis::distribution::Developer;

    fn developer(login: &str, discord_id: Option<&str>) -> Developer {
        Developer {
            login: login.to_string(),
            discord_id: discord_id.map(str::to_string),
            slack_id: None,
        }
    }

    fn pull_request(number: i64, days: Option<i64>, now: DateTime<Utc>) -> PullRequestSummary {
        PullRequestSummary {
            url: format!("https://github.com/org/repo/pull/{number}"),
            repo: "org/repo".to_string(),
            number,
            title: format!("Fix {number}"),
            opened_at: days.map(|days| now - Duration::days(days)),
        }
    }

    fn description(message: &DiscordMessage) -> &str {
        message.embeds.as_ref().unwrap()[0]
            .description
            .as_deref()
            .unwrap()
    }

    #[test]
    fn a_card_pings_its_assignee_and_lists_their_prs_with_their_age() {
        let now = Utc::now();
        let group = DigestGroup {
            developer: developer("amy", Some("1")),
            pull_requests: vec![
                pull_request(1, Some(9), now),
                pull_request(2, Some(4), now),
                pull_request(3, Some(0), now),
            ],
            wrapped_up: Vec::new(),
        };

        let messages = super::group(&group, now);

        assert_eq!(messages.len(), 1);
        let (message, allowed_mentions) = &messages[0];
        assert_eq!(message.content.as_deref(), Some("<@1>"));
        assert_eq!(allowed_mentions.users, vec!["1".to_string()]);
        let embed = &message.embeds.as_ref().unwrap()[0];
        assert_eq!(embed.title.as_deref(), Some("3 PRs waiting on you"));
        assert_eq!(embed.color, Some(STALE_COLOR));
        assert_eq!(
            description(message),
            "🔴 `9d` [**org/repo#1**](https://github.com/org/repo/pull/1) Fix 1\n\
             🟡 `4d` [**org/repo#2**](https://github.com/org/repo/pull/2) Fix 2\n\
             🟢 `0d` [**org/repo#3**](https://github.com/org/repo/pull/3) Fix 3"
        );
    }

    #[test]
    fn a_card_rounds_up_what_its_assignee_wrapped_up() {
        let now = Utc::now();
        let group = DigestGroup {
            developer: developer("amy", Some("1")),
            pull_requests: vec![pull_request(1, Some(1), now)],
            wrapped_up: vec![
                WrappedUp {
                    pull_request: pull_request(2, None, now),
                    merged: true,
                },
                WrappedUp {
                    pull_request: pull_request(3, None, now),
                    merged: false,
                },
            ],
        };

        let messages = super::group(&group, now);

        assert_eq!(
            description(&messages[0].0),
            "🟢 `1d` [**org/repo#1**](https://github.com/org/repo/pull/1) Fix 1\n\
             \n\
             **🎉 Wrapped up since the last digest, nice work**\n\
             ✅ [**org/repo#2**](https://github.com/org/repo/pull/2) Fix 2\n\
             🚫 [**org/repo#3**](https://github.com/org/repo/pull/3) Fix 3 (closed)"
        );
    }

    #[test]
    fn a_card_with_only_a_roundup_celebrates_it() {
        let now = Utc::now();
        let group = DigestGroup {
            developer: developer("amy", Some("1")),
            pull_requests: Vec::new(),
            wrapped_up: vec![WrappedUp {
                pull_request: pull_request(2, None, now),
                merged: true,
            }],
        };

        let (message, _) = &super::group(&group, now)[0];
        let embed = &message.embeds.as_ref().unwrap()[0];

        assert_eq!(embed.title.as_deref(), Some("🎉 You wrapped up 1 PR"));
        assert_eq!(embed.color, Some(FRESH_COLOR));
        assert!(!description(message).contains("Wrapped up since"));
    }

    #[test]
    fn a_long_card_continues_across_messages_that_fit_discord() {
        let now = Utc::now();
        let group = DigestGroup {
            developer: developer("amy", Some("1")),
            pull_requests: (1..=200)
                .map(|number| pull_request(number, Some(2), now))
                .collect(),
            wrapped_up: Vec::new(),
        };

        let messages = super::group(&group, now);

        assert!(messages.len() > 1);
        assert!(messages
            .iter()
            .all(|(message, _)| description(message).chars().count() <= DESCRIPTION_LIMIT));
        assert_eq!(messages[0].0.content.as_deref(), Some("<@1>"));
        assert!(messages[1..]
            .iter()
            .all(|(message, _)| message.content.is_none()));
        assert_eq!(
            messages[1].0.embeds.as_ref().unwrap()[0].title.as_deref(),
            Some("200 PRs waiting on you (continued)")
        );
        let listed: usize = messages
            .iter()
            .map(|(message, _)| description(message).lines().count())
            .sum();
        assert_eq!(listed, 200);
    }

    #[test]
    fn a_long_title_is_cut_and_never_pings() {
        let now = Utc::now();
        let mut pull_request = pull_request(1, Some(1), now);
        pull_request.title = format!("@everyone {}", "x".repeat(300));
        let group = DigestGroup {
            developer: developer("amy", None),
            pull_requests: vec![pull_request],
            wrapped_up: Vec::new(),
        };

        let (message, allowed_mentions) = &super::group(&group, now)[0];

        assert!(!description(message).contains("@everyone"));
        assert!(description(message).ends_with('…'));
        assert!(allowed_mentions.users.is_empty());
        assert_eq!(message.content.as_deref(), Some("@amy"));
    }

    #[test]
    fn the_header_counts_prs_people_and_wrap_ups_without_pinging() {
        let now = Utc::now();
        let digest = Digest {
            groups: vec![
                DigestGroup {
                    developer: developer("amy", Some("1")),
                    pull_requests: vec![
                        pull_request(1, Some(1), now),
                        pull_request(2, Some(1), now),
                    ],
                    wrapped_up: vec![WrappedUp {
                        pull_request: pull_request(3, None, now),
                        merged: true,
                    }],
                },
                DigestGroup {
                    developer: developer("bob", Some("2")),
                    pull_requests: vec![pull_request(4, Some(1), now)],
                    wrapped_up: Vec::new(),
                },
            ],
            generated_at: now,
        };

        let (message, allowed_mentions) = header(&digest);

        assert!(message.content.is_none());
        assert!(allowed_mentions.users.is_empty());
        let text = description(&message);
        assert!(text.starts_with("**3 open PRs** waiting on **2 people**."));
        assert!(text.ends_with("🎉 **1 PR wrapped up** since the last digest."));
    }
}
