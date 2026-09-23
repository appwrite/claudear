//! Discord posting hooks for `[deploy_qa]` outcomes.
//!
//! All-verified: reply under the automated `#releases` message (no @).
//! FAIL: create a thread from that message and `@` the mapped releaser.

use claudear_analysis::deploy_qa::{classify_deploy_qa_verdict, DeployQaVerdict, GitHubDiscordMap};
use claudear_core::error::{Error, Result};
use claudear_core::types::DeployQaTipStatus;
use claudear_storage::FixAttemptTracker;

use crate::discord::{
    CreateMessageParams, CreateThreadParams, DiscordClient, DiscordHttpClient, MessageEmbed,
    ReqwestDiscordClient,
};

const COLOR_VERIFIED: u32 = 0x2ecc71;
const COLOR_FAIL: u32 = 0xe74c3c;

/// Discord reporter for deploy-QA results.
pub struct DeployQaDiscord<H: DiscordHttpClient = ReqwestDiscordClient> {
    client: DiscordClient<H>,
    channel_id: String,
    map: GitHubDiscordMap,
}

impl DeployQaDiscord<ReqwestDiscordClient> {
    /// Create a reporter with the default Discord HTTP client.
    pub fn new(
        bot_token: impl Into<String>,
        channel_id: impl Into<String>,
        map: GitHubDiscordMap,
    ) -> Result<Self> {
        Ok(Self {
            client: DiscordClient::new(bot_token)?,
            channel_id: channel_id.into(),
            map,
        })
    }
}

impl<H: DiscordHttpClient> DeployQaDiscord<H> {
    /// Create a reporter with a custom Discord HTTP client (tests).
    pub fn with_client(
        client: DiscordClient<H>,
        channel_id: impl Into<String>,
        map: GitHubDiscordMap,
    ) -> Self {
        Self {
            client,
            channel_id: channel_id.into(),
            map,
        }
    }

    /// Find the automated `#releases` message for this tip (tag / repo).
    pub async fn find_release_message(&self, tag: &str, repo: &str) -> Result<Option<String>> {
        let messages = self
            .client
            .list_channel_messages(&self.channel_id, 50)
            .await?;
        for msg in messages {
            if message_mentions_tip(&msg.content, tag, repo) {
                return Ok(Some(msg.id));
            }
            for embed in &msg.embeds {
                let title = embed.title.as_deref().unwrap_or("");
                let desc = embed.description.as_deref().unwrap_or("");
                if message_mentions_tip(title, tag, repo) || message_mentions_tip(desc, tag, repo) {
                    return Ok(Some(msg.id));
                }
            }
        }
        Ok(None)
    }

    /// Post the all-verified reply (no @) under the release message.
    pub async fn post_verified(
        &self,
        release_message_id: &str,
        report: &str,
        pin: Option<&str>,
    ) -> Result<String> {
        let embed = MessageEmbed::new()
            .title("All PRs verified")
            .description(truncate_report(report, 1800))
            .color(COLOR_VERIFIED)
            .footer(pin.unwrap_or("deploy_qa"))
            .timestamp(chrono::Utc::now().to_rfc3339());
        let params = CreateMessageParams::with_embed("", embed).replying_to(release_message_id);
        let sent = self.client.send_message(&self.channel_id, params).await?;
        Ok(sent.id)
    }

    /// Create a FAIL thread under the release message and @ the releaser.
    pub async fn post_fail(
        &self,
        release_message_id: &str,
        tag: &str,
        report: &str,
        github_login: Option<&str>,
    ) -> Result<(String, String)> {
        let thread = self
            .client
            .create_thread_from_message(
                &self.channel_id,
                release_message_id,
                CreateThreadParams::public(format!("FAIL {tag}")),
            )
            .await?;

        let mention = github_login
            .and_then(|login| self.map.mention_for(login))
            .unwrap_or_default();
        let content = if mention.is_empty() {
            format!("Deploy QA failed for `{tag}`.")
        } else {
            format!("{mention} Deploy QA failed for `{tag}`.")
        };
        let embed = MessageEmbed::new()
            .title(format!("FAIL {tag}"))
            .description(truncate_report(report, 1800))
            .color(COLOR_FAIL)
            .footer("deploy_qa")
            .timestamp(chrono::Utc::now().to_rfc3339());
        let sent = self
            .client
            .send_message(&thread.id, CreateMessageParams::with_embed(content, embed))
            .await?;
        Ok((thread.id, sent.id))
    }
}

