//! The developers open PRs can be assigned to.

use crate::deploy_qa::GitHubDiscordMap;
use crate::distribution::Developer;
use claudear_config::config::UserConfig;
use claudear_core::types::Issue;
use std::collections::HashMap;

/// Issue metadata holding the `[users]` slug the issue's assignee resolved to.
const RESOLVED_USER_KEY: &str = "resolved_user";
/// Issue metadata holding the source's assignee.
const ASSIGNEE_KEY: &str = "assignee";
/// Issue metadata holding the GitHub login that opened a GitHub issue.
const AUTHOR_KEY: &str = "author";
/// Issue metadata holding the chat user id that posted a Discord or Slack request.
const AUTHOR_ID_KEY: &str = "author_id";

/// Sources whose assignee and author are GitHub logins.
const GITHUB_SOURCES: [&str; 2] = ["github", "github_issues"];
const DISCORD_SOURCE: &str = "discord";
const SLACK_SOURCE: &str = "slack";

/// Every developer eligible for PR assignments.
///
/// Built from the `[users]` entries that have a GitHub username and from the
/// GitHub↔Discord map, minus excluded logins. GitHub logins are matched
/// without regard to case, and developers are kept sorted by login so every
/// run walks them in the same order.
#[derive(Debug, Clone, Default)]
pub struct Roster {
    developers: Vec<Developer>,
    logins: HashMap<String, usize>,
    slugs: HashMap<String, usize>,
}

struct Entry {
    developer: Developer,
    aliases: Vec<String>,
    slugs: Vec<String>,
}

impl Roster {
    /// Build the roster from `[users]` and the GitHub↔Discord map.
    ///
    /// A `[users]` entry's first GitHub username is its canonical login and the
    /// rest are aliases. A map login that matches a `[users]` entry only fills
    /// in a missing Discord id.
    pub fn new(
        users: &HashMap<String, UserConfig>,
        map: &GitHubDiscordMap,
        excluded: &[String],
    ) -> Self {
        let is_excluded = |login: &str| {
            excluded
                .iter()
                .any(|excluded| excluded.trim().eq_ignore_ascii_case(login))
        };
        let mut entries: Vec<Entry> = Vec::new();
        let mut known: HashMap<String, usize> = HashMap::new();

        let mut slugs: Vec<&String> = users.keys().collect();
        slugs.sort();
        for slug in slugs {
            let user = &users[slug];
            let aliases: Vec<String> = user
                .github_usernames
                .iter()
                .map(|login| login.trim().to_string())
                .filter(|login| !login.is_empty())
                .collect();
            let Some(login) = aliases.first() else {
                continue;
            };
            if aliases.iter().any(|alias| is_excluded(alias)) {
                continue;
            }
            let existing = aliases
                .iter()
                .find_map(|alias| known.get(&alias.to_lowercase()).copied());
            let index = existing.unwrap_or_else(|| {
                entries.push(Entry {
                    developer: Developer::new(login.clone()),
                    aliases: Vec::new(),
                    slugs: Vec::new(),
                });
                entries.len() - 1
            });
            let entry = &mut entries[index];
            let developer = &mut entry.developer;
            developer.discord_id = developer
                .discord_id
                .take()
                .or_else(|| non_empty(user.discord_id.as_deref()));
            developer.slack_id = developer
                .slack_id
                .take()
                .or_else(|| non_empty(user.slack_id.as_deref()));
            entry.slugs.push(slug.clone());
            for alias in aliases {
                known.insert(alias.to_lowercase(), index);
                entry.aliases.push(alias);
            }
        }

        let mut logins: Vec<&String> = map.by_github_login.keys().collect();
        logins.sort();
        for login in logins {
            let discord_id = non_empty(Some(&map.by_github_login[login].discord_user_id));
            let login = login.trim();
            if login.is_empty() || is_excluded(login) {
                continue;
            }
            match known.get(&login.to_lowercase()) {
                Some(&index) => {
                    let developer = &mut entries[index].developer;
                    developer.discord_id = developer.discord_id.take().or(discord_id);
                }
                None => {
                    let mut developer = Developer::new(login);
                    developer.discord_id = discord_id;
                    known.insert(login.to_lowercase(), entries.len());
                    entries.push(Entry {
                        developer,
                        aliases: vec![login.to_string()],
                        slugs: Vec::new(),
                    });
                }
            }
        }

        Self::from_entries(entries)
    }

