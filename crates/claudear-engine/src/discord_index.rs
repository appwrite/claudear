use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use claudear_analysis::knowledgebase::{DiscordIndexer, DiscordMessageInput};
use claudear_config::DiscordKnowledgebaseConfig;
use claudear_core::error::Result;
use claudear_core::types::{DiscordChannelKind, DiscordIndexStats};
use claudear_integrations::discord::{
    DiscordChannel, DiscordClient, DiscordMessage, DiscordThread,
};
use claudear_storage::FixAttemptTracker;

/// Discord message page size (the API caps a single page at 100).
const PAGE_LIMIT: usize = 100;

/// A channel or thread registered for message backfill (pass 2 of `run`).
struct IndexTarget {
    id: String,
    name: String,
    is_thread: bool,
}

/// Fetches Discord content and drives the analysis `DiscordIndexer`.
pub struct DiscordIndexOrchestrator {
    client: DiscordClient,
    guild_id: String,
    indexer: Arc<DiscordIndexer>,
    tracker: Arc<dyn FixAttemptTracker>,
}

impl DiscordIndexOrchestrator {
    pub fn new(
        bot_token: &str,
        guild_id: impl Into<String>,
        indexer: Arc<DiscordIndexer>,
        tracker: Arc<dyn FixAttemptTracker>,
    ) -> Result<Self> {
        Ok(Self {
            client: DiscordClient::new(bot_token)?,
            guild_id: guild_id.into(),
            indexer,
            tracker,
        })
    }

    pub async fn run(&self, cfg: &DiscordKnowledgebaseConfig) -> Result<DiscordIndexStats> {
        let mut stats = DiscordIndexStats::default();

        // `categories` is required — an empty list means "index nothing".
        if cfg.categories.is_empty() {
            tracing::info!("Discord knowledgebase has no categories configured — skipping");
            return Ok(stats);
        }
        self.indexer.prepare().await?;

        let channels = self.client.list_guild_channels(&self.guild_id).await?;

        // `categories` are configured by name (or raw id); resolve them to the
        // guild's category channels so we can match by parent id below.
        let (resolved_categories, unresolved) = resolve_categories(&channels, &cfg.categories);
        if !unresolved.is_empty() {
            tracing::warn!(
                guild = %self.guild_id,
                categories = ?unresolved,
                "Configured Discord categories not found in guild — skipping",
            );
        }

        // Persist the resolved categories so the name -> id mapping is stored
        // for future reference (dashboard, debugging, config validation).
        for cat in &resolved_categories {
            let _ = self.tracker.upsert_discord_channel(
                &cat.id,
                Some(self.guild_id.as_str()),
                cat.parent_id.as_deref(),
                cat.name.as_deref(),
                Some(cat.channel_type as i64),
                cat.kind(),
                false,
            );
        }

        let categories: HashSet<&str> = resolved_categories.iter().map(|c| c.id.as_str()).collect();
        let ignore: HashSet<&str> = cfg.ignore_channels.iter().map(String::as_str).collect();

        let selected = select_channels(&channels, &categories, &ignore);

        let active_threads = self
            .client
            .list_active_threads(&self.guild_id, None, None)
            .await
            .unwrap_or_default();

        // register every in-category channel
        let mut targets: Vec<IndexTarget> = Vec::new();
        for channel in &selected {
            let _ = self.tracker.upsert_discord_channel(
                &channel.id,
                Some(self.guild_id.as_str()),
                channel.parent_id.as_deref(),
                channel.name.as_deref(),
                Some(channel.channel_type as i64),
                channel.kind(),
                false,
            );
            targets.push(IndexTarget {
                id: channel.id.clone(),
                name: channel.name.clone().unwrap_or_default(),
                is_thread: false,
            });
        }

        // register each channel's threads
        for channel in &selected {
            let mut threads: Vec<&DiscordThread> = active_threads
                .iter()
                .filter(|t| t.parent_id.as_deref() == Some(channel.id.as_str()))
                .collect();
            let archived = self
                .client
                .list_public_archived_threads(&channel.id, None, Some(PAGE_LIMIT))
                .await
                .unwrap_or_default();
            threads.extend(archived.iter());

            for thread in threads {
                if ignore.contains(thread.id.as_str()) {
                    continue;
                }
                let _ = self.tracker.upsert_discord_channel(
                    &thread.id,
                    thread.guild_id.as_deref().or(Some(self.guild_id.as_str())),
                    thread.parent_id.as_deref(),
                    Some(thread.name.as_str()),
                    Some(thread.thread_type as i64),
                    thread.kind(),
                    thread.archived,
                );
                targets.push(IndexTarget {
                    id: thread.id.clone(),
                    name: thread.name.clone(),
                    is_thread: true,
                });
            }
        }

        tracing::info!(
            guild = %self.guild_id,
            categories = resolved_categories.len(),
            channels = selected.len(),
            targets = targets.len(),
            "Registered Discord channels/threads; starting backfill"
        );

        // backfill messages for every registered target
        for target in &targets {
            match self
                .index_one(&target.id, &target.name, target.is_thread, cfg)
                .await
            {
                Ok(s) => {
                    merge_message_stats(&mut stats, &s);
                    if target.is_thread {
                        stats.threads_processed += 1;
                    } else {
                        stats.channels_processed += 1;
                    }
                }
                Err(e) => {
                    if target.is_thread {
                        tracing::warn!(thread = %target.id, error = %e, "Failed to index thread");
                        stats.threads_failed += 1;
                    } else {
                        tracing::warn!(channel = %target.id, error = %e, "Failed to index channel");
                        stats.channels_failed += 1;
                    }
                }
            }
        }

        tracing::info!(guild = %self.guild_id, %stats, "Discord knowledgebase indexing complete");
        Ok(stats)
    }

