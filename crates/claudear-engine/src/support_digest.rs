//! Reads Discord support forum threads from the Appwrite project the threads bot
//! syncs them into, ranks them for the support digest, and keeps suggested
//! answers in that project's `drafts` table for review.
//!
//! Claudear never posts to the forum. Once a reviewer approves a draft, the
//! threads project's own function posts it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use claudear_config::{Config, SupportDigestConfig};
use claudear_core::error::{Error, Result};
use claudear_integrations::reports::{
    is_solved, SupportDigest, SupportMessage, SupportStatus, SupportThread,
};

/// Rows per Appwrite list request (the API caps a page at 100 rows and an
/// `equal` query at 100 values).
const PAGE_LIMIT: usize = 100;

/// Most rows read from one table per scan.
const MAX_ROWS: usize = 10_000;

/// How many likely-resolved threads a digest lists.
const RESOLVED_LIMIT: usize = 5;

/// Longest answer the `drafts` table stores.
const MAX_ANSWER_CHARS: usize = 16_384;

/// HTTP timeout for Appwrite requests.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Extension, on the database file name, of the file that keeps the last sent
/// digest across restarts.
const SENT_EXTENSION: &str = "support_digest_sent.json";

#[derive(Deserialize)]
struct ThreadRow {
    #[serde(rename = "$id")]
    id: String,
    title: String,
    author: String,
    author_id: Option<String>,
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct MessageRow {
    #[serde(rename = "$id")]
    id: String,
    #[serde(rename = "threadId")]
    thread_id: String,
    author: String,
    author_id: Option<String>,
    message: String,
    timestamp: String,
}

#[derive(Deserialize)]
struct AuthorRow {
    discord_id: String,
}

/// Where a suggested answer stands. The threads project posts `approved`
/// drafts and moves them to `sent` or `failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DraftStatus {
    Pending,
    Approved,
    Rejected,
    Sent,
    Failed,
}

/// A suggested answer in the `drafts` table; its row id is the thread id.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Draft {
    pub thread_id: String,
    pub answer: String,
    pub status: DraftStatus,
    /// The latest thread message when the answer was written.
    pub answered_message_id: Option<String>,
    pub reviewer: Option<String>,
    pub error: Option<String>,
    #[serde(rename = "$updatedAt")]
    pub updated_at: String,
}

/// A draft with its thread, as the dashboard reviews it.
#[derive(Debug, Clone, Serialize)]
pub struct DraftReview {
    pub thread_id: String,
    pub title: String,
    pub url: String,
    pub status: DraftStatus,
    pub answer: String,
    pub reviewer: Option<String>,
    pub error: Option<String>,
    pub updated_at: String,
    pub messages: Vec<ReviewMessage>,
}

/// One thread message shown next to a draft.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewMessage {
    pub author: String,
    /// Whether the thread's author wrote it.
    pub poster: bool,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

/// The Appwrite project the threads bot syncs the forum into.
pub struct ThreadsStore {
    http: reqwest::Client,
    config: SupportDigestConfig,
}

