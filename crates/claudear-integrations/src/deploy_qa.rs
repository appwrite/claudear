//! Discord `#releases` posting for `[deploy_qa]` outcomes.
//!
//! All verified or unverified: reply under the automated release announcement
//! (no @). FAIL: post in the announcement's thread and `@` the mapped releaser.
//! GitHub's release feed never announces a bare tag, so Claudear announces it
//! itself ([`DeployQaAnnouncer`]) and the outcome replies under that. A tip
//! whose announcement cannot be found gets a post of its own in the channel
//! instead, with the same `@` rule.

use async_trait::async_trait;
use claudear_analysis::deploy_qa::{
    classify_deploy_qa_verdict, DeployQaAnnouncement, DeployQaAnnouncer, DeployQaVerdict,
    GitHubDiscordMap, ANNOUNCEMENT_FOOTER, DEPLOY_QA_SOURCE,
};
use claudear_config::UserRegistry;
use claudear_core::error::Result;
use claudear_core::secret::Redactor;
use claudear_core::types::{DeployQaTip, DeployQaTipStatus};
use claudear_storage::DeployQaStore;

use crate::discord::{
    CreateMessageParams, CreateThreadParams, DiscordClient, DiscordHttpClient, DiscordMessage,
    MessageEmbed, ReqwestDiscordClient,
};

const COLOR_VERIFIED: u32 = 0x2ecc71;
const COLOR_UNVERIFIED: u32 = 0xffbf00;
const COLOR_FAIL: u32 = 0xe74c3c;
const COLOR_ANNOUNCEMENT: u32 = 0x24292f;

/// [`UserRegistry::resolve`] source whose usernames are GitHub logins.
const GITHUB_USER_SOURCE: &str = "github";

const TITLE_VERIFIED: &str = "All PRs verified";
const TITLE_UNVERIFIED: &str = "Not fully verified";

/// Recent `#releases` messages searched for the release announcement.
const RELEASE_SEARCH_PAGE_SIZE: usize = 50;

/// Longest posted report in characters, under Discord's 4096-character embed
/// description limit.
const REPORT_CHARACTER_LIMIT: usize = 4000;

const ELLIPSIS: char = '…';

/// Discord reporter for deploy-QA results.
///
/// Reports come from an agent with shell and browser access, so every report
/// passes through `redactor` before it is posted.
pub struct DeployQaDiscord<H: DiscordHttpClient = ReqwestDiscordClient> {
    client: DiscordClient<H>,
    channel_id: String,
    map: GitHubDiscordMap,
    users: UserRegistry,
    redactor: Redactor,
}

impl DeployQaDiscord<ReqwestDiscordClient> {
    /// Create a reporter with the default Discord HTTP client.
    pub fn new(
        bot_token: impl Into<String>,
        channel_id: impl Into<String>,
        map: GitHubDiscordMap,
        redactor: Redactor,
    ) -> Result<Self> {
        Ok(Self {
            client: DiscordClient::new(bot_token)?,
            channel_id: channel_id.into(),
            map,
            users: UserRegistry::default(),
            redactor,
        })
    }
}

impl<H: DiscordHttpClient> DeployQaDiscord<H> {
    /// Create a reporter with a custom Discord HTTP client (tests).
    pub fn with_client(
        client: DiscordClient<H>,
        channel_id: impl Into<String>,
        map: GitHubDiscordMap,
        redactor: Redactor,
    ) -> Self {
        Self {
            client,
            channel_id: channel_id.into(),
            map,
            users: UserRegistry::default(),
            redactor,
        }
    }

    /// Also resolve releasers through `[users]`, for GitHub logins the
    /// GitHub↔Discord map does not name.
    pub fn with_users(mut self, users: UserRegistry) -> Self {
        self.users = users;
        self
    }

    /// Discord mention for a GitHub login: the GitHub↔Discord map first, then
    /// a `[users]` entry listing the login in `github_usernames`.
    pub fn mention_for_login(&self, login: &str) -> Option<String> {
        self.map.mention_for(login).or_else(|| {
            self.users
                .resolve(GITHUB_USER_SOURCE, login)
                .and_then(|user| user.discord_id)
                .map(|discord_id| format!("<@{discord_id}>"))
        })
    }

    /// Claudear's own announcement for `announcement`'s tag among the
    /// channel's recent messages.
    async fn find_announcement(
        &self,
        announcement: &DeployQaAnnouncement,
    ) -> Result<Option<String>> {
        let messages = self
            .client
            .list_channel_messages(&self.channel_id, RELEASE_SEARCH_PAGE_SIZE)
            .await?;
        Ok(messages
            .into_iter()
            .find(|message| {
                is_automated(message)
                    && message.embeds.iter().any(|embed| {
                        embed
                            .footer
                            .as_ref()
                            .is_some_and(|footer| footer.text == ANNOUNCEMENT_FOOTER)
                            && embed
                                .url
                                .as_deref()
                                .is_some_and(|url| same_url(url, &announcement.tag_url))
                    })
            })
            .map(|message| message.id))
    }

    fn announcement_embed(&self, announcement: &DeployQaAnnouncement) -> MessageEmbed {
        let releaser = match announcement.releaser_login.as_deref() {
            Some(login) => match self.mention_for_login(login) {
                Some(mention) => format!("{mention} (`{login}`)"),
                None => format!("`{login}`"),
            },
            None => "unknown".to_string(),
        };
        let mut embed = MessageEmbed::new()
            .title(format!(
                "[{}] New {} release: {}",
                announcement.repo, announcement.track, announcement.tag
            ))
            .url(&announcement.tag_url)
            .color(COLOR_ANNOUNCEMENT)
            .field("Version", format!("`{}`", announcement.version), true)
            .field("Released by", releaser, true);
        if let (Some(previous), Some(compare_url)) = (
            announcement.previous_tag.as_deref(),
            announcement.compare_url(),
        ) {
            embed = embed.field(
                "Changes",
                format!("[{previous}...{}]({compare_url})", announcement.tag),
                false,
            );
        }
        if let Some(run) = announcement.deploy_run.as_ref() {
            embed = embed.field("Deploy", format!("[{}]({})", run.name, run.url), false);
        }
        embed
            .footer(ANNOUNCEMENT_FOOTER)
            .timestamp(chrono::Utc::now().to_rfc3339())
    }

    /// Find the automated `#releases` announcement for `tip`.
    ///
    /// Only webhook or bot posts are candidates, never Claudear's own deploy-QA
    /// posts. The newest candidate with an embed linking the release's
    /// `html_url` wins across the whole page; failing that, the newest whose
    /// content or embed title names both the `owner/repo` and the tag as whole
    /// tokens, so `1.2.3` never matches `1.2.3-db`, `1.2.30`, `1.2.3-rc.1` or
    /// a later release's `1.2.3...1.2.4` changelog link.
    pub async fn find_release_message(&self, tip: &DeployQaTip) -> Result<Option<String>> {
        let messages = self
            .client
            .list_channel_messages(&self.channel_id, RELEASE_SEARCH_PAGE_SIZE)
            .await?;
        let candidates: Vec<DiscordMessage> = messages
            .into_iter()
            .filter(|message| is_automated(message) && !is_deploy_qa_post(message))
            .collect();
        Ok(candidates
            .iter()
            .find(|message| links_release(message, tip))
            .or_else(|| {
                candidates
                    .iter()
                    .find(|message| names_release(message, tip))
            })
            .map(|message| message.id.clone()))
    }

    /// Reply under the release announcement, without an @, when every
    /// LIVE-TESTABLE PR passed.
    pub async fn post_verified(
        &self,
        release_message_id: &str,
        report: &str,
        release_url: Option<&str>,
    ) -> Result<String> {
        self.reply(
            release_message_id,
            self.report_embed(TITLE_VERIFIED, COLOR_VERIFIED, report, release_url),
        )
        .await
    }