    /// Index one channel/thread. Dispatches on persisted backfill state: until
    /// the full history (within `backfill_days`) is scraped, each run resumes the
    /// **backfill**; once `backfill_complete` is set, runs are **incremental**.
    async fn index_one(
        &self,
        channel_id: &str,
        channel_name: &str,
        is_thread: bool,
        cfg: &DiscordKnowledgebaseConfig,
    ) -> Result<DiscordIndexStats> {
        let (complete, backfill_cursor) = self.tracker.get_discord_channel_backfill(channel_id)?;
        if complete {
            self.index_incremental(channel_id, channel_name, is_thread)
                .await
        } else {
            self.index_backfill(channel_id, channel_name, is_thread, backfill_cursor, cfg)
                .await
        }
    }

    /// Incremental: page forward from the stored cursor, indexing each page and
    /// advancing the cursor as we go (so a crash resumes mid-stream).
    async fn index_incremental(
        &self,
        channel_id: &str,
        channel_name: &str,
        is_thread: bool,
    ) -> Result<DiscordIndexStats> {
        let mut after = match self.tracker.get_discord_channel_cursor(channel_id)? {
            Some(a) => a,
            // Backfill marked complete but no forward cursor — nothing to do.
            None => return Ok(DiscordIndexStats::default()),
        };
        let mut stats = DiscordIndexStats::default();
        loop {
            let batch = self
                .client
                .list_channel_messages_after(channel_id, &after, PAGE_LIMIT)
                .await?;
            if batch.is_empty() {
                break;
            }
            let full_page = batch.len() == PAGE_LIMIT;
            let newest = batch.last().unwrap().id.clone();

            let s = self
                .index_batch(channel_id, channel_name, is_thread, &batch)
                .await?;
            merge_message_stats(&mut stats, &s);

            let now = Utc::now().to_rfc3339();
            let _ = self
                .tracker
                .set_discord_channel_cursor(channel_id, &newest, &now);
            after = newest;

            if !full_page {
                break;
            }
        }
        Ok(stats)
    }