impl ThreadsStore {
    pub fn new(config: &SupportDigestConfig) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build()?;
        Ok(Self {
            http,
            config: config.clone(),
        })
    }

    /// Unresolved threads active since `since`, with their messages oldest
    /// first. Threads without messages are left out.
    pub async fn open_threads(&self, since: DateTime<Utc>) -> Result<Vec<SupportThread>> {
        // Rows synced before `is_resolved` existed leave it null; count them open.
        let rows: Vec<ThreadRow> = self
            .list_rows(
                "threads",
                vec![
                    json!({"method": "or", "values": [
                        {"method": "equal", "attribute": "is_resolved", "values": [false]},
                        {"method": "isNull", "attribute": "is_resolved"},
                    ]}),
                    json!({"method": "greaterThan", "attribute": "last_activity", "values": [since.to_rfc3339()]}),
                ],
            )
            .await?;
        let rows: Vec<ThreadRow> = rows
            .into_iter()
            .filter(|row| !is_solved(&row.title))
            .collect();
        self.with_messages(rows).await
    }

    /// Configured team user ids plus thread authors holding a team role. The
    /// authors table only has people who opened a thread, so most staff need
    /// listing in `team_user_ids`.
    pub async fn team(&self) -> Result<HashSet<String>> {
        let mut team: HashSet<String> = self.config.team_user_ids.iter().cloned().collect();
        if !self.config.team_roles.is_empty() {
            let authors: Vec<AuthorRow> = self
                .list_rows(
                    "authors",
                    vec![json!({"method": "contains", "attribute": "roles", "values": self.config.team_roles})],
                )
                .await?;
            team.extend(authors.into_iter().map(|author| author.discord_id));
        }
        Ok(team)
    }

    /// The draft for a thread, if one was written.
    pub async fn draft(&self, thread_id: &str) -> Result<Option<Draft>> {
        let body = self
            .request(Method::GET, &format!("drafts/rows/{thread_id}"), None)
            .await?;
        body.map(|body| {
            serde_json::from_str(&body).map_err(|e| Error::Other(format!("Invalid draft: {e}")))
        })
        .transpose()
    }

    /// Write a pending answer for the thread, replacing any earlier draft.
    pub async fn save_draft(&self, thread: &SupportThread, answer: &str) -> Result<()> {
        let answer: String = answer.chars().take(MAX_ANSWER_CHARS).collect();
        let data = json!({
            "threadId": thread.id,
            "answer": answer,
            "status": DraftStatus::Pending,
            "answeredMessageId": thread.messages.last().map(|message| &message.id),
            "reviewer": null,
            "sentMessageId": null,
            "error": null,
        });
        self.request(
            Method::PUT,
            &format!("drafts/rows/{}", thread.id),
            Some(json!({ "data": data })),
        )
        .await?
        .ok_or_else(|| Error::Other("drafts table not found".to_string()))?;
        Ok(())
    }

    /// Set a reviewer's decision, and their edited answer if given. Approving
    /// makes the threads project post the answer. `false` when no draft exists.
    pub async fn review_draft(
        &self,
        thread_id: &str,
        status: DraftStatus,
        answer: Option<&str>,
        reviewer: &str,
    ) -> Result<bool> {
        let mut data = json!({ "status": status, "reviewer": reviewer });
        if let Some(answer) = answer {
            data["answer"] = json!(answer.chars().take(MAX_ANSWER_CHARS).collect::<String>());
        }
        let updated = self
            .request(
                Method::PATCH,
                &format!("drafts/rows/{thread_id}"),
                Some(json!({ "data": data })),
            )
            .await?;
        Ok(updated.is_some())
    }

    /// How many drafts wait for a reviewer.
    pub async fn count_pending_drafts(&self) -> Result<usize> {
        #[derive(Deserialize)]
        struct Total {
            total: usize,
        }

        let query =
            json!({"method": "equal", "attribute": "status", "values": [DraftStatus::Pending]});
        let limit = json!({"method": "limit", "values": [1]});
        let body = self
            .request_with(
                Method::GET,
                "drafts/rows",
                &[
                    ("queries[]", query.to_string()),
                    ("queries[]", limit.to_string()),
                ],
                None,
            )
            .await?
            .unwrap_or_default();
        let total: Total = serde_json::from_str(&body)
            .map_err(|e| Error::Other(format!("Invalid drafts count: {e}")))?;
        Ok(total.total)
    }

    /// Drafts a reviewer still has to act on (pending or failed to post), with
    /// their threads, oldest first.
    pub async fn drafts_to_review(&self) -> Result<Vec<DraftReview>> {
        let drafts: Vec<Draft> = self
            .list_rows(
                "drafts",
                vec![
                    json!({"method": "equal", "attribute": "status", "values": [DraftStatus::Pending, DraftStatus::Failed]}),
                    json!({"method": "orderAsc", "attribute": "$updatedAt"}),
                ],
            )
            .await?;

        let mut threads: HashMap<String, SupportThread> = HashMap::new();
        for chunk in drafts.chunks(PAGE_LIMIT) {
            let ids: Vec<&str> = chunk.iter().map(|draft| draft.thread_id.as_str()).collect();
            let rows: Vec<ThreadRow> = self
                .list_rows(
                    "threads",
                    vec![json!({"method": "equal", "attribute": "$id", "values": ids})],
                )
                .await?;
            threads.extend(
                self.with_messages(rows)
                    .await?
                    .into_iter()
                    .map(|thread| (thread.id.clone(), thread)),
            );
        }

        Ok(drafts
            .into_iter()
            .filter_map(|draft| {
                let thread = threads.remove(&draft.thread_id)?;
                Some(DraftReview {
                    messages: thread
                        .messages
                        .iter()
                        .map(|message| ReviewMessage {
                            author: message.author.clone(),
                            poster: message.author_id == thread.owner_id,
                            content: message.content.clone(),
                            timestamp: message.timestamp,
                        })
                        .collect(),
                    thread_id: draft.thread_id,
                    title: thread.title,
                    url: thread.url,
                    status: draft.status,
                    answer: draft.answer,
                    reviewer: draft.reviewer,
                    error: draft.error,
                    updated_at: draft.updated_at,
                })
            })
            .collect())
    }

    /// Attach each thread's messages, oldest first, dropping threads without any.
    async fn with_messages(&self, rows: Vec<ThreadRow>) -> Result<Vec<SupportThread>> {
        let mut by_thread: HashMap<String, Vec<MessageRow>> = HashMap::new();
        for chunk in rows.chunks(PAGE_LIMIT) {
            let ids: Vec<&str> = chunk.iter().map(|row| row.id.as_str()).collect();
            let page: Vec<MessageRow> = self
                .list_rows(
                    "messages",
                    vec![json!({"method": "equal", "attribute": "threadId", "values": ids})],
                )
                .await?;
            for message in page {
                by_thread
                    .entry(message.thread_id.clone())
                    .or_default()
                    .push(message);
            }
        }

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let owner_id = row.author_id.clone().unwrap_or_default();
                let mut messages: Vec<SupportMessage> = by_thread
                    .remove(&row.id)?
                    .into_iter()
                    .filter_map(|message| {
                        let timestamp = DateTime::parse_from_rfc3339(&message.timestamp)
                            .ok()?
                            .with_timezone(&Utc);
                        // Older rows have no author id; match the poster by name.
                        let author_id = match message.author_id.filter(|id| !id.is_empty()) {
                            Some(id) => id,
                            None if message.author == row.author => owner_id.clone(),
                            None => format!("name:{}", message.author),
                        };
                        Some(SupportMessage {
                            id: message.id,
                            author_id,
                            author: message.author,
                            content: message.message,
                            timestamp,
                        })
                    })
                    .collect();
                messages.sort_by_key(|message| message.timestamp);
                Some(SupportThread {
                    url: format!(
                        "https://discord.com/channels/{}/{}",
                        self.config.guild_id, row.id
                    ),
                    id: row.id,
                    title: row.title,
                    owner_id,
                    tags: row.tags.unwrap_or_default(),
                    messages,
                })
            })
            .collect())
    }

    /// Every row of a table matching `queries`, paged by cursor up to [`MAX_ROWS`].
    async fn list_rows<T: DeserializeOwned>(
        &self,
        table: &str,
        queries: Vec<Value>,
    ) -> Result<Vec<T>> {
        #[derive(Deserialize)]
        struct Page {
            rows: Vec<Value>,
        }

        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        while rows.len() < MAX_ROWS {
            let mut params: Vec<(&str, String)> = queries
                .iter()
                .chain(&[json!({"method": "limit", "values": [PAGE_LIMIT]})])
                .map(|query| ("queries[]", query.to_string()))
                .collect();
            if let Some(cursor) = &cursor {
                params.push((
                    "queries[]",
                    json!({"method": "cursorAfter", "values": [cursor]}).to_string(),
                ));
            }
            params.push(("total", "false".to_string()));

            let body = self
                .request_with(Method::GET, &format!("{table}/rows"), &params, None)
                .await?
                .ok_or_else(|| Error::Other(format!("{table} table not found")))?;
            let page: Page = serde_json::from_str(&body)
                .map_err(|e| Error::Other(format!("Invalid {table} rows: {e}")))?;

            let full = page.rows.len() == PAGE_LIMIT;
            cursor = page
                .rows
                .last()
                .and_then(|row| row.get("$id"))
                .and_then(Value::as_str)
                .map(String::from);
            for row in page.rows {
                rows.push(
                    serde_json::from_value(row)
                        .map_err(|e| Error::Other(format!("Invalid {table} row: {e}")))?,
                );
            }
            if !full || cursor.is_none() {
                break;
            }
        }
        Ok(rows)
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Option<String>> {
        self.request_with(method, path, &[], body).await
    }

    /// Call `{endpoint}/tablesdb/{database}/tables/{path}`. `None` on a 404.
    async fn request_with(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Option<String>> {
        let url = format!(
            "{}/tablesdb/{}/tables/{}",
            self.config.endpoint.trim_end_matches('/'),
            self.config.database_id,
            path
        );
        let url = url::Url::parse_with_params(&url, params)
            .map_err(|e| Error::config(format!("Invalid support digest endpoint: {e}")))?;

        let mut request = self
            .http
            .request(method, url)
            .header("X-Appwrite-Project", &self.config.project_id);
        if let Some(key) = &self.config.api_key {
            request = request.header("X-Appwrite-Key", key.expose());
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(Error::network(format!(
                "Threads project request to {path} failed ({status}): {text}"
            )));
        }
        Ok(Some(text))
    }
}

/// Ranks the threads project's open threads into [`SupportDigest`]s.
pub struct SupportDigestOrchestrator {
    store: ThreadsStore,
    config: SupportDigestConfig,
    /// Needs-reply threads listed in the last digest that was sent.
    sent: Mutex<HashSet<String>>,
    /// Where `sent` is saved; `None` for an in-memory database.
    sent_path: Option<PathBuf>,
}

impl SupportDigestOrchestrator {
    /// Build from config, or `None` when the digest is disabled.
    pub fn from_config(config: &Config) -> Option<Self> {
        let cfg = &config.reports.support_digest;
        if !cfg.enabled {
            return None;
        }
        let store = match ThreadsStore::new(cfg) {
            Ok(store) => store,
            Err(e) => {
                tracing::warn!(error = %e, "Support digest HTTP client failed; skipping");
                return None;
            }
        };
        let sent_path = (config.db_path.as_os_str() != ":memory:")
            .then(|| config.db_path.with_extension(SENT_EXTENSION));
        let sent = sent_path.as_deref().map(load_sent).unwrap_or_default();
        Some(Self {
            store,
            config: cfg.clone(),
            sent: Mutex::new(sent),
            sent_path,
        })
    }

    pub fn store(&self) -> &ThreadsStore {
        &self.store
    }

    /// Rank the forum's open threads, returning the digest and the threads it
    /// was built from. Threads missing from the last sent digest are marked new.
    pub async fn collect(&self) -> Result<(SupportDigest, Vec<SupportThread>)> {
        let now = Utc::now();
        let since = now - Duration::days(self.config.days);
        let threads = self.store.open_threads(since).await?;
        let team = self.store.team().await?;

        let mut needs_reply = Vec::new();
        let mut likely_resolved = Vec::new();
        let mut waiting_on_user = 0;
        for entry in threads
            .iter()
            .filter_map(|thread| thread.triage(&team, now))
        {
            match entry.status {
                SupportStatus::NeedsReply => needs_reply.push(entry),
                SupportStatus::LikelyResolved => likely_resolved.push(entry),
                SupportStatus::WaitingOnUser => waiting_on_user += 1,
            }
        }
        needs_reply.sort_by_key(|entry| std::cmp::Reverse(entry.score));
        likely_resolved.sort_by_key(|entry| std::cmp::Reverse(entry.waiting_hours));
        let needs_reply_total = needs_reply.len();
        // The Discord message lists at most this many; anything cut here is never
        // marked sent, so it still counts as new later.
        needs_reply.truncate(
            self.config
                .max_entries
                .min(claudear_integrations::notifier::SUPPORT_DIGEST_MAX_ENTRIES),
        );
        likely_resolved.truncate(RESOLVED_LIMIT);

        let sent = self.sent.lock().unwrap();
        for entry in &mut needs_reply {
            entry.is_new = !sent.contains(&entry.thread_id);
        }
        drop(sent);

        let digest = SupportDigest {
            days: self.config.days,
            needs_reply,
            needs_reply_total,
            likely_resolved,
            waiting_on_user,
            drafts_to_review: 0,
        };
        Ok((digest, threads))
    }

    /// Remember which threads a sent digest listed, on disk too so a restart
    /// does not announce them again.
    pub fn mark_sent(&self, digest: &SupportDigest) {
        let sent: HashSet<String> = digest
            .needs_reply
            .iter()
            .map(|entry| entry.thread_id.clone())
            .collect();
        if let Some(path) = &self.sent_path {
            let json = serde_json::to_string(&sent).unwrap_or_default();
            if let Err(e) = std::fs::write(path, json) {
                tracing::warn!(path = %path.display(), error = %e, "Failed to save support digest state");
            }
        }
        *self.sent.lock().unwrap() = sent;
    }
}

/// Threads listed in the last sent digest, or none when the file is missing or
/// unreadable.
fn load_sent(path: &std::path::Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}