    /// A roster of exactly `developers`.
    pub fn from_developers(developers: Vec<Developer>) -> Self {
        Self::from_entries(
            developers
                .into_iter()
                .map(|developer| Entry {
                    aliases: vec![developer.login.clone()],
                    developer,
                    slugs: Vec::new(),
                })
                .collect(),
        )
    }

    fn from_entries(mut entries: Vec<Entry>) -> Self {
        entries.sort_by_key(|entry| entry.developer.login.to_lowercase());
        let mut roster = Self::default();
        for entry in entries {
            let developer = entry.developer;
            let index = roster.developers.len();
            if roster
                .logins
                .insert(developer.login.to_lowercase(), index)
                .is_some()
            {
                continue;
            }
            for alias in entry.aliases {
                roster.logins.insert(alias.to_lowercase(), index);
            }
            for slug in entry.slugs {
                roster.slugs.insert(slug, index);
            }
            roster.developers.push(developer);
        }
        roster
    }

    /// Every developer, sorted by login.
    pub fn developers(&self) -> &[Developer] {
        &self.developers
    }

    /// Number of developers.
    pub fn len(&self) -> usize {
        self.developers.len()
    }

    /// Whether nobody is eligible.
    pub fn is_empty(&self) -> bool {
        self.developers.is_empty()
    }

    /// The developer with GitHub `login` or one of its aliases.
    pub fn find(&self, login: &str) -> Option<&Developer> {
        self.logins
            .get(&login.trim().to_lowercase())
            .map(|&index| &self.developers[index])
    }

    /// The developer configured under `[users.<slug>]`.
    pub fn by_slug(&self, slug: &str) -> Option<&Developer> {
        self.slugs.get(slug).map(|&index| &self.developers[index])
    }

    /// The developer with Discord user id `id`.
    pub fn by_discord_id(&self, id: &str) -> Option<&Developer> {
        self.developers
            .iter()
            .find(|developer| developer.discord_id.as_deref() == Some(id))
    }

    /// The developer with Slack user id `id`.
    pub fn by_slack_id(&self, id: &str) -> Option<&Developer> {
        self.developers
            .iter()
            .find(|developer| developer.slack_id.as_deref() == Some(id))
    }