    async fn index_backfill(
        &self,
        channel_id: &str,
        channel_name: &str,
        is_thread: bool,
        mut backfill_cursor: Option<String>,
        cfg: &DiscordKnowledgebaseConfig,
    ) -> Result<DiscordIndexStats> {
        let cutoff: Option<DateTime<Utc>> = match cfg.backfill_days {
            Some(days) if days > 0 => Some(Utc::now() - Duration::days(days as i64)),
            _ => None,
        };
        let mut stats = DiscordIndexStats::default();

        loop {
            // One page older than the progress cursor, or the newest page to start.
            let mut batch = match &backfill_cursor {
                None => {
                    let mut b = self
                        .client
                        .list_channel_messages(channel_id, PAGE_LIMIT)
                        .await?;
                    b.reverse(); // newest-first -> chronological
                    b
                }
                Some(before) => {
                    self.client
                        .list_channel_messages_before(channel_id, before, PAGE_LIMIT)
                        .await?
                }
            };

            let now = Utc::now().to_rfc3339();
            if batch.is_empty() {
                let _ = self.tracker.set_discord_channel_backfill(
                    channel_id,
                    true,
                    backfill_cursor.as_deref(),
                    &now,
                );
                break;
            }

            let full_page = batch.len() == PAGE_LIMIT;
            let oldest_id = batch.first().unwrap().id.clone();

            // First page is the newest: seed the forward cursor for later incremental.
            if backfill_cursor.is_none() {
                if let Some(newest) = batch.last() {
                    let _ = self
                        .tracker
                        .set_discord_channel_cursor(channel_id, &newest.id, &now);
                }
            }

            // Drop anything older than the cutoff; any drop means we've reached the bound.
            let reached_cutoff = if let Some(cutoff) = cutoff {
                let before_len = batch.len();
                batch.retain(|m| parse_ts(&m.timestamp).map(|t| t >= cutoff).unwrap_or(true));
                batch.len() < before_len
            } else {
                false
            };

            let s = self
                .index_batch(channel_id, channel_name, is_thread, &batch)
                .await?;
            merge_message_stats(&mut stats, &s);

            backfill_cursor = Some(oldest_id);
            let done = reached_cutoff || !full_page;
            let _ = self.tracker.set_discord_channel_backfill(
                channel_id,
                done,
                backfill_cursor.as_deref(),
                &now,
            );
            if done {
                break;
            }
        }

        Ok(stats)
    }

    /// Filter bot/empty messages, map to the analysis input type, and index a
    /// single batch. No-op (default stats) when nothing survives the filter.
    async fn index_batch(
        &self,
        channel_id: &str,
        channel_name: &str,
        is_thread: bool,
        messages: &[DiscordMessage],
    ) -> Result<DiscordIndexStats> {
        let inputs: Vec<DiscordMessageInput> = messages
            .iter()
            .filter(|m| !is_bot_message(m))
            .filter(|m| !m.content.trim().is_empty())
            .map(|m| map_message(m, channel_id, channel_name, &self.guild_id, is_thread))
            .collect();
        if inputs.is_empty() {
            return Ok(DiscordIndexStats::default());
        }
        self.indexer.index(channel_id, is_thread, inputs).await
    }
}

/// Resolve the configured `categories` — each entry a category **name**
/// (case-insensitive) or a raw category ID — to the matching category channels
/// discovered in the guild. Deduplicated by id. The second element lists the
/// config entries that matched no category so the caller can warn about them.
fn resolve_categories<'a>(
    channels: &'a [DiscordChannel],
    configured: &[String],
) -> (Vec<&'a DiscordChannel>, Vec<String>) {
    let categories: Vec<&DiscordChannel> = channels
        .iter()
        .filter(|c| c.kind() == DiscordChannelKind::Category)
        .collect();

    let mut resolved: Vec<&DiscordChannel> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut unresolved: Vec<String> = Vec::new();

    for entry in configured {
        let needle = entry.trim();
        if needle.is_empty() {
            continue;
        }
        match categories.iter().find(|c| {
            c.id == needle
                || c.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(needle))
        }) {
            // Newly matched category — record it.
            Some(cat) if seen.insert(cat.id.as_str()) => resolved.push(cat),
            // Already matched via another entry (name + id, or duplicate).
            Some(_) => {}
            None => unresolved.push(entry.clone()),
        }
    }

    (resolved, unresolved)
}