    /// Reply under the release announcement, without an @, when nothing failed
    /// but a LIVE-TESTABLE check was blocked or the report was incomplete.
    pub async fn post_unverified(
        &self,
        release_message_id: &str,
        report: &str,
        release_url: Option<&str>,
    ) -> Result<String> {
        self.reply(
            release_message_id,
            self.report_embed(TITLE_UNVERIFIED, COLOR_UNVERIFIED, report, release_url),
        )
        .await
    }

    /// Post the FAIL report in the release announcement's thread and `@` the
    /// releaser. Returns the thread id and the posted message id.
    ///
    /// Reuses the tip's stored thread. Otherwise opens a thread from the
    /// announcement, falling back to the thread Discord already attached to it:
    /// a message starts at most one thread, and that thread's id is the
    /// message id.
    pub async fn post_fail(
        &self,
        release_message_id: &str,
        tip: &DeployQaTip,
        report: &str,
    ) -> Result<(String, String)> {
        let title = format!("FAIL {}", tip.tag);
        let thread_id = match tip.discord_thread_id.clone() {
            Some(thread_id) => thread_id,
            None => self.open_thread(release_message_id, &title).await,
        };
        let summary = format!("Deploy QA failed for `{}`.", tip.tag);
        let content = match self.releaser_mention(tip) {
            Some(mention) => format!("{mention} {summary}"),
            None => summary,
        };
        let embed = self.report_embed(&title, COLOR_FAIL, report, tip.html_url.as_deref());
        let sent = self
            .client
            .send_message(&thread_id, CreateMessageParams::with_embed(content, embed))
            .await?;
        Ok((thread_id, sent.id))
    }

    /// Post the outcome as a message of its own in the channel, for a tip with
    /// no release announcement to reply under. FAIL still `@`s the releaser.
    pub async fn post_unannounced(
        &self,
        tip: &DeployQaTip,
        verdict: DeployQaVerdict,
        report: &str,
    ) -> Result<String> {
        let release_url = tip.html_url.as_deref();
        let (headline, embed) = match verdict {
            DeployQaVerdict::AllVerified => (
                format!("Deploy QA for `{}` (`{}`).", tip.tag, tip.repo),
                self.report_embed(TITLE_VERIFIED, COLOR_VERIFIED, report, release_url),
            ),
            DeployQaVerdict::Unverified => (
                format!("Deploy QA for `{}` (`{}`).", tip.tag, tip.repo),
                self.report_embed(TITLE_UNVERIFIED, COLOR_UNVERIFIED, report, release_url),
            ),
            DeployQaVerdict::Fail => {
                let summary = format!("Deploy QA failed for `{}` (`{}`).", tip.tag, tip.repo);
                let headline = match self.releaser_mention(tip) {
                    Some(mention) => format!("{mention} {summary}"),
                    None => summary,
                };
                let title = format!("FAIL {}", tip.tag);
                (
                    headline,
                    self.report_embed(&title, COLOR_FAIL, report, release_url),
                )
            }
        };
        let sent = self
            .client
            .send_message(
                &self.channel_id,
                CreateMessageParams::with_embed(headline, embed),
            )
            .await?;
        Ok(sent.id)
    }

    /// Embed `report`, redacted before it is truncated so a cut can never
    /// leave part of a secret that no longer matches.
    fn report_embed(
        &self,
        title: &str,
        color: u32,
        report: &str,
        release_url: Option<&str>,
    ) -> MessageEmbed {
        let embed = MessageEmbed::new()
            .title(title)
            .description(truncate_report(&self.redactor.redact(report)))
            .color(color)
            .footer(DEPLOY_QA_SOURCE)
            .timestamp(chrono::Utc::now().to_rfc3339());
        match release_url {
            Some(url) => embed.url(url),
            None => embed,
        }
    }

    async fn reply(&self, release_message_id: &str, embed: MessageEmbed) -> Result<String> {
        let params = CreateMessageParams::with_embed("", embed).replying_to(release_message_id);
        let sent = self.client.send_message(&self.channel_id, params).await?;
        Ok(sent.id)
    }

    async fn open_thread(&self, release_message_id: &str, name: &str) -> String {
        match self
            .client
            .create_thread_from_message(
                &self.channel_id,
                release_message_id,
                CreateThreadParams::public(name),
            )
            .await
        {
            Ok(thread) => thread.id,
            Err(error) => {
                tracing::warn!(
                    release_message_id,
                    %error,
                    "deploy_qa: could not open a FAIL thread; posting into the release announcement's existing thread"
                );
                release_message_id.to_string()
            }
        }
    }

    fn releaser_mention(&self, tip: &DeployQaTip) -> Option<String> {
        let Some(login) = tip.author_login.as_deref() else {
            tracing::warn!(
                tag = %tip.tag,
                "deploy_qa: release has no author login; posting FAIL without @releaser"
            );
            return None;
        };
        let mention = self.mention_for_login(login);
        if mention.is_none() {
            tracing::warn!(
                login,
                tag = %tip.tag,
                "deploy_qa: releaser is in neither the GitHub↔Discord map nor [users]; posting FAIL without @releaser"
            );
        }
        mention
    }
}

#[async_trait]
impl<H: DiscordHttpClient> DeployQaAnnouncer for DeployQaDiscord<H> {
    async fn announce(&self, announcement: &DeployQaAnnouncement) -> Result<String> {
        if let Some(message_id) = self.find_announcement(announcement).await? {
            return Ok(message_id);
        }
        let embed = self.announcement_embed(announcement);
        let sent = self
            .client
            .send_message(&self.channel_id, CreateMessageParams::with_embed("", embed))
            .await?;
        Ok(sent.id)
    }
}

/// Classify a deploy-QA report, persist the tip's status, and post the outcome
/// to Discord `#releases`.
///
/// The verdict is read from the report as the agent wrote it; the reporter
/// redacts credentials only from what it posts, so a redacted line can never
/// hide a `LIVE FAIL`.
///
/// A tip with no announcement in `#releases` (a bare tag, which no release
/// feed posts) gets its own post instead of a reply.
///
/// Only loading the tip or persisting its status can fail. Once the status is
/// stored, Discord problems (no reporter, a failed search or post, ids that
/// could not be recorded) are logged instead, so a report Discord already
/// received is never broadcast again.
pub async fn report_deploy_qa_outcome<S, H>(
    store: &S,
    discord: Option<&DeployQaDiscord<H>>,
    issue_id: &str,
    report: &str,
) -> Result<DeployQaVerdict>
where
    S: DeployQaStore + ?Sized,
    H: DiscordHttpClient,
{
    let verdict = classify_deploy_qa_verdict(report);
    let Some(tip) = store.get_deploy_qa_tip_by_issue_id(issue_id)? else {
        return Ok(verdict);
    };
    store.update_deploy_qa_tip_status(tip.id, tip_status(verdict), None)?;

    let Some(discord) = discord else {
        tracing::info!(
            issue_id,
            ?verdict,
            "deploy_qa Discord reporter unset; status persisted without posting"
        );
        return Ok(verdict);
    };

    let release_message_id = match tip.discord_message_id.clone() {
        Some(id) => id,
        None => match discord.find_release_message(&tip).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                tracing::info!(
                    tag = %tip.tag,
                    "deploy_qa: no #releases announcement found; posting the outcome on its own"
                );
                if let Err(error) = discord.post_unannounced(&tip, verdict, report).await {
                    tracing::warn!(
                        tag = %tip.tag,
                        ?verdict,
                        %error,
                        "deploy_qa: failed to post the outcome to #releases"
                    );
                }
                return Ok(verdict);
            }
            Err(error) => {
                tracing::warn!(%error, "deploy_qa: failed to search #releases");
                return Ok(verdict);
            }
        },
    };

    let release_url = tip.html_url.as_deref();
    let posted = match verdict {
        DeployQaVerdict::AllVerified => discord
            .post_verified(&release_message_id, report, release_url)
            .await
            .map(|_| None),
        DeployQaVerdict::Unverified => discord
            .post_unverified(&release_message_id, report, release_url)
            .await
            .map(|_| None),
        DeployQaVerdict::Fail => discord
            .post_fail(&release_message_id, &tip, report)
            .await
            .map(|(thread_id, _)| Some(thread_id)),
    };
    let thread_id = posted.unwrap_or_else(|error| {
        tracing::warn!(
            tag = %tip.tag,
            ?verdict,
            %error,
            "deploy_qa: failed to post the outcome to #releases"
        );
        None
    });
    record_discord_ids(store, &tip, &release_message_id, thread_id.as_deref());

    Ok(verdict)
}