    /// Logins of the developers who asked for `issue`'s work, closest first.
    ///
    /// The resolved `[users]` assignee comes first, then a GitHub issue's
    /// assignee and author, then the Discord or Slack user who posted the
    /// request. Only developers on the roster are returned.
    pub fn requesters(&self, issue: &Issue) -> Vec<String> {
        let metadata = |key: &str| {
            issue
                .get_metadata::<String>(key)
                .filter(|value| !value.trim().is_empty())
        };
        let source = issue.source.to_lowercase();
        let mut found: Vec<&Developer> = Vec::new();

        if let Some(slug) = metadata(RESOLVED_USER_KEY) {
            found.extend(self.by_slug(&slug));
        }
        if GITHUB_SOURCES.contains(&source.as_str()) {
            for key in [ASSIGNEE_KEY, AUTHOR_KEY] {
                if let Some(login) = metadata(key) {
                    found.extend(self.find(&login));
                }
            }
        }
        if let Some(id) = metadata(AUTHOR_ID_KEY) {
            match source.as_str() {
                DISCORD_SOURCE => found.extend(self.by_discord_id(&id)),
                SLACK_SOURCE => found.extend(self.by_slack_id(&id)),
                _ => {}
            }
        }

        let mut requesters: Vec<String> = Vec::new();
        for developer in found {
            if !requesters.contains(&developer.login) {
                requesters.push(developer.login.clone());
            }
        }
        requesters
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(logins: &[&str], discord_id: Option<&str>, slack_id: Option<&str>) -> UserConfig {
        UserConfig {
            github_usernames: logins.iter().map(|login| login.to_string()).collect(),
            discord_id: discord_id.map(str::to_string),
            slack_id: slack_id.map(str::to_string),
            ..UserConfig::default()
        }
    }

    fn map(entries: &[(&str, &str)]) -> GitHubDiscordMap {
        let by_github_login: serde_json::Map<String, serde_json::Value> = entries
            .iter()
            .map(|(login, id)| {
                (
                    login.to_string(),
                    serde_json::json!({ "discordUserId": id }),
                )
            })
            .collect();
        GitHubDiscordMap::from_json_bytes(
            serde_json::json!({ "byGithubLogin": by_github_login })
                .to_string()
                .as_bytes(),
        )
        .unwrap()
    }

    fn users(entries: Vec<(&str, UserConfig)>) -> HashMap<String, UserConfig> {
        entries
            .into_iter()
            .map(|(slug, user)| (slug.to_string(), user))
            .collect()
    }

    fn logins(roster: &Roster) -> Vec<&str> {
        roster
            .developers()
            .iter()
            .map(|developer| developer.login.as_str())
            .collect()
    }

    #[test]
    fn merges_users_and_the_discord_map_sorted_by_login() {
        let roster = Roster::new(
            &users(vec![
                ("zed", user(&["zed"], None, Some("U_ZED"))),
                ("amy", user(&["Amy", "amy-alt"], Some("1"), None)),
            ]),
            &map(&[("ZED", "26"), ("bob", "2"), ("amy", "99")]),
            &[],
        );

        assert_eq!(logins(&roster), vec!["Amy", "bob", "zed"]);
        let amy = roster.find("AMY-ALT").unwrap();
        assert_eq!(amy.login, "Amy");
        assert_eq!(amy.discord_id.as_deref(), Some("1"));
        let zed = roster.find("zed").unwrap();
        assert_eq!(zed.discord_id.as_deref(), Some("26"));
        assert_eq!(zed.slack_id.as_deref(), Some("U_ZED"));
        assert_eq!(roster.by_slug("amy"), Some(amy));
        assert_eq!(roster.by_discord_id("2").unwrap().login, "bob");
        assert_eq!(roster.by_slack_id("U_ZED").unwrap().login, "zed");
    }

    #[test]
    fn skips_users_without_a_github_username_and_excluded_logins() {
        let roster = Roster::new(
            &users(vec![
                ("nobody", user(&[], Some("5"), None)),
                ("away", user(&["main", "Away-Alt"], None, None)),
            ]),
            &map(&[("Gone", "3"), ("here", "4")]),
            &["away-alt".to_string(), " gone ".to_string()],
        );

        assert_eq!(logins(&roster), vec!["here"]);
        assert!(roster.find("main").is_none());
    }

    #[test]
    fn ignores_blank_discord_ids() {
        let roster = Roster::new(&HashMap::new(), &map(&[("amy", " ")]), &[]);
        assert_eq!(roster.find("amy").unwrap().discord_id, None);
    }

    #[test]
    fn requesters_follow_resolved_user_then_github_then_chat_author() {
        let roster = Roster::from_developers(vec![
            Developer {
                login: "amy".into(),
                discord_id: Some("1".into()),
                slack_id: Some("U1".into()),
            },
            Developer::new("bob"),
            Developer::new("cat"),
        ]);

        let mut github = Issue::new("o/r#1", "o/r#1", "Title", "url", "github_issues");
        github.set_metadata("assignee", "BOB");
        github.set_metadata("author", "cat");
        github.set_metadata("resolved_user", "unknown-slug");
        assert_eq!(roster.requesters(&github), vec!["bob", "cat"]);

        let mut discord = Issue::new("1", "D-1", "Title", "url", "discord");
        discord.set_metadata("author_id", "1");
        discord.set_metadata("author", "bob");
        assert_eq!(roster.requesters(&discord), vec!["amy"]);

        let mut slack = Issue::new("1", "S-1", "Title", "url", "slack");
        slack.set_metadata("author_id", "U1");
        assert_eq!(roster.requesters(&slack), vec!["amy"]);

        let mut sentry = Issue::new("1", "S-1", "Title", "url", "sentry");
        sentry.set_metadata("author_id", "1");
        assert!(roster.requesters(&sentry).is_empty());
    }

    #[test]
    fn requesters_resolve_the_users_slug_first_and_deduplicate() {
        let roster = Roster::new(
            &users(vec![("jake", user(&["abnegate"], None, None))]),
            &map(&[("bob", "2")]),
            &[],
        );
        let mut issue = Issue::new("o/r#1", "o/r#1", "Title", "url", "github");
        issue.set_metadata("resolved_user", "jake");
        issue.set_metadata("assignee", "bob");
        issue.set_metadata("author", "Abnegate");

        assert_eq!(roster.requesters(&issue), vec!["abnegate", "bob"]);
    }
}