/// Select the text channels whose parent category id is in `categories`, minus
/// any in `ignore`. Categories and threads (non-`Channel` kinds) are excluded —
/// threads are discovered separately per channel.
fn select_channels<'a>(
    channels: &'a [DiscordChannel],
    categories: &HashSet<&str>,
    ignore: &HashSet<&str>,
) -> Vec<&'a DiscordChannel> {
    channels
        .iter()
        .filter(|c| c.kind() == DiscordChannelKind::Channel)
        .filter(|c| {
            c.parent_id
                .as_deref()
                .map(|p| categories.contains(p))
                .unwrap_or(false)
        })
        .filter(|c| !ignore.contains(c.id.as_str()))
        .collect()
}

/// Map an integration `DiscordMessage` into the analysis-owned input type.
fn map_message(
    msg: &DiscordMessage,
    channel_id: &str,
    channel_name: &str,
    guild_id: &str,
    is_thread: bool,
) -> DiscordMessageInput {
    DiscordMessageInput {
        message_id: msg.id.clone(),
        channel_id: channel_id.to_string(),
        guild_id: guild_id.to_string(),
        channel_name: channel_name.to_string(),
        is_thread,
        author: msg
            .author
            .as_ref()
            .map(|a| a.username.clone())
            .unwrap_or_else(|| "unknown".to_string()),
        content: msg.content.clone(),
        timestamp: msg.timestamp.clone(),
        reply_to: msg
            .message_reference
            .as_ref()
            .and_then(|r| r.message_id.clone()),
    }
}

/// Whether a message should be skipped as bot noise. Webhook messages are
/// treated as human content (mirrors `source/discord.rs::is_bot_message`).
fn is_bot_message(msg: &DiscordMessage) -> bool {
    if msg.webhook_id.is_some() {
        return false;
    }
    msg.author.as_ref().is_some_and(|a| a.bot)
}