/// Post a deploy-QA report to Discord when a bot client is configured.
///
/// When Discord is unavailable the tip status is still updated so the poller
/// does not re-fire. Missing thread APIs are not an issue — DiscordClient
/// already implements `create_thread_from_message`.
pub async fn report_deploy_qa_outcome(
    store: &dyn FixAttemptTracker,
    discord: Option<&DeployQaDiscord>,
    issue_id: &str,
    report: &str,
) -> Result<DeployQaVerdict> {
    let verdict = classify_deploy_qa_verdict(report);
    let Some(tip) = store.get_deploy_qa_tip_by_issue_id(issue_id)? else {
        return Ok(verdict);
    };

    let status = match verdict {
        DeployQaVerdict::AllVerified => DeployQaTipStatus::Verified,
        DeployQaVerdict::Fail => DeployQaTipStatus::Failed,
    };
    store.update_deploy_qa_tip_status(tip.id, status, None)?;

    let Some(discord) = discord else {
        tracing::info!(
            issue_id,
            ?verdict,
            "deploy_qa Discord reporter unset; status persisted without posting"
        );
        return Ok(verdict);
    };

    let release_message_id = match tip.discord_message_id.as_deref() {
        Some(id) => id.to_string(),
        None => match discord.find_release_message(&tip.tag, &tip.repo).await {
            Ok(Some(id)) => {
                store.update_deploy_qa_discord_ids(tip.id, Some(&id), None)?;
                id
            }
            Ok(None) => {
                tracing::warn!(
                    tag = %tip.tag,
                    "deploy_qa: no #releases message found; skipping Discord post"
                );
                return Ok(verdict);
            }
            Err(e) => {
                tracing::warn!(error = %e, "deploy_qa: failed to search #releases");
                return Ok(verdict);
            }
        },
    };

    match verdict {
        DeployQaVerdict::AllVerified => {
            if let Err(e) = discord
                .post_verified(&release_message_id, report, tip.html_url.as_deref())
                .await
            {
                tracing::warn!(error = %e, "deploy_qa: failed to post verified reply");
            }
        }
        DeployQaVerdict::Fail => {
            match discord
                .post_fail(
                    &release_message_id,
                    &tip.tag,
                    report,
                    tip.author_login.as_deref(),
                )
                .await
            {
                Ok((thread_id, _)) => {
                    store.update_deploy_qa_discord_ids(
                        tip.id,
                        Some(&release_message_id),
                        Some(&thread_id),
                    )?;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "deploy_qa: failed to post FAIL thread");
                }
            }
        }
    }

    Ok(verdict)
}

fn message_mentions_tip(text: &str, tag: &str, repo: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    text.contains(tag)
        && (text.contains(repo) || text.contains(repo.split('/').next_back().unwrap_or(repo)))
}

fn truncate_report(report: &str, max: usize) -> String {
    let trimmed = report.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out = trimmed
        .chars()
        .take(max.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

/// Build a reporter from a bot token + channel, or `None` when unconfigured.
pub fn try_build_discord(
    bot_token: Option<&str>,
    channel_id: Option<&str>,
    map: GitHubDiscordMap,
) -> Result<Option<DeployQaDiscord>> {
    let token = bot_token.filter(|t| !t.is_empty());
    let channel = channel_id.filter(|c| !c.is_empty());
    match (token, channel) {
        (Some(token), Some(channel)) => Ok(Some(DeployQaDiscord::new(token, channel, map)?)),
        _ => Ok(None),
    }
}

/// Error helper so callers can surface a missing Discord path without panicking.
pub fn discord_unconfigured() -> Error {
    Error::config("deploy_qa Discord bot token / channel_id not configured")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tip_match_requires_tag() {
        assert!(message_mentions_tip(
            "Released appwrite-labs/cloud 1.2.3",
            "1.2.3",
            "appwrite-labs/cloud"
        ));
        assert!(!message_mentions_tip(
            "Released appwrite-labs/cloud 1.2.2",
            "1.2.3",
            "appwrite-labs/cloud"
        ));
    }

    #[test]
    fn truncate_keeps_short_reports() {
        assert_eq!(truncate_report("ok", 10), "ok");
        assert!(truncate_report(&"x".repeat(50), 10).ends_with("..."));
    }
}