fn tip_status(verdict: DeployQaVerdict) -> DeployQaTipStatus {
    match verdict {
        DeployQaVerdict::AllVerified => DeployQaTipStatus::Verified,
        DeployQaVerdict::Unverified => DeployQaTipStatus::Unverified,
        DeployQaVerdict::Fail => DeployQaTipStatus::Failed,
    }
}

fn record_discord_ids<S: DeployQaStore + ?Sized>(
    store: &S,
    tip: &DeployQaTip,
    release_message_id: &str,
    thread_id: Option<&str>,
) {
    let message_id = (tip.discord_message_id.as_deref() != Some(release_message_id))
        .then_some(release_message_id);
    let thread_id = thread_id.filter(|id| tip.discord_thread_id.as_deref() != Some(*id));
    if message_id.is_none() && thread_id.is_none() {
        return;
    }
    if let Err(error) = store.update_deploy_qa_discord_ids(tip.id, message_id, thread_id) {
        tracing::warn!(
            issue_id = %tip.issue_id,
            %error,
            "deploy_qa: failed to record the #releases message and thread ids"
        );
    }
}

fn is_deploy_qa_post(message: &DiscordMessage) -> bool {
    message.embeds.iter().any(|embed| {
        embed
            .footer
            .as_ref()
            .is_some_and(|footer| footer.text == DEPLOY_QA_SOURCE)
    })
}

/// Whether a webhook (GitHub's release feed) or a bot sent `message`, rather
/// than a person chatting about the release.
fn is_automated(message: &DiscordMessage) -> bool {
    message.webhook_id.is_some() || message.author.as_ref().is_some_and(|author| author.bot)
}

fn links_release(message: &DiscordMessage, tip: &DeployQaTip) -> bool {
    let Some(release_url) = tip.html_url.as_deref() else {
        return false;
    };
    message
        .embeds
        .iter()
        .filter_map(|embed| embed.url.as_deref())
        .any(|url| same_url(url, release_url))
}

fn same_url(left: &str, right: &str) -> bool {
    left.trim_end_matches('/')
        .eq_ignore_ascii_case(right.trim_end_matches('/'))
}

/// Whether the message content or an embed title names the tip's repo and tag.
/// Embed descriptions are skipped: they carry release notes and changelog
/// links that name earlier tags.
fn names_release(message: &DiscordMessage, tip: &DeployQaTip) -> bool {
    let headlines: Vec<&str> = std::iter::once(message.content.as_str())
        .chain(
            message
                .embeds
                .iter()
                .filter_map(|embed| embed.title.as_deref()),
        )
        .collect();
    let repo = tip.repo.to_ascii_lowercase();
    headlines
        .iter()
        .any(|text| contains_token(&text.to_ascii_lowercase(), &repo))
        && headlines.iter().any(|text| contains_token(text, &tip.tag))
}

/// Whether `token` occurs in `text` without running into a neighbouring name
/// or version character, so `1.2.3` does not match inside `1.2.3-db`,
/// `1.2.30`, `v1.2.3`, `1.2.3.4` or the `1.2.3...1.2.4` compare range.
fn contains_token(text: &str, token: &str) -> bool {
    !token.is_empty()
        && text.match_indices(token).any(|(start, _)| {
            bounded_before(&text[..start], token) && bounded_after(&text[start + token.len()..])
        })
}

fn bounded_before(before: &str, token: &str) -> bool {
    match before.chars().next_back() {
        None => true,
        Some('.') => !token.starts_with(char::is_alphanumeric),
        Some(neighbour) => !extends_token(neighbour),
    }
}

fn bounded_after(after: &str) -> bool {
    let mut characters = after.chars();
    match characters.next() {
        None => true,
        Some('.') => !characters
            .next()
            .is_some_and(|next| next == '.' || next.is_alphanumeric()),
        Some(neighbour) => !extends_token(neighbour),
    }
}

fn extends_token(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '-' | '_' | '+')
}

/// Trim the report to [`REPORT_CHARACTER_LIMIT`], keeping the tail: the per-PR
/// results and the verdict footer come last.
fn truncate_report(report: &str) -> String {
    let trimmed = report.trim();
    let length = trimmed.chars().count();
    if length <= REPORT_CHARACTER_LIMIT {
        return trimmed.to_string();
    }
    let tail: String = trimmed
        .chars()
        .skip(length - (REPORT_CHARACTER_LIMIT - 1))
        .collect();
    format!("{ELLIPSIS}{tail}")
}