fn parse_ts(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Accumulate per-message stats from one channel/thread into the running total.
/// Channel/thread counts are tracked by the caller.
fn merge_message_stats(into: &mut DiscordIndexStats, from: &DiscordIndexStats) {
    into.messages_processed += from.messages_processed;
    into.messages_skipped += from.messages_skipped;
    into.messages_failed += from.messages_failed;
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_integrations::discord::{DiscordMessage, DiscordUser};

    fn channel(id: &str, channel_type: u8, parent: Option<&str>) -> DiscordChannel {
        DiscordChannel {
            id: id.to_string(),
            channel_type,
            guild_id: Some("guild1".to_string()),
            name: Some(format!("name-{id}")),
            parent_id: parent.map(String::from),
        }
    }

    fn user(id: &str, name: &str, bot: bool) -> DiscordUser {
        DiscordUser {
            id: id.to_string(),
            username: name.to_string(),
            discriminator: String::new(),
            avatar: None,
            bot,
        }
    }

    fn message(id: &str, author: Option<DiscordUser>, content: &str) -> DiscordMessage {
        DiscordMessage {
            id: id.to_string(),
            channel_id: "c1".to_string(),
            author,
            content: content.to_string(),
            timestamp: "2024-01-01T10:00:00Z".to_string(),
            message_reference: None,
            thread: None,
            webhook_id: None,
            embeds: vec![],
            mentions: vec![],
        }
    }

    #[test]
    fn test_select_channels_by_category_minus_ignored() {
        let channels = vec![
            channel("cat1", 4, None),        // a category itself — excluded
            channel("c1", 0, Some("cat1")),  // text under cat1 — kept
            channel("c2", 0, Some("cat2")),  // text under another category — excluded
            channel("c3", 0, Some("cat1")),  // text under cat1 but ignored
            channel("t1", 11, Some("cat1")), // a thread-type — excluded (not Channel)
            channel("c4", 0, None),          // no parent — excluded
        ];
        let categories: HashSet<&str> = ["cat1"].into_iter().collect();
        let ignore: HashSet<&str> = ["c3"].into_iter().collect();

        let selected = select_channels(&channels, &categories, &ignore);
        let ids: Vec<&str> = selected.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["c1"]);
    }

    #[test]
    fn test_select_channels_empty_categories_selects_nothing() {
        let channels = vec![channel("c1", 0, Some("cat1"))];
        let categories: HashSet<&str> = HashSet::new();
        let ignore: HashSet<&str> = HashSet::new();
        assert!(select_channels(&channels, &categories, &ignore).is_empty());
    }

    #[test]
    fn test_resolve_categories_by_name_case_insensitive() {
        // Category "cat1" has name "name-cat1" (per the `channel` helper).
        let channels = vec![
            channel("cat1", 4, None),
            channel("cat2", 4, None),
            channel("c1", 0, Some("cat1")),
        ];
        // Match by name (case-insensitive).
        let (resolved, unresolved) = resolve_categories(&channels, &["NAME-CAT1".to_string()]);
        let ids: Vec<&str> = resolved.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["cat1"]);
        assert!(unresolved.is_empty());
    }

    #[test]
    fn test_resolve_categories_by_id_and_dedup() {
        let channels = vec![channel("cat1", 4, None), channel("cat2", 4, None)];
        // Same category referenced by both id and name => returned once.
        let (resolved, unresolved) =
            resolve_categories(&channels, &["cat1".to_string(), "name-cat1".to_string()]);
        let ids: Vec<&str> = resolved.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["cat1"]);
        assert!(unresolved.is_empty());
    }

    #[test]
    fn test_resolve_categories_reports_unresolved() {
        let channels = vec![channel("cat1", 4, None)];
        let (resolved, unresolved) = resolve_categories(
            &channels,
            &["name-cat1".to_string(), "does-not-exist".to_string()],
        );
        let ids: Vec<&str> = resolved.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["cat1"]);
        assert_eq!(unresolved, vec!["does-not-exist".to_string()]);
    }

    #[test]
    fn test_resolve_categories_ignores_non_category_kinds() {
        // A text channel literally named like a category must not match.
        let channels = vec![channel("c1", 0, None)];
        let (resolved, unresolved) = resolve_categories(&channels, &["name-c1".to_string()]);
        assert!(resolved.is_empty());
        assert_eq!(unresolved, vec!["name-c1".to_string()]);
    }

    #[test]
    fn test_is_bot_message() {
        assert!(is_bot_message(&message(
            "1",
            Some(user("u", "bot", true)),
            "hi"
        )));
        assert!(!is_bot_message(&message(
            "2",
            Some(user("u", "alice", false)),
            "hi"
        )));

        // Webhook messages are kept even when author.bot is true.
        let mut webhook = message("3", Some(user("u", "wh", true)), "hi");
        webhook.webhook_id = Some("wh-1".to_string());
        assert!(!is_bot_message(&webhook));
    }

    #[test]
    fn test_map_message_fields() {
        let mut msg = message("42", Some(user("u9", "alice", false)), "hello");
        msg.message_reference = Some(claudear_integrations::discord::DiscordMessageReference {
            message_id: Some("7".to_string()),
            channel_id: None,
            guild_id: None,
            fail_if_not_exists: None,
        });

        let input = map_message(&msg, "chan1", "general", "guild1", true);
        assert_eq!(input.message_id, "42");
        assert_eq!(input.channel_id, "chan1");
        assert_eq!(input.channel_name, "general");
        assert_eq!(input.guild_id, "guild1");
        assert!(input.is_thread);
        assert_eq!(input.author, "alice");
        assert_eq!(input.content, "hello");
        assert_eq!(input.reply_to.as_deref(), Some("7"));
    }

    #[test]
    fn test_map_message_missing_author_defaults_unknown() {
        let input = map_message(&message("1", None, "x"), "c", "n", "g", false);
        assert_eq!(input.author, "unknown");
        assert_eq!(input.reply_to, None);
    }
}