/// Build a reporter from a bot token + channel, or `None` when unconfigured.
pub fn try_build_discord(
    bot_token: Option<&str>,
    channel_id: Option<&str>,
    map: GitHubDiscordMap,
    redactor: Redactor,
) -> Result<Option<DeployQaDiscord>> {
    let token = bot_token.filter(|token| !token.is_empty());
    let channel = channel_id.filter(|channel| !channel.is_empty());
    match (token, channel) {
        (Some(token), Some(channel)) => {
            Ok(Some(DeployQaDiscord::new(token, channel, map, redactor)?))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abnegate_http::HttpResponse;
    use async_trait::async_trait;
    use claudear_analysis::deploy_qa::VERDICT_ALL_VERIFIED;
    use claudear_analysis::deploy_qa::VERDICT_FAIL;
    use claudear_analysis::deploy_qa::VERDICT_PREFIX;
    use claudear_analysis::deploy_qa::VERDICT_UNVERIFIED;
    use claudear_core::error::Error;
    use serde_json::json;
    use serde_json::Value;
    use std::sync::Arc;
    use std::sync::Mutex;

    const CHANNEL: &str = "990878183580651571";
    const RELEASE_MESSAGE: &str = "1111";
    const CREATED_THREAD: &str = "5555";
    const REPO: &str = "appwrite-labs/cloud";
    const TAG: &str = "1.2.3";
    const RELEASER: &str = "abnegate";
    const RELEASER_DISCORD_ID: &str = "452316113016193024";
    const KNOWN_SECRET: &str = "configured-bot-token-0001";
    const BEARER_TOKEN: &str = "abc123def456ghi789";
    const GITHUB_TOKEN: &str = "ghp_0123456789abcdefXYZ";
    const MASK: &str = "[REDACTED]";
    const DISCORD_EMBED_DESCRIPTION_LIMIT: usize = 4096;
    const CUT_MARKER: char = '…';

    #[derive(Debug, Clone)]
    struct Request {
        method: &'static str,
        url: String,
        body: Value,
    }

    impl Request {
        fn is_post_to(&self, path: &str) -> bool {
            self.method == "POST" && self.url.ends_with(path)
        }
    }

    type Requests = Arc<Mutex<Vec<Request>>>;

    struct FakeDiscord {
        channel_messages: Vec<Value>,
        thread_creation_fails: bool,
        requests: Requests,
    }

    impl FakeDiscord {
        fn record(&self, method: &'static str, url: &str, body: Value) {
            self.requests.lock().unwrap().push(Request {
                method,
                url: url.to_string(),
                body,
            });
        }
    }

    fn respond(status: u16, body: Value) -> Result<HttpResponse> {
        Ok(HttpResponse::new(status, body.to_string()))
    }

    #[async_trait]
    impl DiscordHttpClient for FakeDiscord {
        async fn get(&self, url: &str) -> Result<HttpResponse> {
            self.record("GET", url, Value::Null);
            if url.contains(&format!("/channels/{CHANNEL}/messages?")) {
                return respond(200, Value::Array(self.channel_messages.clone()));
            }
            respond(404, json!({ "message": "Unknown Channel", "code": 10003 }))
        }

        async fn post(&self, url: &str, body: Value) -> Result<HttpResponse> {
            self.record("POST", url, body.clone());
            if url.ends_with("/threads") {
                if self.thread_creation_fails {
                    return respond(
                        400,
                        json!({
                            "message": "A thread has already been created for this message",
                            "code": 160004
                        }),
                    );
                }
                return respond(
                    200,
                    json!({ "id": CREATED_THREAD, "type": 11, "name": body["name"] }),
                );
            }
            let channel = url
                .trim_end_matches("/messages")
                .rsplit('/')
                .next()
                .unwrap_or_default();
            respond(
                200,
                json!({
                    "id": format!("sent-in-{channel}"),
                    "channel_id": channel,
                    "content": body["content"],
                    "timestamp": "2026-09-24T00:00:00Z"
                }),
            )
        }

        async fn patch(&self, url: &str, body: Value) -> Result<HttpResponse> {
            self.record("PATCH", url, body);
            respond(404, Value::Null)
        }

        async fn put_empty(&self, url: &str) -> Result<HttpResponse> {
            self.record("PUT", url, Value::Null);
            respond(404, Value::Null)
        }
    }

    fn reporter(
        channel_messages: Vec<Value>,
        thread_creation_fails: bool,
    ) -> (DeployQaDiscord<FakeDiscord>, Requests) {
        let requests = Requests::default();
        let http = FakeDiscord {
            channel_messages,
            thread_creation_fails,
            requests: requests.clone(),
        };
        let client = DiscordClient::with_http_client("test-token", http).unwrap();
        let map = GitHubDiscordMap::from_json_bytes(
            json!({ "byGithubLogin": { RELEASER: { "discordUserId": RELEASER_DISCORD_ID } } })
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        let redactor = Redactor::new([KNOWN_SECRET]);
        (
            DeployQaDiscord::with_client(client, CHANNEL, map, redactor),
            requests,
        )
    }

    fn posts(requests: &Requests) -> Vec<Request> {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.method == "POST")
            .cloned()
            .collect()
    }

    fn posted_text(requests: &Requests) -> String {
        posts(requests)
            .iter()
            .map(|request| request.body.to_string())
            .collect()
    }

    fn description(request: &Request) -> String {
        request.body["embeds"][0]["description"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn release_url(repo: &str, tag: &str) -> String {
        format!("https://github.com/{repo}/releases/tag/{tag}")
    }

    fn announcement(id: &str, repo: &str, tag: &str) -> Value {
        json!({
            "id": id,
            "channel_id": CHANNEL,
            "content": "",
            "timestamp": "2026-09-23T01:00:00Z",
            "author": { "id": "900", "username": "GitHub", "bot": true },
            "webhook_id": "901",
            "embeds": [{
                "title": format!("[{repo}] New release published: {tag}"),
                "url": release_url(repo, tag)
            }]
        })
    }

    fn human_message(id: &str, content: &str) -> Value {
        json!({
            "id": id,
            "channel_id": CHANNEL,
            "content": content,
            "timestamp": "2026-09-23T02:00:00Z",
            "author": { "id": "700", "username": "releaser", "bot": false }
        })
    }

    fn tip() -> DeployQaTip {
        let mut tip = DeployQaTip::new("cloud", REPO, TAG);
        tip.id = 7;
        tip.html_url = Some(release_url(REPO, TAG));
        tip.author_login = Some(RELEASER.to_string());
        tip
    }

    /// The tip as stored, and without a release URL so only the repo + tag
    /// fallback can find its announcement.
    fn tips_with_and_without_release_url() -> [DeployQaTip; 2] {
        let mut without_url = tip();
        without_url.html_url = None;
        [tip(), without_url]
    }

    fn verdict_report(result: &str, verdict: &str) -> String {
        format!("- #42 new route {result}\n- #43 ci INFRA\n{VERDICT_PREFIX} {verdict}")
    }

    struct MemoryStore {
        tip: Mutex<DeployQaTip>,
        discord_ids_writable: bool,
    }

    impl MemoryStore {
        fn new(tip: DeployQaTip) -> Self {
            Self {
                tip: Mutex::new(tip),
                discord_ids_writable: true,
            }
        }

        fn read_only_discord_ids(tip: DeployQaTip) -> Self {
            Self {
                discord_ids_writable: false,
                ..Self::new(tip)
            }
        }

        fn tip(&self) -> DeployQaTip {
            self.tip.lock().unwrap().clone()
        }
    }

    impl DeployQaStore for MemoryStore {
        fn get_deploy_qa_tip_by_issue_id(&self, issue_id: &str) -> Result<Option<DeployQaTip>> {
            let tip = self.tip();
            Ok((tip.issue_id == issue_id).then_some(tip))
        }

        fn update_deploy_qa_tip_status(
            &self,
            id: i64,
            status: DeployQaTipStatus,
            attempt_id: Option<i64>,
        ) -> Result<()> {
            let mut tip = self.tip.lock().unwrap();
            if tip.id == id {
                tip.status = status;
                tip.attempt_id = attempt_id.or(tip.attempt_id);
            }
            Ok(())
        }

        fn update_deploy_qa_discord_ids(
            &self,
            id: i64,
            message_id: Option<&str>,
            thread_id: Option<&str>,
        ) -> Result<()> {
            if !self.discord_ids_writable {
                return Err(Error::storage("database is locked"));
            }
            let mut tip = self.tip.lock().unwrap();
            if tip.id == id {
                if let Some(message_id) = message_id {
                    tip.discord_message_id = Some(message_id.to_string());
                }
                if let Some(thread_id) = thread_id {
                    tip.discord_thread_id = Some(thread_id.to_string());
                }
            }
            Ok(())
        }
    }

    async fn report(
        store: &MemoryStore,
        discord: &DeployQaDiscord<FakeDiscord>,
        report: &str,
    ) -> DeployQaVerdict {
        let issue_id = store.tip().issue_id;
        report_deploy_qa_outcome(store, Some(discord), &issue_id, report)
            .await
            .expect("reporting should not fail once the status is stored")
    }

    async fn posted_title(text: &str) -> Value {
        let store = MemoryStore::new(tip());
        let (discord, requests) = reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], false);
        report(&store, &discord, text).await;
        posts(&requests)
            .pop()
            .expect("the report should be posted")
            .body["embeds"][0]["title"]
            .clone()
    }

    fn assert_reply_without_mention(request: &Request) {
        assert!(request.is_post_to(&format!("/channels/{CHANNEL}/messages")));
        assert_eq!(
            request.body["message_reference"]["message_id"], RELEASE_MESSAGE,
            "the outcome should reply to the release announcement: {:?}",
            request.body
        );
        let content = request.body["content"].as_str().unwrap_or_default();
        assert!(
            !content.contains("<@"),
            "no one should be pinged: {content}"
        );
    }

    #[tokio::test]
    async fn verified_report_replies_under_release_message_without_mention() {
        let store = MemoryStore::new(tip());
        let (discord, requests) = reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], false);

        let verdict = report(
            &store,
            &discord,
            &verdict_report("LIVE PASS", VERDICT_ALL_VERIFIED),
        )
        .await;

        assert_eq!(verdict, DeployQaVerdict::AllVerified);
        let posts = posts(&requests);
        assert_eq!(posts.len(), 1, "{posts:?}");
        assert_reply_without_mention(&posts[0]);
        let stored = store.tip();
        assert_eq!(stored.status, DeployQaTipStatus::Verified);
        assert_eq!(stored.discord_message_id.as_deref(), Some(RELEASE_MESSAGE));
        assert_eq!(stored.discord_thread_id, None);
    }

    #[tokio::test]
    async fn unverified_report_replies_without_mention_or_thread() {
        let store = MemoryStore::new(tip());
        let (discord, requests) = reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], false);

        let verdict = report(
            &store,
            &discord,
            &verdict_report("LIVE BLOCKED", VERDICT_ALL_VERIFIED),
        )
        .await;

        assert_eq!(verdict, DeployQaVerdict::Unverified);
        let posts = posts(&requests);
        assert_eq!(posts.len(), 1, "exactly one reply, no thread: {posts:?}");
        assert_reply_without_mention(&posts[0]);
        let verified_title = posted_title(&verdict_report("LIVE PASS", VERDICT_ALL_VERIFIED)).await;
        assert_ne!(
            posts[0].body["embeds"][0]["title"], verified_title,
            "a blocked check must not be reported as verified"
        );
        let stored = store.tip();
        assert_eq!(stored.status, DeployQaTipStatus::Unverified);
        assert_eq!(stored.discord_thread_id, None);
    }

    #[tokio::test]
    async fn fail_report_opens_thread_and_mentions_mapped_releaser_ignoring_case() {
        let mut tip = tip();
        tip.author_login = Some("AbNeGaTe".to_string());
        let store = MemoryStore::new(tip);
        let (discord, requests) = reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], false);

        let verdict = report(&store, &discord, &verdict_report("LIVE FAIL", VERDICT_FAIL)).await;

        assert_eq!(verdict, DeployQaVerdict::Fail);
        let posts = posts(&requests);
        assert_eq!(posts.len(), 2, "{posts:?}");
        assert!(posts[0].is_post_to(&format!(
            "/channels/{CHANNEL}/messages/{RELEASE_MESSAGE}/threads"
        )));
        assert!(posts[1].is_post_to(&format!("/channels/{CREATED_THREAD}/messages")));
        let content = posts[1].body["content"].as_str().unwrap_or_default();
        assert!(
            content.contains(&format!("<@{RELEASER_DISCORD_ID}>")),
            "the releaser should be pinged: {content}"
        );
        let stored = store.tip();
        assert_eq!(stored.status, DeployQaTipStatus::Failed);
        assert_eq!(stored.discord_message_id.as_deref(), Some(RELEASE_MESSAGE));
        assert_eq!(stored.discord_thread_id.as_deref(), Some(CREATED_THREAD));
    }

    #[tokio::test]
    async fn fail_posts_into_existing_release_thread_when_thread_creation_fails() {
        let store = MemoryStore::new(tip());
        let (discord, requests) = reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], true);

        let verdict = report(&store, &discord, &verdict_report("LIVE FAIL", VERDICT_FAIL)).await;

        assert_eq!(verdict, DeployQaVerdict::Fail);
        let posts = posts(&requests);
        let report_post = posts.last().expect("the FAIL report should be posted");
        assert!(
            report_post.is_post_to(&format!("/channels/{RELEASE_MESSAGE}/messages")),
            "{posts:?}"
        );
        let content = report_post.body["content"].as_str().unwrap_or_default();
        assert!(content.contains(&format!("<@{RELEASER_DISCORD_ID}>")));
        assert_eq!(
            store.tip().discord_thread_id.as_deref(),
            Some(RELEASE_MESSAGE)
        );
    }

    #[tokio::test]
    async fn fail_reuses_stored_thread_without_creating_another() {
        let mut tip = tip();
        tip.discord_message_id = Some(RELEASE_MESSAGE.to_string());
        tip.discord_thread_id = Some("7777".to_string());
        let store = MemoryStore::new(tip);
        let (discord, requests) = reporter(Vec::new(), false);

        report(&store, &discord, &verdict_report("LIVE FAIL", VERDICT_FAIL)).await;

        let recorded = requests.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "no search and no new thread: {recorded:?}"
        );
        assert!(recorded[0].is_post_to("/channels/7777/messages"));
        assert_eq!(store.tip().discord_thread_id.as_deref(), Some("7777"));
    }

    #[tokio::test]
    async fn fail_without_mapped_releaser_posts_without_mention() {
        for author in [None, Some("someone-else")] {
            let mut tip = tip();
            tip.author_login = author.map(str::to_string);
            let store = MemoryStore::new(tip);
            let (discord, requests) =
                reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], false);

            report(&store, &discord, &verdict_report("LIVE FAIL", VERDICT_FAIL)).await;

            let posts = posts(&requests);
            let content = posts.last().expect("the FAIL report should be posted").body["content"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            assert!(!content.contains("<@"), "{author:?}: {content}");
        }
    }

    const CONSOLE_REPO: &str = "appwrite/appwrite";
    const CONSOLE_TAG: &str = "apps/console/1.2.141";

    /// A console tip: a bare monorepo tag that no release feed announces.
    fn console_tip() -> DeployQaTip {
        let mut tip = DeployQaTip::new("console", CONSOLE_REPO, CONSOLE_TAG);
        tip.id = 9;
        tip.html_url = Some(release_url(CONSOLE_REPO, CONSOLE_TAG));
        tip.author_login = Some(RELEASER.to_string());
        tip
    }

    #[tokio::test]
    async fn unannounced_fail_posts_on_its_own_and_mentions_the_releaser() {
        let store = MemoryStore::new(tip());
        let (discord, requests) = reporter(vec![announcement("2222", REPO, "1.2.2")], false);

        let verdict = report(&store, &discord, &verdict_report("LIVE FAIL", VERDICT_FAIL)).await;

        assert_eq!(verdict, DeployQaVerdict::Fail);
        let posted = posts(&requests);
        assert_eq!(posted.len(), 1, "one standalone post: {posted:?}");
        let post = &posted[0];
        assert!(post.is_post_to(&format!("/channels/{CHANNEL}/messages")));
        assert!(
            post.body["message_reference"].is_null(),
            "the post must not reply to another release's announcement: {:?}",
            post.body
        );
        let content = post.body["content"].as_str().unwrap_or_default();
        assert!(
            content.contains(&format!("<@{RELEASER_DISCORD_ID}>")),
            "{content}"
        );
        assert!(content.contains(TAG), "{content}");
        assert_eq!(post.body["embeds"][0]["title"], format!("FAIL {TAG}"));
        let stored = store.tip();
        assert_eq!(stored.status, DeployQaTipStatus::Failed);
        assert_eq!(
            stored.discord_message_id, None,
            "Claudear's own post is not a release announcement"
        );
    }

    #[tokio::test]
    async fn console_tag_outcomes_post_on_their_own_naming_the_slashed_tag() {
        let cases = [
            (
                verdict_report("LIVE PASS", VERDICT_ALL_VERIFIED),
                TITLE_VERIFIED,
            ),
            (
                verdict_report("LIVE BLOCKED", VERDICT_UNVERIFIED),
                TITLE_UNVERIFIED,
            ),
        ];
        for (text, title) in cases {
            let store = MemoryStore::new(console_tip());
            let (discord, requests) =
                reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], false);

            report(&store, &discord, &text).await;

            let posted = posts(&requests);
            assert_eq!(posted.len(), 1, "{title}: {posted:?}");
            let post = &posted[0];
            assert!(
                post.body["message_reference"].is_null(),
                "{title}: a console tag must not reply under a cloud release: {:?}",
                post.body
            );
            let content = post.body["content"].as_str().unwrap_or_default();
            assert!(content.contains(CONSOLE_TAG), "{title}: {content}");
            assert!(!content.contains("<@"), "{title}: {content}");
            assert_eq!(post.body["embeds"][0]["title"], title);
            assert_eq!(
                post.body["embeds"][0]["url"],
                "https://github.com/appwrite/appwrite/releases/tag/apps/console/1.2.141"
            );
        }
    }

    #[tokio::test]
    async fn console_tag_matches_an_announcement_naming_it() {
        let (discord, _) = reporter(
            vec![
                announcement("3000", CONSOLE_REPO, "apps/console/1.2.1410"),
                announcement("2000", CONSOLE_REPO, "console/1.2.141"),
                announcement(RELEASE_MESSAGE, CONSOLE_REPO, CONSOLE_TAG),
            ],
            false,
        );
        let mut tip = console_tip();
        tip.html_url = None;

        assert_eq!(
            discord.find_release_message(&tip).await.unwrap().as_deref(),
            Some(RELEASE_MESSAGE)
        );
    }

    #[tokio::test]
    async fn discord_id_write_failure_after_posting_is_not_an_error() {
        let store = MemoryStore::read_only_discord_ids(tip());
        let (discord, requests) = reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], false);

        let result = report_deploy_qa_outcome(
            &store,
            Some(&discord),
            &store.tip().issue_id,
            &verdict_report("LIVE PASS", VERDICT_ALL_VERIFIED),
        )
        .await;

        assert!(
            matches!(result, Ok(DeployQaVerdict::AllVerified)),
            "{result:?}"
        );
        assert_eq!(posts(&requests).len(), 1);
        assert_eq!(store.tip().status, DeployQaTipStatus::Verified);
    }

    #[tokio::test]
    async fn fail_report_is_posted_without_credentials() {
        let store = MemoryStore::new(tip());
        let (discord, requests) = reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], false);
        let leaky_report = format!(
            "- #42 login LIVE FAIL: 401 with `Authorization: Bearer {BEARER_TOKEN}`\n\
             - #43 release notes INFRA, cloned with {GITHUB_TOKEN}\n\
             - #44 bot token {KNOWN_SECRET} in the logs INFRA\n\
             {VERDICT_PREFIX} {VERDICT_FAIL}"
        );

        let verdict = report(&store, &discord, &leaky_report).await;

        assert_eq!(verdict, DeployQaVerdict::Fail);
        assert_eq!(store.tip().status, DeployQaTipStatus::Failed);
        let posted = posted_text(&requests);
        for (kind, value) in [
            ("bearer token", BEARER_TOKEN),
            ("GitHub token", GITHUB_TOKEN),
            ("configured bot token", KNOWN_SECRET),
        ] {
            assert!(
                !posted.contains(value),
                "the {kind} reached the posted report"
            );
        }
        let fail_post = posts(&requests)
            .pop()
            .expect("the FAIL report should be posted");
        assert!(fail_post.is_post_to(&format!("/channels/{CREATED_THREAD}/messages")));
        let description = description(&fail_post);
        assert!(
            description.contains(MASK),
            "the FAIL report must show where a credential was masked"
        );
        assert!(
            description.contains("#42 login LIVE FAIL"),
            "the FAIL report must keep its failing result"
        );
    }

    #[tokio::test]
    async fn verified_and_unverified_replies_are_posted_without_credentials() {
        for (result, expected) in [
            ("LIVE PASS", DeployQaVerdict::AllVerified),
            ("LIVE BLOCKED", DeployQaVerdict::Unverified),
        ] {
            let store = MemoryStore::new(tip());
            let (discord, requests) =
                reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], false);
            let leaky_report = format!(
                "- #42 new route {result}, checked with X-Appwrite-Key: {BEARER_TOKEN}\n\
                 - #43 ci INFRA, bot token {KNOWN_SECRET}\n\
                 {VERDICT_PREFIX} {VERDICT_ALL_VERIFIED}"
            );

            let verdict = report(&store, &discord, &leaky_report).await;

            assert_eq!(verdict, expected, "{result}");
            let posts = posts(&requests);
            assert_eq!(
                posts.len(),
                1,
                "{result}: the report must be posted as one reply"
            );
            assert_reply_without_mention(&posts[0]);
            let description = description(&posts[0]);
            for (kind, value) in [
                ("X-Appwrite-Key value", BEARER_TOKEN),
                ("configured bot token", KNOWN_SECRET),
            ] {
                assert!(
                    !description.contains(value),
                    "{result}: the {kind} reached the posted reply"
                );
            }
            assert!(
                description.contains(MASK),
                "{result}: the reply must show where a credential was masked"
            );
            assert!(
                description.contains(&format!("{VERDICT_PREFIX} {VERDICT_ALL_VERIFIED}")),
                "{result}: the reply must keep its verdict line"
            );
        }
    }

    #[tokio::test]
    async fn report_is_redacted_before_it_is_truncated() {
        const FRAGMENT_LENGTH: usize = 8;
        let fragments: Vec<&str> = (0..=KNOWN_SECRET.len() - FRAGMENT_LENGTH)
            .map(|start| &KNOWN_SECRET[start..start + FRAGMENT_LENGTH])
            .collect();
        let check = format!("- #42 check LIVE PASS token={KNOWN_SECRET}\n");
        let checks = check.repeat(1_000);
        let footer = format!("{VERDICT_PREFIX} {VERDICT_ALL_VERIFIED}");
        for shift in 0..check.len() {
            let store = MemoryStore::new(tip());
            let (discord, requests) =
                reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], false);
            let leaky_report = format!("{checks}{}\n{footer}", " ".repeat(shift));

            report(&store, &discord, &leaky_report).await;

            let posted = posted_text(&requests);
            assert!(
                !fragments.iter().any(|fragment| posted.contains(fragment)),
                "shifted {shift}: part of the configured bot token reached the posted report"
            );
            let description = description(&posts(&requests)[0]);
            assert!(
                description.contains(MASK),
                "shifted {shift}: the report must show where the token was masked"
            );
            assert!(
                description.ends_with(&footer),
                "shifted {shift}: the truncated report must keep its verdict line"
            );
        }
    }

    #[tokio::test]
    async fn verdict_is_classified_from_the_report_before_redaction() {
        let store = MemoryStore::new(tip());
        let (discord, requests) = reporter(vec![announcement(RELEASE_MESSAGE, REPO, TAG)], false);
        let leaky_report = format!(
            "- #42 login with Authorization: Bearer {BEARER_TOKEN} LIVE FAIL\n\
             {VERDICT_PREFIX} {VERDICT_UNVERIFIED}"
        );

        let verdict = report(&store, &discord, &leaky_report).await;

        assert_eq!(
            verdict,
            DeployQaVerdict::Fail,
            "redacting the header must not hide its LIVE FAIL from classification"
        );
        let fail_post = posts(&requests)
            .pop()
            .expect("the FAIL report should be posted");
        assert!(
            fail_post.is_post_to(&format!("/channels/{CREATED_THREAD}/messages")),
            "the report must be posted as a FAIL, in the release thread: {fail_post:?}"
        );
        let description = description(&fail_post);
        assert!(
            !description.contains(BEARER_TOKEN),
            "the bearer token reached the posted report"
        );
    }

    #[tokio::test]
    async fn find_release_message_picks_exact_tag_over_newer_lookalikes() {
        let (discord, _) = reporter(
            vec![
                announcement("5000", REPO, "1.2.3-db"),
                announcement("4000", REPO, "1.2.30"),
                announcement("3000", REPO, "1.2.3-rc.1"),
                announcement("2000", "appwrite-labs/cloud-sdk", TAG),
                announcement(RELEASE_MESSAGE, REPO, TAG),
            ],
            false,
        );

        for tip in tips_with_and_without_release_url() {
            let found = discord.find_release_message(&tip).await.unwrap();

            assert_eq!(
                found.as_deref(),
                Some(RELEASE_MESSAGE),
                "{:?}",
                tip.html_url
            );
        }
    }

    #[tokio::test]
    async fn find_release_message_ignores_newer_announcement_comparing_against_the_tag() {
        let mut next_release = announcement("3000", REPO, "1.2.4");
        next_release["embeds"][0]["description"] = json!(format!(
            "## What's Changed\n* Fix routing\n\n**Full Changelog**: https://github.com/{REPO}/compare/{TAG}...1.2.4"
        ));
        let (discord, _) = reporter(
            vec![next_release, announcement(RELEASE_MESSAGE, REPO, TAG)],
            false,
        );

        for tip in tips_with_and_without_release_url() {
            let found = discord.find_release_message(&tip).await.unwrap();

            assert_eq!(
                found.as_deref(),
                Some(RELEASE_MESSAGE),
                "{:?}",
                tip.html_url
            );
        }
    }

    #[tokio::test]
    async fn find_release_message_ignores_newer_human_message_naming_the_release() {
        let (discord, _) = reporter(
            vec![
                human_message("3000", &format!("rolling {REPO} {TAG} now")),
                announcement(RELEASE_MESSAGE, REPO, TAG),
            ],
            false,
        );

        for tip in tips_with_and_without_release_url() {
            let found = discord.find_release_message(&tip).await.unwrap();

            assert_eq!(
                found.as_deref(),
                Some(RELEASE_MESSAGE),
                "{:?}",
                tip.html_url
            );
        }
    }

    #[tokio::test]
    async fn find_release_message_ignores_newer_human_link_to_the_release() {
        let mut shared_link =
            human_message("3000", &format!("rolling {} now", release_url(REPO, TAG)));
        shared_link["embeds"] = json!([{
            "title": format!("Release {TAG} · {REPO}"),
            "url": release_url(REPO, TAG)
        }]);
        let (discord, _) = reporter(
            vec![shared_link, announcement(RELEASE_MESSAGE, REPO, TAG)],
            false,
        );

        let found = discord.find_release_message(&tip()).await.unwrap();

        assert_eq!(found.as_deref(), Some(RELEASE_MESSAGE));
    }

    #[tokio::test]
    async fn find_release_message_prefers_release_url_over_newer_repo_and_tag_match() {
        let mut deploy_notice = announcement("3000", REPO, TAG);
        deploy_notice["embeds"] = json!([{ "title": format!("[{REPO}] Deploying {TAG}") }]);
        let (discord, _) = reporter(
            vec![deploy_notice, announcement(RELEASE_MESSAGE, REPO, TAG)],
            false,
        );

        let found = discord.find_release_message(&tip()).await.unwrap();

        assert_eq!(found.as_deref(), Some(RELEASE_MESSAGE));
    }

    #[tokio::test]
    async fn find_release_message_falls_back_to_automated_posts_naming_repo_and_tag() {
        let bot_post = json!({
            "id": RELEASE_MESSAGE,
            "channel_id": CHANNEL,
            "content": format!("{REPO} {TAG} released"),
            "timestamp": "2026-09-23T01:00:00Z",
            "author": { "id": "600", "username": "Release Bot", "bot": true }
        });
        let webhook_post = json!({
            "id": RELEASE_MESSAGE,
            "channel_id": CHANNEL,
            "content": "",
            "timestamp": "2026-09-23T01:00:00Z",
            "author": { "id": "901", "username": "Releases" },
            "webhook_id": "901",
            "embeds": [{ "title": format!("[{REPO}] New release published: {TAG}") }]
        });
        for automated_post in [bot_post, webhook_post] {
            let (discord, _) = reporter(
                vec![
                    human_message("3000", &format!("rolling {REPO} {TAG} now")),
                    automated_post.clone(),
                ],
                false,
            );

            let found = discord.find_release_message(&tip()).await.unwrap();

            assert_eq!(found.as_deref(), Some(RELEASE_MESSAGE), "{automated_post}");
        }
    }

    #[tokio::test]
    async fn find_release_message_skips_claudears_own_posts() {
        let (seed, seed_requests) = reporter(Vec::new(), false);
        seed.post_verified(
            RELEASE_MESSAGE,
            &format!("{REPO} {TAG}\n{}", release_url(REPO, TAG)),
            Some(&release_url(REPO, TAG)),
        )
        .await
        .unwrap();
        let posted = posts(&seed_requests).remove(0).body;
        let own_post = json!({
            "id": "9999",
            "channel_id": CHANNEL,
            "content": posted["content"],
            "timestamp": "2026-09-24T00:00:00Z",
            "author": { "id": "800", "username": "Claudear", "bot": true },
            "embeds": posted["embeds"]
        });
        let (discord, _) = reporter(
            vec![own_post, announcement(RELEASE_MESSAGE, REPO, TAG)],
            false,
        );

        let found = discord.find_release_message(&tip()).await.unwrap();

        assert_eq!(found.as_deref(), Some(RELEASE_MESSAGE));
    }

    #[tokio::test]
    async fn find_release_message_matches_release_url_alone() {
        let mut tip = tip();
        tip.repo = "appwrite/cloud".to_string();
        let message = json!({
            "id": RELEASE_MESSAGE,
            "channel_id": CHANNEL,
            "content": "",
            "timestamp": "2026-09-23T01:00:00Z",
            "webhook_id": "901",
            "embeds": [{ "title": "Cloud shipped", "url": format!("{}/", release_url(REPO, TAG)) }]
        });
        let (discord, _) = reporter(vec![message], false);

        let found = discord.find_release_message(&tip).await.unwrap();

        assert_eq!(found.as_deref(), Some(RELEASE_MESSAGE));
    }

    #[tokio::test]
    async fn find_release_message_ignores_repo_case() {
        let (discord, _) = reporter(
            vec![announcement(RELEASE_MESSAGE, "Appwrite-Labs/Cloud", TAG)],
            false,
        );
        let mut tip = tip();
        tip.html_url = None;

        let found = discord.find_release_message(&tip).await.unwrap();

        assert_eq!(found.as_deref(), Some(RELEASE_MESSAGE));
    }

    #[test]
    fn tokens_must_not_run_into_neighbouring_version_characters() {
        let cases = [
            ("Released 1.2.3", "1.2.3", true),
            ("Released 1.2.3.", "1.2.3", true),
            ("(1.2.3)", "1.2.3", true),
            ("`1.2.3`", "1.2.3", true),
            ("releases/tag/1.2.3", "1.2.3", true),
            ("Released 1.2.3-db", "1.2.3", false),
            ("Released 1.2.30", "1.2.3", false),
            ("Released 1.2.3-rc.1", "1.2.3", false),
            ("Released 1.2.3+build", "1.2.3", false),
            ("Released 1.2.3_hotfix", "1.2.3", false),
            ("Released 1.2.3.4", "1.2.3", false),
            ("Released v1.2.3", "1.2.3", false),
            ("Released 0.1.2.3", "1.2.3", false),
            ("Released 1.2.3-db then 1.2.3", "1.2.3", true),
            ("compare/1.2.3...1.2.4", "1.2.3", false),
            ("compare/1.2.3..1.2.4", "1.2.3", false),
            ("compare/1.2.2...1.2.3", "1.2.3", false),
            ("[appwrite-labs/cloud]", "appwrite-labs/cloud", true),
            ("appwrite-labs/cloud-sdk", "appwrite-labs/cloud", false),
            ("anything", "", false),
        ];
        for (text, token, expected) in cases {
            assert_eq!(contains_token(text, token), expected, "{token} in {text}");
        }
    }

    #[test]
    fn truncate_keeps_short_reports() {
        assert_eq!(truncate_report("  ok \n"), "ok");
    }

    #[test]
    fn truncate_keeps_the_tail_with_the_verdict_footer() {
        let footer = format!("{VERDICT_PREFIX} {VERDICT_FAIL}");
        let report = format!("{}\n- #42 route LIVE FAIL\n{footer}", "x".repeat(10_000));

        let truncated = truncate_report(&report);

        let length = truncated.chars().count();
        assert!(
            length <= DISCORD_EMBED_DESCRIPTION_LIMIT,
            "{length} characters do not fit in a Discord embed description"
        );
        assert!(
            length < report.chars().count(),
            "a report too long for Discord must be cut"
        );
        assert!(
            truncated.starts_with(CUT_MARKER),
            "a cut report must show that text was dropped"
        );
        assert!(
            truncated.ends_with(&footer),
            "a cut report must keep its tail, where the verdict footer is"
        );
    }

    const ANNOUNCEMENT_MESSAGE: &str = "7777";
    const REGISTRY_LOGIN: &str = "atharvadeosthale";
    const REGISTRY_DISCORD_ID: &str = "822500146658017292";

    fn console_announcement(releaser: Option<&str>) -> DeployQaAnnouncement {
        DeployQaAnnouncement {
            track: "console".into(),
            repo: CONSOLE_REPO.into(),
            tag: CONSOLE_TAG.into(),
            version: "1.2.141".into(),
            tag_url: release_url(CONSOLE_REPO, CONSOLE_TAG),
            previous_tag: Some("apps/console/1.2.140".into()),
            deploy_run: Some(claudear_analysis::deploy_qa::DeployRun {
                name: "Console / Production #312".into(),
                url: "https://github.com/appwrite/appwrite/actions/runs/41684208512".into(),
            }),
            releaser_login: releaser.map(str::to_string),
        }
    }

    fn registry() -> UserRegistry {
        let user = claudear_config::config::UserConfig {
            github_usernames: vec![REGISTRY_LOGIN.to_string()],
            discord_id: Some(REGISTRY_DISCORD_ID.to_string()),
            ..Default::default()
        };
        UserRegistry::new(std::collections::HashMap::from([(
            "atharva".to_string(),
            user,
        )]))
    }

    fn field<'a>(request: &'a Request, name: &str) -> &'a str {
        request.body["embeds"][0]["fields"]
            .as_array()
            .and_then(|fields| fields.iter().find(|field| field["name"] == name))
            .and_then(|field| field["value"].as_str())
            .unwrap_or_default()
    }

    /// Claudear's own announcement, as Discord lists it back.
    fn posted_announcement(id: &str, tag_url: &str) -> Value {
        json!({
            "id": id,
            "channel_id": CHANNEL,
            "content": "",
            "timestamp": "2026-11-03T17:30:00Z",
            "author": { "id": "800", "username": "Claudear", "bot": true },
            "embeds": [{
                "title": format!("[{CONSOLE_REPO}] New console release: {CONSOLE_TAG}"),
                "url": tag_url,
                "footer": { "text": ANNOUNCEMENT_FOOTER }
            }]
        })
    }

    #[tokio::test]
    async fn announcement_names_the_tag_version_changes_deploy_and_releaser() {
        let (discord, requests) = reporter(Vec::new(), false);

        let id = discord
            .announce(&console_announcement(Some(RELEASER)))
            .await
            .unwrap();

        let posted = posts(&requests);
        assert_eq!(posted.len(), 1, "{posted:?}");
        let post = &posted[0];
        assert_eq!(id, format!("sent-in-{CHANNEL}"));
        assert!(post.is_post_to(&format!("/channels/{CHANNEL}/messages")));
        assert!(post.body["message_reference"].is_null());
        let embed = &post.body["embeds"][0];
        assert_eq!(
            embed["title"],
            "[appwrite/appwrite] New console release: apps/console/1.2.141"
        );
        assert_eq!(embed["url"], release_url(CONSOLE_REPO, CONSOLE_TAG));
        assert_eq!(embed["footer"]["text"], ANNOUNCEMENT_FOOTER);
        assert_eq!(field(post, "Version"), "`1.2.141`");
        assert_eq!(
            field(post, "Released by"),
            format!("<@{RELEASER_DISCORD_ID}> (`{RELEASER}`)")
        );
        assert_eq!(
            field(post, "Changes"),
            "[apps/console/1.2.140...apps/console/1.2.141](https://github.com/appwrite/appwrite/compare/apps/console/1.2.140...apps/console/1.2.141)"
        );
        assert_eq!(
            field(post, "Deploy"),
            "[Console / Production #312](https://github.com/appwrite/appwrite/actions/runs/41684208512)"
        );
        assert_eq!(
            post.body["content"], "",
            "an announcement shows the releaser without pinging them"
        );
    }

    #[tokio::test]
    async fn announcement_resolves_releasers_through_users_then_falls_back_to_the_login() {
        let (discord, requests) = reporter(Vec::new(), false);
        let discord = discord.with_users(registry());

        discord
            .announce(&console_announcement(Some(REGISTRY_LOGIN)))
            .await
            .unwrap();
        discord
            .announce(&console_announcement(Some("unmapped-person")))
            .await
            .unwrap();
        discord.announce(&console_announcement(None)).await.unwrap();

        let posted = posts(&requests);
        assert_eq!(
            field(&posted[0], "Released by"),
            format!("<@{REGISTRY_DISCORD_ID}> (`{REGISTRY_LOGIN}`)")
        );
        assert_eq!(field(&posted[1], "Released by"), "`unmapped-person`");
        assert_eq!(field(&posted[2], "Released by"), "unknown");
    }

    #[tokio::test]
    async fn announcement_already_in_the_channel_is_reused_not_reposted() {
        let tag_url = release_url(CONSOLE_REPO, CONSOLE_TAG);
        let (discord, requests) = reporter(
            vec![
                posted_announcement("6000", &release_url(CONSOLE_REPO, "apps/console/1.2.140")),
                posted_announcement(ANNOUNCEMENT_MESSAGE, &tag_url),
            ],
            false,
        );

        let id = discord
            .announce(&console_announcement(Some(RELEASER)))
            .await
            .unwrap();

        assert_eq!(id, ANNOUNCEMENT_MESSAGE);
        assert!(posts(&requests).is_empty(), "{:?}", posts(&requests));
    }

    fn announced_console_tip() -> DeployQaTip {
        let mut tip = console_tip();
        tip.discord_message_id = Some(ANNOUNCEMENT_MESSAGE.to_string());
        tip
    }

    #[tokio::test]
    async fn console_outcome_replies_under_its_stored_announcement() {
        let store = MemoryStore::new(announced_console_tip());
        let (discord, requests) = reporter(Vec::new(), false);

        report(
            &store,
            &discord,
            &verdict_report("LIVE PASS", VERDICT_ALL_VERIFIED),
        )
        .await;

        let posted = posts(&requests);
        assert_eq!(posted.len(), 1, "{posted:?}");
        assert_eq!(
            posted[0].body["message_reference"]["message_id"], ANNOUNCEMENT_MESSAGE,
            "the outcome replies under Claudear's announcement like a release's"
        );
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| request.method != "GET"),
            "a stored announcement needs no channel search"
        );
    }

    #[tokio::test]
    async fn console_fail_threads_under_the_announcement_and_mentions_the_releaser() {
        let mut tip = announced_console_tip();
        tip.author_login = Some(REGISTRY_LOGIN.to_string());
        let store = MemoryStore::new(tip);
        let (discord, requests) = reporter(Vec::new(), false);
        let discord = discord.with_users(registry());

        report(&store, &discord, &verdict_report("LIVE FAIL", VERDICT_FAIL)).await;

        let posted = posts(&requests);
        assert_eq!(posted.len(), 2, "{posted:?}");
        assert!(posted[0].is_post_to(&format!(
            "/channels/{CHANNEL}/messages/{ANNOUNCEMENT_MESSAGE}/threads"
        )));
        let content = posted[1].body["content"].as_str().unwrap_or_default();
        assert!(
            content.contains(&format!("<@{REGISTRY_DISCORD_ID}>")),
            "a releaser known only to [users] is still pinged: {content}"
        );
        assert_eq!(
            store.tip().discord_thread_id.as_deref(),
            Some(CREATED_THREAD)
        );
    }

    #[tokio::test]
    async fn outcome_finds_the_announcement_when_its_id_was_never_stored() {
        let tag_url = release_url(CONSOLE_REPO, CONSOLE_TAG);
        let store = MemoryStore::new(console_tip());
        let (discord, requests) = reporter(
            vec![posted_announcement(ANNOUNCEMENT_MESSAGE, &tag_url)],
            false,
        );

        report(
            &store,
            &discord,
            &verdict_report("LIVE PASS", VERDICT_ALL_VERIFIED),
        )
        .await;

        let posted = posts(&requests);
        assert_eq!(
            posted[0].body["message_reference"]["message_id"],
            ANNOUNCEMENT_MESSAGE
        );
        assert_eq!(
            store.tip().discord_message_id.as_deref(),
            Some(ANNOUNCEMENT_MESSAGE)
        );
    }
}
