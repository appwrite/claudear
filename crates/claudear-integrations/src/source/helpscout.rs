//! HelpScout issue source adapter.
//!
//! Treats each HelpScout conversation as an incoming "payload" and each mailbox
//! as an "inbox". Uses the Mailbox API v2 with OAuth2 client-credentials.

use super::ConversationStatus;
use super::IssueSource;
use super::ReporterReply;
use crate::notifier::html;
use abnegate_http::HttpClient;
use abnegate_http::HttpResponse;
use abnegate_http::ReqwestHttpClient;
use async_trait::async_trait;
use claudear_config::config::HelpScoutConfig;
use claudear_config::config::MailboxProfile;
use claudear_config::config::ReplyAs;
use claudear_config::config::ATTACHMENTS_METADATA_KEY;
use claudear_config::config::CUSTOMER_THREAD_COUNT_METADATA_KEY;
use claudear_config::config::HELPSCOUT_SOURCE;
use claudear_config::config::MAILBOX_ID_METADATA_KEY;
use claudear_config::config::MAILBOX_PROFILE_METADATA_KEY;
use claudear_core::error::Error;
use claudear_core::error::Result;
use claudear_core::types::Issue;
use claudear_core::types::MatchPriority;
use claudear_core::types::MatchResult;
use serde::Deserialize;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

/// Base URL for the HelpScout Mailbox API v2.
const HELPSCOUT_API_BASE: &str = "https://api.helpscout.net";

/// Refresh the access token this many seconds before it actually expires.
const TOKEN_EXPIRY_BUFFER_SECS: u64 = 60;

const SECURITY_DESCRIPTION_MAX_CHARS: usize = 20_000;
const SECURITY_THREAD_SEPARATOR: &str = "\n\n---\n\n";
const REPLY_ERROR_MAX_CHARS: usize = 500;

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct ConversationsListResponse {
    #[serde(default, rename = "_embedded")]
    embedded: Option<ConversationsEmbedded>,
}

#[derive(Debug, Default, Deserialize)]
struct ConversationsEmbedded {
    #[serde(default)]
    conversations: Vec<HsConversation>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HsConversation {
    id: u64,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    mailbox_id: Option<u64>,
    #[serde(default)]
    preview: Option<String>,
    #[serde(default)]
    tags: Vec<HsTag>,
    #[serde(default)]
    primary_customer: Option<HsCustomer>,
    #[serde(default, rename = "createdAt")]
    created_at: Option<String>,
    #[serde(default, rename = "userUpdatedAt")]
    user_updated_at: Option<String>,
    #[serde(default, rename = "_embedded")]
    embedded: Option<ThreadsEmbedded>,
}

impl HsConversation {
    fn customer_id(&self) -> Option<u64> {
        self.primary_customer
            .as_ref()
            .and_then(|customer| customer.id)
    }
}

#[derive(Debug, Default, Deserialize)]
struct HsTag {
    #[serde(default)]
    tag: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HsCustomer {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ThreadsListResponse {
    #[serde(default, rename = "_embedded")]
    embedded: Option<ThreadsEmbedded>,
}

#[derive(Debug, Default, Deserialize)]
struct ThreadsEmbedded {
    #[serde(default)]
    threads: Vec<HsThread>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HsThread {
    #[serde(default, rename = "type")]
    thread_type: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default, rename = "_embedded")]
    embedded: Option<AttachmentsEmbedded>,
}

impl HsThread {
    fn is_customer(&self) -> bool {
        self.thread_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("customer"))
    }
}

#[derive(Debug, Default, Deserialize)]
struct AttachmentsEmbedded {
    #[serde(default)]
    attachments: Vec<HsAttachment>,
}

#[derive(Debug, Default, Deserialize)]
struct HsAttachment {
    #[serde(default)]
    filename: Option<String>,
}

/// HelpScout source: polls conversations from one or more mailboxes.
pub struct HelpScoutSource<H: HttpClient = ReqwestHttpClient> {
    config: HelpScoutConfig,
    http: H,
    /// Cached OAuth2 access token and the instant it should be refreshed at.
    token: Mutex<Option<(String, Instant)>>,
}

impl HelpScoutSource<ReqwestHttpClient> {
    /// Create a HelpScout source with the default HTTP client.
    pub fn new(config: HelpScoutConfig) -> Self {
        let http = ReqwestHttpClient::new()
            .unwrap_or_else(|_| ReqwestHttpClient::from(reqwest::Client::new()));
        Self::with_http_client(config, http)
    }
}

impl<H: HttpClient> HelpScoutSource<H> {
    /// Create a HelpScout source with a custom HTTP client (for testing).
    pub fn with_http_client(config: HelpScoutConfig, http: H) -> Self {
        Self {
            config,
            http,
            token: Mutex::new(None),
        }
    }

    /// Return a valid bearer token, fetching a new one if needed.
    async fn access_token(&self) -> Result<String> {
        if let Ok(guard) = self.token.lock() {
            if let Some((cached, refresh_at)) = guard.as_ref() {
                if Instant::now() < *refresh_at {
                    return Ok(cached.clone());
                }
            }
        }

        let body = format!(
            "grant_type=client_credentials&client_id={}&client_secret={}",
            urlencoding(self.config.app_id.expose()),
            urlencoding(self.config.app_secret.expose()),
        );
        let response = self
            .http
            .post(
                &format!("{HELPSCOUT_API_BASE}/v2/oauth2/token"),
                vec![(
                    "Content-Type",
                    "application/x-www-form-urlencoded".to_string(),
                )],
                &body,
            )
            .await?;
        if !response.is_success() {
            return Err(Error::source(
                HELPSCOUT_SOURCE,
                format!(
                    "token request failed ({}): {}",
                    response.status, response.body
                ),
            ));
        }
        let token: TokenResponse = response.json()?;
        let ttl = token
            .expires_in
            .saturating_sub(TOKEN_EXPIRY_BUFFER_SECS)
            .max(1);
        if let Ok(mut guard) = self.token.lock() {
            *guard = Some((
                token.access_token.clone(),
                Instant::now() + Duration::from_secs(ttl),
            ));
        }
        Ok(token.access_token)
    }

    /// Authorized GET returning the raw response.
    async fn api_get(&self, path_and_query: &str) -> Result<HttpResponse> {
        let token = self.access_token().await?;
        Ok(self
            .http
            .get(
                &format!("{HELPSCOUT_API_BASE}{path_and_query}"),
                vec![("Authorization", format!("Bearer {token}"))],
            )
            .await?)
    }

    /// Mailboxes polled for processing. Security mailboxes are polled only
    /// while the security inbox is enabled.
    fn polled_mailboxes(&self) -> Vec<String> {
        self.config
            .all_mailbox_ids()
            .into_iter()
            .filter(|mailbox| {
                self.config.security.enabled || !self.config.is_security_mailbox(mailbox)
            })
            .collect()
    }

    /// Mailboxes whose conversations may be listed to tools and humans.
    fn listable_mailboxes(&self) -> Vec<String> {
        self.config
            .all_mailbox_ids()
            .into_iter()
            .filter(|mailbox| !self.config.is_security_mailbox(mailbox))
            .collect()
    }

    /// List conversations in `mailboxes`, each filtered by `status` or, when
    /// none is requested, by that mailbox's trigger status.
    async fn fetch_mailboxes(
        &self,
        mailboxes: &[String],
        status: Option<&str>,
    ) -> Result<Vec<Issue>> {
        let mut issues = Vec::new();
        for mailbox in mailboxes {
            let status = status.unwrap_or_else(|| self.config.trigger_status_for(mailbox));
            let path = format!(
                "/v2/conversations?mailbox={}&status={}&embed=threads",
                urlencoding(mailbox),
                urlencoding(status),
            );
            let response = self.api_get(&path).await?;
            if !response.is_success() {
                return Err(Error::source(
                    HELPSCOUT_SOURCE,
                    format!(
                        "list conversations failed ({}): {}",
                        response.status, response.body
                    ),
                ));
            }
            let list: ConversationsListResponse = response.json()?;
            if let Some(embedded) = list.embedded {
                for conversation in embedded.conversations {
                    issues.push(self.map_conversation(conversation, Some(mailbox)));
                }
            }
        }
        Ok(issues)
    }

    /// Fetch the threads for a conversation (used to build full context).
    async fn fetch_threads(&self, conversation_id: &str) -> Vec<HsThread> {
        match self
            .api_get(&format!("/v2/conversations/{conversation_id}/threads"))
            .await
        {
            Ok(response) if response.is_success() => response
                .json::<ThreadsListResponse>()
                .ok()
                .and_then(|threads| threads.embedded)
                .map(|embedded| embedded.threads)
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    async fn fetch_conversation(&self, issue_id: &str) -> Result<HsConversation> {
        let response = self
            .api_get(&format!("/v2/conversations/{issue_id}"))
            .await?;
        if response.is_not_found() {
            return Err(Error::issue_not_found(HELPSCOUT_SOURCE, issue_id));
        }
        if !response.is_success() {
            return Err(Error::source(
                HELPSCOUT_SOURCE,
                format!(
                    "get conversation failed ({}): {}",
                    response.status, response.body
                ),
            ));
        }
        Ok(response.json()?)
    }

    /// Customer to address a comment to, or `None` to post an internal note.
    /// Security mailboxes, and conversations whose mailbox can't be read, only
    /// ever get notes.
    async fn comment_recipient(&self, issue_id: &str) -> Result<Option<String>> {
        let conversation = match self.fetch_conversation(issue_id).await {
            Ok(conversation) => conversation,
            Err(error) => {
                tracing::warn!(
                    source = HELPSCOUT_SOURCE,
                    issue_id = %issue_id,
                    error = %error,
                    "Conversation lookup failed; posting the comment as a note"
                );
                return Ok(None);
            }
        };
        let mailbox_id = conversation.mailbox_id.map(|id| id.to_string());
        if self.config.treats_as_security(mailbox_id.as_deref()) {
            return Ok(None);
        }
        let reply_as = mailbox_id
            .as_deref()
            .map(|mailbox| self.config.reply_as_for(mailbox))
            .unwrap_or(self.config.reply_as);
        match reply_as {
            ReplyAs::Note => Ok(None),
            ReplyAs::Reply => conversation
                .customer_id()
                .map(|id| Some(id.to_string()))
                .ok_or_else(|| {
                    Error::source(
                        HELPSCOUT_SOURCE,
                        "conversation has no customer id for reply",
                    )
                }),
        }
    }

    async fn update_status(&self, issue_id: &str, status: &str, action: &str) -> Result<()> {
        // HelpScout's update endpoint takes one JSON Patch operation as an
        // object; an array is rejected with "Error parsing request body into JSON".
        let token = self.access_token().await?;
        let body =
            serde_json::json!({ "op": "replace", "path": "/status", "value": status }).to_string();
        let response = self
            .http
            .patch(
                &format!("{HELPSCOUT_API_BASE}/v2/conversations/{issue_id}"),
                vec![
                    ("Authorization", format!("Bearer {token}")),
                    ("Content-Type", "application/json".to_string()),
                ],
                &body,
            )
            .await?;
        if response.is_success() {
            Ok(())
        } else {
            Err(Error::source(
                HELPSCOUT_SOURCE,
                format!("{action} failed ({}): {}", response.status, response.body),
            ))
        }
    }

    /// Map a HelpScout conversation to a generic `Issue`. `polled_mailbox` is
    /// the mailbox the conversation was listed from, used when the payload
    /// omits `mailboxId`.
    fn map_conversation(&self, c: HsConversation, polled_mailbox: Option<&str>) -> Issue {
        let tags: Vec<String> = c.tags.iter().map(|t| t.tag.clone()).collect();
        let number = c.number.unwrap_or(c.id);
        let short_id = format!("HS-{number}");
        let title = c
            .subject
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("Conversation {number}"));
        let url = format!("https://secure.helpscout.net/conversation/{}", c.id);

        let mut issue = Issue::new(c.id.to_string(), short_id, title, url, HELPSCOUT_SOURCE);

        // Capture timestamps so consumers (e.g. the MCP list tool) can order
        // conversations globally; `updated_at` falls back to `created_at`.
        issue.created_at = c.created_at.as_deref().and_then(parse_rfc3339);
        issue.updated_at = c
            .user_updated_at
            .as_deref()
            .and_then(parse_rfc3339)
            .or(issue.created_at);

        let mailbox_id = c
            .mailbox_id
            .map(|id| id.to_string())
            .or_else(|| polled_mailbox.map(str::to_string));
        let security = self.config.treats_as_security(mailbox_id.as_deref());
        let threads: &[HsThread] = c
            .embedded
            .as_ref()
            .map(|embedded| embedded.threads.as_slice())
            .unwrap_or_default();
        let customer_threads = customer_threads_oldest_first(threads);

        issue.description = if security {
            security_description(&customer_threads)
                .or_else(|| c.preview.as_deref().map(html_to_text))
        } else {
            // Prefer the latest customer thread body; fall back to the preview.
            latest_customer_body(threads).or_else(|| c.preview.clone())
        };

        let profile = if security {
            MailboxProfile::Security
        } else {
            MailboxProfile::Support
        };
        issue.set_metadata(MAILBOX_PROFILE_METADATA_KEY, profile.as_str());
        issue.set_metadata(
            ATTACHMENTS_METADATA_KEY,
            attachment_names(&customer_threads),
        );
        issue.set_metadata(
            CUSTOMER_THREAD_COUNT_METADATA_KEY,
            u32::try_from(customer_threads.len()).unwrap_or(u32::MAX),
        );
        if let Some(mailbox_id) = mailbox_id {
            issue.set_metadata(MAILBOX_ID_METADATA_KEY, mailbox_id);
        }
        issue.set_metadata("tags", &tags);
        // `labels` is the key the pipeline reads to populate FixAttempt.issue_labels
        // (used by the bug-detection heuristic).
        issue.set_metadata("labels", &tags);
        if let Some(status) = c.status.as_ref() {
            issue.set_metadata("status", status);
        }
        if let Some(customer) = c.primary_customer.as_ref() {
            if let Some(email) = customer.email.as_ref() {
                issue.set_metadata("customer_email", email);
            }
            if let Some(id) = customer.id {
                issue.set_metadata("customer_id", id.to_string());
            }
        }
        issue
    }
}

/// Parse an RFC3339 timestamp into UTC, ignoring malformed values.
fn parse_rfc3339(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// Return the body of the most recent customer-authored thread, if any.
fn latest_customer_body(threads: &[HsThread]) -> Option<String> {
    threads
        .iter()
        .filter(|t| t.is_customer())
        .filter_map(|t| t.body.clone())
        .rfind(|b| !b.trim().is_empty())
}

/// Customer threads, oldest first. HelpScout lists threads newest first; when
/// every thread carries a timestamp, the timestamps decide.
fn customer_threads_oldest_first(threads: &[HsThread]) -> Vec<&HsThread> {
    let mut customer: Vec<&HsThread> = threads.iter().filter(|t| t.is_customer()).collect();
    customer.reverse();
    let timestamps: Option<Vec<_>> = customer
        .iter()
        .map(|thread| thread.created_at.as_deref().and_then(parse_rfc3339))
        .collect();
    if let Some(timestamps) = timestamps {
        let mut ordered: Vec<_> = timestamps.into_iter().zip(customer).collect();
        ordered.sort_by_key(|(timestamp, _)| *timestamp);
        return ordered.into_iter().map(|(_, thread)| thread).collect();
    }
    customer
}

/// Every customer thread as plain text, joined and capped.
fn security_description(customer_threads: &[&HsThread]) -> Option<String> {
    let bodies: Vec<String> = customer_threads
        .iter()
        .filter_map(|thread| thread.body.as_deref())
        .map(html_to_text)
        .filter(|text| !text.is_empty())
        .collect();
    if bodies.is_empty() {
        return None;
    }
    let joined = bodies.join(SECURITY_THREAD_SEPARATOR);
    Some(truncate_chars(&joined, SECURITY_DESCRIPTION_MAX_CHARS))
}

fn attachment_names(customer_threads: &[&HsThread]) -> Vec<String> {
    customer_threads
        .iter()
        .filter_map(|thread| thread.embedded.as_ref())
        .flat_map(|embedded| embedded.attachments.iter())
        .filter_map(|attachment| attachment.filename.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

fn truncate_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    }
}

const PARAGRAPH_TAGS: &[&str] = &[
    "p",
    "blockquote",
    "pre",
    "hr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
];
const LINE_TAGS: &[&str] = &["div", "li", "ul", "ol", "tr", "table"];
const SKIPPED_TAGS: &[&str] = &["script", "style", "head"];

/// Convert an HTML thread body to plain text: tags are dropped (block tags
/// become line or paragraph breaks, script/style contents vanish), entities are
/// decoded and blank lines are collapsed.
/// `text` as HelpScout thread HTML: markup characters escaped so reporter- or
/// agent-derived text renders literally, and line breaks kept as `<br>`.
fn plain_text_html(text: &str) -> String {
    html::escape(&text.replace("\r\n", "\n")).replace('\n', "<br>")
}

fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        text.push_str(&rest[..start]);
        let candidate = &rest[start..];
        let opens_markup = candidate[1..]
            .chars()
            .next()
            .is_some_and(|next| next.is_ascii_alphabetic() || next == '/' || next == '!');
        if !opens_markup {
            text.push('<');
            rest = &candidate[1..];
            continue;
        }
        if candidate.starts_with("<!--") {
            rest = candidate
                .find("-->")
                .map(|end| &candidate[end + 3..])
                .unwrap_or("");
            continue;
        }
        let Some(end) = candidate.find('>') else {
            rest = "";
            break;
        };
        let tag = &candidate[1..end];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        rest = &candidate[end + 1..];
        if !closing && SKIPPED_TAGS.contains(&name.as_str()) {
            let terminator = format!("</{name}");
            rest = find_ascii_case_insensitive(rest, &terminator)
                .map(|position| {
                    let after = &rest[position..];
                    after
                        .find('>')
                        .map(|close| &after[close + 1..])
                        .unwrap_or("")
                })
                .unwrap_or("");
            continue;
        }
        if name == "br" {
            text.push('\n');
        } else if PARAGRAPH_TAGS.contains(&name.as_str()) {
            end_with_newlines(&mut text, 2);
        } else if LINE_TAGS.contains(&name.as_str()) {
            end_with_newlines(&mut text, 1);
        }
    }
    text.push_str(rest);
    normalize_lines(&decode_entities(&text))
}

fn end_with_newlines(text: &mut String, count: usize) {
    if text.is_empty() {
        return;
    }
    let trailing = text.chars().rev().take_while(|c| *c == '\n').count();
    for _ in trailing..count {
        text.push('\n');
    }
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn decode_entities(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let candidate = &rest[start..];
        let entity = candidate
            .char_indices()
            .take(12)
            .find(|(_, c)| *c == ';')
            .and_then(|(end, _)| decode_entity(&candidate[1..end]).map(|c| (c, end)));
        match entity {
            Some((character, end)) => {
                decoded.push(character);
                rest = &candidate[end + 1..];
            }
            None => {
                decoded.push('&');
                rest = &candidate[1..];
            }
        }
    }
    decoded.push_str(rest);
    decoded
}

fn decode_entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let code = name.strip_prefix('#')?;
            let value = match code.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => code.parse().ok()?,
            };
            char::from_u32(value)
        }
    }
}

/// Trim each line and collapse runs of blank lines to one.
fn normalize_lines(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() && lines.last().is_none_or(|last| last.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn is_conversation_id(issue_id: &str) -> bool {
    !issue_id.is_empty() && issue_id.bytes().all(|byte| byte.is_ascii_digit())
}

fn truncated_body(body: &str) -> String {
    truncate_chars(body, REPLY_ERROR_MAX_CHARS)
}

/// Classify a transport failure on the reply request. Only failures that
/// provably happened before anything reached HelpScout count as rejected;
/// everything else may have delivered the reply.
fn classify_reply_failure(error: &abnegate_http::Error) -> ReporterReply {
    use abnegate_http::Error as HttpError;
    match error {
        HttpError::Request(inner) if inner.is_connect() => {
            ReporterReply::Rejected(format!("could not connect to HelpScout: {error}"))
        }
        HttpError::InvalidUrl
        | HttpError::InvalidHeaderName
        | HttpError::InvalidHeaderValue
        | HttpError::UnsupportedScheme
        | HttpError::EmbeddedCredentials
        | HttpError::MissingHost
        | HttpError::PrivateAddress
        | HttpError::InternalHost
        | HttpError::Unsupported(_)
        | HttpError::UnfetchableResolution { .. } => {
            ReporterReply::Rejected(format!("reply request was not sent: {error}"))
        }
        _ => ReporterReply::Unconfirmed(format!("reply delivery is unknown: {error}")),
    }
}

/// Whether a conversation passes `trigger_tags`. No trigger tags matches every
/// conversation.
pub(crate) fn helpscout_matches_trigger_tags(
    trigger_tags: &[String],
    issue: &Issue,
) -> MatchResult {
    if trigger_tags.is_empty() {
        return MatchResult::matched("No trigger tags configured", MatchPriority::Normal);
    }
    let tags: Vec<String> = issue.get_metadata("tags").unwrap_or_default();
    let matched = trigger_tags
        .iter()
        .any(|trigger| tags.iter().any(|t| t.eq_ignore_ascii_case(trigger)));
    if matched {
        MatchResult::matched("Matches trigger tags", MatchPriority::Normal)
    } else {
        MatchResult::not_matched("No matching trigger tags")
    }
}

/// Minimal application/x-www-form-urlencoded encoder for token request values.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build a Claude-facing context string from a HelpScout conversation issue.
fn format_helpscout_context(issue: &Issue) -> String {
    let mut ctx = format!("# HelpScout Conversation: {}\n\n", issue.short_id);
    ctx.push_str(&format!("**Subject:** {}\n", issue.title));
    ctx.push_str(&format!("**URL:** {}\n", issue.url));
    if let Some(status) = issue.get_metadata::<String>("status") {
        ctx.push_str(&format!("**Status:** {status}\n"));
    }
    if let Some(email) = issue.get_metadata::<String>("customer_email") {
        ctx.push_str(&format!("**Customer:** {email}\n"));
    }
    let tags: Vec<String> = issue.get_metadata("tags").unwrap_or_default();
    if !tags.is_empty() {
        ctx.push_str(&format!("**Tags:** {}\n", tags.join(", ")));
    }
    ctx.push('\n');
    if let Some(desc) = issue.description.as_ref() {
        ctx.push_str(&format!("## Customer message\n{desc}\n"));
    }
    ctx
}

#[async_trait]
impl<H: HttpClient + 'static> IssueSource for HelpScoutSource<H> {
    fn name(&self) -> &str {
        HELPSCOUT_SOURCE
    }

    fn display_name(&self) -> &str {
        "HelpScout"
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        self.fetch_mailboxes(&self.polled_mailboxes(), None).await
    }

    async fn list_conversations(&self, status: Option<&str>) -> Result<Vec<Issue>> {
        let status = status.map(str::trim).filter(|s| !s.is_empty());
        let issues = self
            .fetch_mailboxes(&self.listable_mailboxes(), status)
            .await?;
        Ok(issues
            .into_iter()
            .filter(|issue| {
                !self.config.treats_as_security(
                    issue
                        .get_metadata::<String>(MAILBOX_ID_METADATA_KEY)
                        .as_deref(),
                )
            })
            .collect())
    }

    fn matches_criteria(&self, issue: &Issue) -> MatchResult {
        let mailbox_id: Option<String> = issue.get_metadata(MAILBOX_ID_METADATA_KEY);
        let trigger_tags = match mailbox_id.as_deref() {
            Some(mailbox) => self.config.trigger_tags_for(mailbox),
            None if self.config.has_security_mailbox() => {
                return MatchResult::not_matched(
                    "Conversation has no mailbox id while a security mailbox is configured",
                );
            }
            None => &self.config.trigger_tags,
        };
        helpscout_matches_trigger_tags(trigger_tags, issue)
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        Ok(format_helpscout_context(issue))
    }

    async fn get_issue(&self, issue_id: &str) -> Result<Issue> {
        let mut conversation = self.fetch_conversation(issue_id).await?;
        // Ensure we have the full thread history for context.
        if conversation
            .embedded
            .as_ref()
            .map(|e| e.threads.is_empty())
            .unwrap_or(true)
        {
            let threads = self.fetch_threads(issue_id).await;
            conversation.embedded = Some(ThreadsEmbedded { threads });
        }
        Ok(self.map_conversation(conversation, None))
    }

    async fn resolve_issue(&self, issue_id: &str) -> Result<()> {
        self.update_status(issue_id, "closed", "close conversation")
            .await
    }

    async fn add_comment(&self, issue_id: &str, comment: &str) -> Result<()> {
        let token = self.access_token().await?;
        let text =
            serde_json::to_string(comment).map_err(|e| Error::Other(format!("JSON error: {e}")))?;

        let recipient = if self.config.has_mailbox_overrides() {
            self.comment_recipient(issue_id).await?
        } else {
            match self.config.reply_as {
                ReplyAs::Note => None,
                ReplyAs::Reply => {
                    // A customer-facing reply requires the customer id.
                    let issue = self.get_issue(issue_id).await?;
                    Some(issue.get_metadata::<String>("customer_id").ok_or_else(|| {
                        Error::source(
                            HELPSCOUT_SOURCE,
                            "conversation has no customer id for reply",
                        )
                    })?)
                }
            }
        };

        // Note (internal) is the safe default; reply is customer-facing.
        let (endpoint, body) = match recipient {
            None => (
                format!("/v2/conversations/{issue_id}/notes"),
                format!("{{\"text\":{text}}}"),
            ),
            Some(customer_id) => (
                format!("/v2/conversations/{issue_id}/reply"),
                format!("{{\"text\":{text},\"customer\":{{\"id\":{customer_id}}}}}"),
            ),
        };

        let response = self
            .http
            .post(
                &format!("{HELPSCOUT_API_BASE}{endpoint}"),
                vec![
                    ("Authorization", format!("Bearer {token}")),
                    ("Content-Type", "application/json".to_string()),
                ],
                &body,
            )
            .await?;
        if response.is_success() {
            Ok(())
        } else {
            Err(Error::source(
                HELPSCOUT_SOURCE,
                format!("post reply failed ({}): {}", response.status, response.body),
            ))
        }
    }

    async fn reply_to_reporter(
        &self,
        issue_id: &str,
        body: &str,
        status: Option<ConversationStatus>,
    ) -> ReporterReply {
        if !is_conversation_id(issue_id) {
            return ReporterReply::Rejected(format!("invalid conversation id '{issue_id}'"));
        }
        let conversation = match self.fetch_conversation(issue_id).await {
            Ok(conversation) => conversation,
            Err(error) => {
                return ReporterReply::Rejected(format!("conversation lookup failed: {error}"))
            }
        };
        let Some(mailbox_id) = conversation.mailbox_id.map(|id| id.to_string()) else {
            return ReporterReply::Rejected("conversation has no mailbox id".to_string());
        };
        if !self.config.is_security_mailbox(&mailbox_id) {
            return ReporterReply::Rejected(format!(
                "mailbox {mailbox_id} is not a security mailbox"
            ));
        }
        let Some(customer_id) = conversation.customer_id() else {
            return ReporterReply::Rejected("conversation has no customer id".to_string());
        };
        let token = match self.access_token().await {
            Ok(token) => token,
            Err(error) => return ReporterReply::Rejected(format!("token request failed: {error}")),
        };

        let mut payload = serde_json::json!({
            "text": plain_text_html(body),
            "customer": { "id": customer_id },
        });
        if let Some(status) = status {
            payload["status"] = serde_json::Value::from(status.as_str());
        }

        let response = self
            .http
            .post(
                &format!("{HELPSCOUT_API_BASE}/v2/conversations/{issue_id}/reply"),
                vec![
                    ("Authorization", format!("Bearer {token}")),
                    ("Content-Type", "application/json".to_string()),
                ],
                &payload.to_string(),
            )
            .await;
        match response {
            Ok(response) if response.is_success() => ReporterReply::Sent,
            Ok(response) if (400..500).contains(&response.status) => {
                ReporterReply::Rejected(format!(
                    "reply rejected ({}): {}",
                    response.status,
                    truncated_body(&response.body)
                ))
            }
            Ok(response) => ReporterReply::Unconfirmed(format!(
                "reply status unknown ({}): {}",
                response.status,
                truncated_body(&response.body)
            )),
            Err(error) => classify_reply_failure(&error),
        }
    }

    async fn mark_spam(&self, issue_id: &str) -> Result<()> {
        if !is_conversation_id(issue_id) {
            return Err(Error::source(
                HELPSCOUT_SOURCE,
                format!("invalid conversation id '{issue_id}'"),
            ));
        }
        self.update_status(issue_id, "spam", "mark conversation as spam")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abnegate_http::HttpResponse;
    use claudear_config::config::HelpScoutMailboxConfig;
    use claudear_config::config::SecurityInboxConfig;

    enum StubOutcome {
        Respond(u16, String),
        Fail(Mutex<Option<abnegate_http::Error>>),
    }

    struct Stub {
        method: Option<&'static str>,
        key: String,
        outcome: StubOutcome,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Recorded {
        method: String,
        url: String,
        body: String,
    }

    /// Mock HTTP client: returns canned responses keyed by a substring of the
    /// URL, optionally restricted to one method. Stubs added later win.
    struct MockHttpClient {
        stubs: Mutex<Vec<Stub>>,
        requests: Mutex<Vec<Recorded>>,
    }

    impl MockHttpClient {
        fn new(responses: Vec<(&str, u16, &str)>) -> Self {
            Self {
                stubs: Mutex::new(
                    responses
                        .into_iter()
                        .map(|(key, status, body)| Stub {
                            method: None,
                            key: key.to_string(),
                            outcome: StubOutcome::Respond(status, body.to_string()),
                        })
                        .collect(),
                ),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn on(self, method: &'static str, key: &str, status: u16, body: &str) -> Self {
            self.stubs.lock().unwrap().insert(
                0,
                Stub {
                    method: Some(method),
                    key: key.to_string(),
                    outcome: StubOutcome::Respond(status, body.to_string()),
                },
            );
            self
        }

        fn failing(self, method: &'static str, key: &str, error: abnegate_http::Error) -> Self {
            self.stubs.lock().unwrap().insert(
                0,
                Stub {
                    method: Some(method),
                    key: key.to_string(),
                    outcome: StubOutcome::Fail(Mutex::new(Some(error))),
                },
            );
            self
        }

        fn recorded(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }

        fn requests_to(&self, method: &str, fragment: &str) -> Vec<Recorded> {
            self.recorded()
                .into_iter()
                .filter(|request| request.method == method && request.url.contains(fragment))
                .collect()
        }

        fn respond(
            &self,
            method: &str,
            url: &str,
            body: &str,
        ) -> abnegate_http::Result<HttpResponse> {
            self.requests.lock().unwrap().push(Recorded {
                method: method.to_string(),
                url: url.to_string(),
                body: body.to_string(),
            });
            let stubs = self.stubs.lock().unwrap();
            let stub = stubs.iter().find(|stub| {
                stub.method.is_none_or(|expected| expected == method) && url.contains(&stub.key)
            });
            match stub.map(|stub| &stub.outcome) {
                Some(StubOutcome::Respond(status, body)) => {
                    Ok(HttpResponse::new(*status, body.clone()))
                }
                Some(StubOutcome::Fail(error)) => Err(error
                    .lock()
                    .unwrap()
                    .take()
                    .expect("a failing stub fails once")),
                None => Ok(HttpResponse::new(404, "{}")),
            }
        }
    }

    #[async_trait]
    impl HttpClient for MockHttpClient {
        async fn get(
            &self,
            url: &str,
            _headers: Vec<(&str, String)>,
        ) -> abnegate_http::Result<HttpResponse> {
            self.respond("GET", url, "")
        }

        async fn post(
            &self,
            url: &str,
            _headers: Vec<(&str, String)>,
            body: &str,
        ) -> abnegate_http::Result<HttpResponse> {
            self.respond("POST", url, body)
        }

        async fn patch(
            &self,
            url: &str,
            _headers: Vec<(&str, String)>,
            body: &str,
        ) -> abnegate_http::Result<HttpResponse> {
            self.respond("PATCH", url, body)
        }
    }

    fn test_config() -> HelpScoutConfig {
        HelpScoutConfig {
            enabled: true,
            app_id: "app".into(),
            app_secret: "secret".into(),
            mailbox_ids: vec!["1".to_string()],
            trigger_tags: vec!["bug".to_string()],
            trigger_status: "active".to_string(),
            reply_as: ReplyAs::Note,
            webhook_secret: None,
            max_issues_per_cycle: None,
            max_concurrent: None,
            poll_interval_ms: None,
            mailboxes: Vec::new(),
            security: SecurityInboxConfig::default(),
        }
    }

    const SUPPORT_MAILBOX: &str = "249877";
    const SECURITY_MAILBOX: &str = "249891";

    fn security_config() -> HelpScoutConfig {
        HelpScoutConfig {
            mailbox_ids: vec![SUPPORT_MAILBOX.to_string()],
            trigger_tags: vec!["auto-implement".to_string(), "claude".to_string()],
            mailboxes: vec![HelpScoutMailboxConfig {
                id: SECURITY_MAILBOX.to_string(),
                profile: MailboxProfile::Security,
                reply_as: None,
                trigger_tags: Some(Vec::new()),
                trigger_status: Some("active".to_string()),
            }],
            security: SecurityInboxConfig {
                enabled: true,
                ..Default::default()
            },
            ..test_config()
        }
    }

    fn conversation_body(mailbox_id: &str, customer_id: Option<u64>) -> String {
        let customer = customer_id
            .map(|id| format!(r#","primaryCustomer":{{"id":{id},"email":"r@x.io"}}"#))
            .unwrap_or_default();
        format!(
            r#"{{"id":42,"number":7,"subject":"s","status":"active","mailboxId":{mailbox_id}{customer}}}"#
        )
    }

    async fn transport_error_after_connect() -> abnegate_http::Error {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });
        let error = claudear_core::tls::client_builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://{address}/v2/conversations/42/reply"))
            .body("{}")
            .send()
            .await
            .expect_err("the server hangs up without answering");
        server.await.unwrap();
        assert!(!error.is_connect(), "{error:?}");
        abnegate_http::Error::from(error)
    }

    async fn connect_error() -> abnegate_http::Error {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = claudear_core::tls::client_builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://{address}/"))
            .send()
            .await
            .expect_err("nothing listens there");
        assert!(error.is_connect(), "{error:?}");
        abnegate_http::Error::from(error)
    }

    const TOKEN_BODY: &str =
        r#"{"access_token":"tok-123","expires_in":3600,"token_type":"bearer"}"#;

    #[tokio::test]
    async fn test_fetch_issues_maps_conversations() {
        let list = r#"{"_embedded":{"conversations":[
            {"id":42,"number":7,"subject":"Login crashes","status":"active","mailboxId":1,
             "preview":"It crashes on submit","tags":[{"id":1,"tag":"bug"}],
             "primaryCustomer":{"id":99,"email":"a@b.com"}}
        ]}}"#;
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("/v2/conversations?", 200, list),
        ]);
        let source = HelpScoutSource::with_http_client(test_config(), mock);

        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 1);
        let i = &issues[0];
        assert_eq!(i.id, "42");
        assert_eq!(i.short_id, "HS-7");
        assert_eq!(i.title, "Login crashes");
        assert_eq!(i.source, "helpscout");
        assert_eq!(i.description.as_deref(), Some("It crashes on submit"));
        assert_eq!(i.get_metadata::<String>("mailbox_id").as_deref(), Some("1"));
        assert_eq!(
            i.get_metadata::<String>("customer_email").as_deref(),
            Some("a@b.com")
        );
        assert_eq!(
            i.get_metadata::<Vec<String>>("labels"),
            Some(vec!["bug".to_string()])
        );
    }

    #[tokio::test]
    async fn test_list_conversations_forwards_status() {
        let list = r#"{"_embedded":{"conversations":[
            {"id":1,"number":1,"subject":"Closed one","status":"closed","mailboxId":1,
             "primaryCustomer":{"id":1,"email":"a@b.com"}}
        ]}}"#;
        // The conversations response only matches when the URL carries
        // status=closed, so a successful fetch proves the status was forwarded.
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("status=closed", 200, list),
        ]);
        let source = HelpScoutSource::with_http_client(test_config(), mock);
        let issues = source.list_conversations(Some("closed")).await.unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "1");

        // A different status does not hit the status=closed stub -> 404 -> error,
        // confirming the status really goes into the query (not hardcoded).
        let mock2 = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("status=closed", 200, list),
        ]);
        let source2 = HelpScoutSource::with_http_client(test_config(), mock2);
        assert!(source2.list_conversations(Some("active")).await.is_err());
    }

    #[test]
    fn test_matches_criteria_by_tag() {
        let source = HelpScoutSource::with_http_client(test_config(), MockHttpClient::new(vec![]));
        let mut issue = Issue::new("1", "HS-1", "t", "u", "helpscout");
        issue.set_metadata("tags", vec!["bug".to_string(), "p1".to_string()]);
        assert!(source.matches_criteria(&issue).matches);

        let mut other = Issue::new("2", "HS-2", "t", "u", "helpscout");
        other.set_metadata("tags", vec!["question".to_string()]);
        assert!(!source.matches_criteria(&other).matches);
    }

    #[test]
    fn test_matches_criteria_no_trigger_tags_matches_all() {
        let mut cfg = test_config();
        cfg.trigger_tags = vec![];
        let source = HelpScoutSource::with_http_client(cfg, MockHttpClient::new(vec![]));
        let issue = Issue::new("1", "HS-1", "t", "u", "helpscout");
        assert!(source.matches_criteria(&issue).matches);
    }

    #[tokio::test]
    async fn test_add_comment_posts_note() {
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("/notes", 201, ""),
        ]);
        let source = HelpScoutSource::with_http_client(test_config(), mock);
        source
            .add_comment("42", "Thanks for the report!")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_resolve_issue_closes_conversation() {
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("/v2/conversations/42", 200, ""),
        ]);
        let source = HelpScoutSource::with_http_client(test_config(), mock);
        source.resolve_issue("42").await.unwrap();
    }

    #[test]
    fn test_latest_customer_body_prefers_last_customer_thread() {
        let threads = vec![
            HsThread {
                thread_type: Some("customer".into()),
                body: Some("first".into()),
                ..Default::default()
            },
            HsThread {
                thread_type: Some("note".into()),
                body: Some("internal".into()),
                ..Default::default()
            },
            HsThread {
                thread_type: Some("customer".into()),
                body: Some("latest".into()),
                ..Default::default()
            },
        ];
        assert_eq!(latest_customer_body(&threads).as_deref(), Some("latest"));
    }

    #[test]
    fn test_urlencoding_escapes_reserved() {
        assert_eq!(urlencoding("a b&c"), "a%20b%26c");
        assert_eq!(urlencoding("plain-123_.~"), "plain-123_.~");
    }

    #[test]
    fn test_format_context_includes_fields() {
        let mut issue = Issue::new("42", "HS-7", "Login crashes", "url", "helpscout");
        issue.description = Some("steps".into());
        issue.set_metadata("status", "active");
        issue.set_metadata("customer_email", "a@b.com");
        issue.set_metadata("tags", vec!["bug".to_string()]);
        let ctx = format_helpscout_context(&issue);
        assert!(ctx.contains("HS-7"));
        assert!(ctx.contains("Login crashes"));
        assert!(ctx.contains("a@b.com"));
        assert!(ctx.contains("bug"));
        assert!(ctx.contains("steps"));
    }

    #[tokio::test]
    async fn test_add_comment_without_overrides_makes_no_get() {
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("/notes", 201, ""),
        ]);
        let source = HelpScoutSource::with_http_client(test_config(), mock);
        source.add_comment("42", "internal").await.unwrap();
        assert!(source.http.requests_to("GET", "").is_empty());
        let notes = source
            .http
            .requests_to("POST", "/v2/conversations/42/notes");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].body, r#"{"text":"internal"}"#);
    }

    #[tokio::test]
    async fn test_support_mailbox_reply_mode_is_unchanged() {
        let mut config = test_config();
        config.reply_as = ReplyAs::Reply;
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("/reply", 201, ""),
        ])
        .on(
            "GET",
            "/v2/conversations/42",
            200,
            r#"{"id":42,"mailboxId":1,"primaryCustomer":{"id":99},"_embedded":{"threads":[{"type":"customer","body":"b"}]}}"#,
        );
        let source = HelpScoutSource::with_http_client(config, mock);
        source.add_comment("42", "hello").await.unwrap();
        let replies = source
            .http
            .requests_to("POST", "/v2/conversations/42/reply");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].body, r#"{"text":"hello","customer":{"id":99}}"#);
    }

    #[tokio::test]
    async fn test_add_comment_on_security_mailbox_is_a_note() {
        let config = security_config();
        assert_eq!(config.reply_as_for(SECURITY_MAILBOX), ReplyAs::Reply);
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("/notes", 201, ""),
        ])
        .on(
            "GET",
            "/v2/conversations/42",
            200,
            &conversation_body(SECURITY_MAILBOX, Some(99)),
        );
        let source = HelpScoutSource::with_http_client(config, mock);
        source
            .add_comment("42", "Closed automatically as spam.")
            .await
            .unwrap();
        assert_eq!(source.http.requests_to("POST", "/notes").len(), 1);
        assert!(source.http.requests_to("POST", "/reply").is_empty());
    }

    #[tokio::test]
    async fn test_add_comment_uses_per_mailbox_reply_override() {
        let mut config = security_config();
        config.mailboxes.push(HelpScoutMailboxConfig {
            id: "300".to_string(),
            profile: MailboxProfile::Support,
            reply_as: Some(ReplyAs::Reply),
            trigger_tags: None,
            trigger_status: None,
        });
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("/reply", 201, ""),
        ])
        .on(
            "GET",
            "/v2/conversations/42",
            200,
            &conversation_body("300", Some(5)),
        );
        let source = HelpScoutSource::with_http_client(config, mock);
        source.add_comment("42", "hi").await.unwrap();
        let replies = source.http.requests_to("POST", "/reply");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].body, r#"{"text":"hi","customer":{"id":5}}"#);
    }

    #[tokio::test]
    async fn test_add_comment_falls_back_to_note_when_lookup_fails() {
        let mut config = security_config();
        config.mailboxes.push(HelpScoutMailboxConfig {
            id: SUPPORT_MAILBOX.to_string(),
            profile: MailboxProfile::Support,
            reply_as: Some(ReplyAs::Reply),
            trigger_tags: None,
            trigger_status: None,
        });
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("/notes", 201, ""),
        ])
        .on("GET", "/v2/conversations/42", 500, "boom");
        let source = HelpScoutSource::with_http_client(config, mock);
        source.add_comment("42", "hi").await.unwrap();
        assert_eq!(
            source.http.requests_to("GET", "/v2/conversations/42").len(),
            1
        );
        assert_eq!(source.http.requests_to("POST", "/notes").len(), 1);
        assert!(source.http.requests_to("POST", "/reply").is_empty());
    }

    #[tokio::test]
    async fn test_reply_to_reporter_posts_closed_reply_despite_top_level_note() {
        let config = security_config();
        assert_eq!(config.reply_as, ReplyAs::Note);
        let mock = MockHttpClient::new(vec![("/v2/oauth2/token", 200, TOKEN_BODY)])
            .on(
                "GET",
                "/v2/conversations/42",
                200,
                &conversation_body(SECURITY_MAILBOX, Some(99)),
            )
            .on("POST", "/v2/conversations/42/reply", 201, "");
        let source = HelpScoutSource::with_http_client(config, mock);
        let outcome = source
            .reply_to_reporter("42", "Hi,\n\nThanks.", Some(ConversationStatus::Closed))
            .await;
        assert_eq!(outcome, ReporterReply::Sent);
        let replies = source
            .http
            .requests_to("POST", "/v2/conversations/42/reply");
        assert_eq!(replies.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&replies[0].body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"text": "Hi,<br><br>Thanks.", "customer": {"id": 99}, "status": "closed"})
        );
        assert!(source.http.requests_to("POST", "/notes").is_empty());
    }

    #[tokio::test]
    async fn test_reply_to_reporter_maps_statuses_and_omits_none() {
        for (status, expected) in [
            (Some(ConversationStatus::Active), Some("active")),
            (Some(ConversationStatus::Pending), Some("pending")),
            (None, None),
        ] {
            let mock = MockHttpClient::new(vec![("/v2/oauth2/token", 200, TOKEN_BODY)])
                .on(
                    "GET",
                    "/v2/conversations/42",
                    200,
                    &conversation_body(SECURITY_MAILBOX, Some(99)),
                )
                .on("POST", "/reply", 201, "");
            let source = HelpScoutSource::with_http_client(security_config(), mock);
            assert_eq!(
                source.reply_to_reporter("42", "x", status).await,
                ReporterReply::Sent
            );
            let body: serde_json::Value =
                serde_json::from_str(&source.http.requests_to("POST", "/reply")[0].body).unwrap();
            assert_eq!(body.get("status").and_then(|v| v.as_str()), expected);
        }
    }

    async fn reply_outcome_for(stub: MockHttpClient) -> (ReporterReply, Vec<Recorded>) {
        let mock = stub.on(
            "GET",
            "/v2/conversations/42",
            200,
            &conversation_body(SECURITY_MAILBOX, Some(99)),
        );
        let source = HelpScoutSource::with_http_client(security_config(), mock);
        let outcome = source
            .reply_to_reporter("42", "x", Some(ConversationStatus::Closed))
            .await;
        (outcome, source.http.requests_to("POST", "/reply"))
    }

    #[tokio::test]
    async fn test_reply_to_reporter_classifies_responses() {
        let (outcome, _) = reply_outcome_for(
            MockHttpClient::new(vec![("/v2/oauth2/token", 200, TOKEN_BODY)]).on(
                "POST",
                "/reply",
                422,
                r#"{"message":"invalid"}"#,
            ),
        )
        .await;
        assert!(
            matches!(&outcome, ReporterReply::Rejected(reason) if reason.contains("422")),
            "{outcome:?}"
        );

        let (outcome, _) = reply_outcome_for(
            MockHttpClient::new(vec![("/v2/oauth2/token", 200, TOKEN_BODY)]).on(
                "POST",
                "/reply",
                503,
                "unavailable",
            ),
        )
        .await;
        assert!(
            matches!(&outcome, ReporterReply::Unconfirmed(reason) if reason.contains("503")),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn test_reply_to_reporter_transport_error_is_unconfirmed() {
        let (outcome, requests) = reply_outcome_for(
            MockHttpClient::new(vec![("/v2/oauth2/token", 200, TOKEN_BODY)]).failing(
                "POST",
                "/reply",
                transport_error_after_connect().await,
            ),
        )
        .await;
        assert_eq!(requests.len(), 1);
        assert!(
            matches!(outcome, ReporterReply::Unconfirmed(_)),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn test_reply_to_reporter_connect_error_is_rejected() {
        let (outcome, _) = reply_outcome_for(
            MockHttpClient::new(vec![("/v2/oauth2/token", 200, TOKEN_BODY)]).failing(
                "POST",
                "/reply",
                connect_error().await,
            ),
        )
        .await;
        assert!(matches!(outcome, ReporterReply::Rejected(_)), "{outcome:?}");
    }

    #[tokio::test]
    async fn test_reply_to_reporter_rejects_before_posting() {
        let cases = [
            (
                "42",
                conversation_body(SECURITY_MAILBOX, None),
                "customer id",
            ),
            (
                "42",
                conversation_body(SUPPORT_MAILBOX, Some(99)),
                "not a security mailbox",
            ),
            (
                "42",
                r#"{"id":42,"primaryCustomer":{"id":99}}"#.to_string(),
                "mailbox id",
            ),
            (
                "42/../1",
                conversation_body(SECURITY_MAILBOX, Some(99)),
                "invalid",
            ),
        ];
        for (issue_id, conversation, expected) in cases {
            let mock = MockHttpClient::new(vec![
                ("/v2/oauth2/token", 200, TOKEN_BODY),
                ("/reply", 201, ""),
            ])
            .on("GET", "/v2/conversations/42", 200, &conversation);
            let source = HelpScoutSource::with_http_client(security_config(), mock);
            let outcome = source.reply_to_reporter(issue_id, "x", None).await;
            assert!(
                matches!(&outcome, ReporterReply::Rejected(reason) if reason.contains(expected)),
                "{issue_id} {expected}: {outcome:?}"
            );
            assert!(source.http.requests_to("POST", "/reply").is_empty());
        }

        let mock = MockHttpClient::new(vec![("/v2/oauth2/token", 200, TOKEN_BODY)]).on(
            "GET",
            "/v2/conversations/42",
            503,
            "down",
        );
        let source = HelpScoutSource::with_http_client(security_config(), mock);
        assert!(matches!(
            source.reply_to_reporter("42", "x", None).await,
            ReporterReply::Rejected(_)
        ));

        let mock = MockHttpClient::new(vec![("/v2/oauth2/token", 401, "nope")]);
        let source = HelpScoutSource::with_http_client(security_config(), mock);
        assert!(matches!(
            source.reply_to_reporter("42", "x", None).await,
            ReporterReply::Rejected(_)
        ));
        assert!(source.http.requests_to("POST", "/reply").is_empty());
    }

    #[tokio::test]
    async fn test_reply_to_reporter_rejects_a_support_mailbox_that_sends_replies() {
        let mut config = security_config();
        config.reply_as = ReplyAs::Reply;
        config.mailboxes.push(HelpScoutMailboxConfig {
            id: SUPPORT_MAILBOX.to_string(),
            profile: MailboxProfile::Support,
            reply_as: Some(ReplyAs::Reply),
            trigger_tags: None,
            trigger_status: None,
        });
        assert_eq!(config.reply_as_for(SUPPORT_MAILBOX), ReplyAs::Reply);
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("/reply", 201, ""),
        ])
        .on(
            "GET",
            "/v2/conversations/42",
            200,
            &conversation_body(SUPPORT_MAILBOX, Some(99)),
        );
        let source = HelpScoutSource::with_http_client(config, mock);

        let outcome = source.reply_to_reporter("42", "x", None).await;

        assert!(
            matches!(&outcome, ReporterReply::Rejected(reason) if reason.contains("not a security mailbox")),
            "{outcome:?}"
        );
        assert!(source.http.requests_to("POST", "/reply").is_empty());
    }

    #[tokio::test]
    async fn test_reply_to_reporter_escapes_markup_and_keeps_line_breaks() {
        let mock = MockHttpClient::new(vec![("/v2/oauth2/token", 200, TOKEN_BODY)])
            .on(
                "GET",
                "/v2/conversations/42",
                200,
                &conversation_body(SECURITY_MAILBOX, Some(99)),
            )
            .on("POST", "/reply", 201, "");
        let source = HelpScoutSource::with_http_client(security_config(), mock);

        let outcome = source
            .reply_to_reporter(
                "42",
                "Hi,\r\n\nThe <img src=x onerror=alert(1)> & \"token\" isn't valid.",
                None,
            )
            .await;

        assert_eq!(outcome, ReporterReply::Sent);
        let body: serde_json::Value =
            serde_json::from_str(&source.http.requests_to("POST", "/reply")[0].body).unwrap();
        assert_eq!(
            body["text"],
            "Hi,<br><br>The &lt;img src=x onerror=alert(1)&gt; &amp; &quot;token&quot; isn't valid."
        );
    }

    #[tokio::test]
    async fn test_reply_to_reporter_rejected_for_support_note_mailbox() {
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("/reply", 201, ""),
        ])
        .on(
            "GET",
            "/v2/conversations/42",
            200,
            &conversation_body("1", Some(9)),
        );
        let source = HelpScoutSource::with_http_client(test_config(), mock);
        assert!(matches!(
            source.reply_to_reporter("42", "x", None).await,
            ReporterReply::Rejected(_)
        ));
        assert!(source.http.requests_to("POST", "/reply").is_empty());
    }

    #[tokio::test]
    async fn test_mark_spam_patches_status() {
        let mock = MockHttpClient::new(vec![("/v2/oauth2/token", 200, TOKEN_BODY)]).on(
            "PATCH",
            "/v2/conversations/42",
            204,
            "",
        );
        let source = HelpScoutSource::with_http_client(security_config(), mock);
        source.mark_spam("42").await.unwrap();
        let patches = source.http.requests_to("PATCH", "/v2/conversations/42");
        assert_eq!(patches.len(), 1);
        assert_status_patch(&patches[0].body, "spam");

        let mock = MockHttpClient::new(vec![("/v2/oauth2/token", 200, TOKEN_BODY)]).on(
            "PATCH",
            "/v2/conversations/42",
            400,
            "bad status",
        );
        let source = HelpScoutSource::with_http_client(security_config(), mock);
        assert!(source.mark_spam("42").await.is_err());
        assert!(source.mark_spam("4 2").await.is_err());
    }

    /// HelpScout documents the update body as a single JSON Patch object and
    /// rejects an array, so the patch must parse as exactly that object.
    fn assert_status_patch(body: &str, status: &str) {
        let patch: serde_json::Value = serde_json::from_str(body).expect("patch body is JSON");
        assert_eq!(
            patch,
            serde_json::json!({ "op": "replace", "path": "/status", "value": status }),
            "the status patch must be one JSON object, not an array: {body}"
        );
    }

    #[tokio::test]
    async fn test_resolve_issue_patches_status_as_one_object() {
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("/v2/conversations/42", 204, ""),
        ]);
        let source = HelpScoutSource::with_http_client(test_config(), mock);
        source.resolve_issue("42").await.unwrap();
        assert_status_patch(
            &source.http.requests_to("PATCH", "/v2/conversations/42")[0].body,
            "closed",
        );
    }

    #[tokio::test]
    async fn test_fetch_issues_uses_per_mailbox_status() {
        let mut config = security_config();
        config.trigger_status = "pending".to_string();
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            (
                "/v2/conversations?",
                200,
                r#"{"_embedded":{"conversations":[]}}"#,
            ),
        ]);
        let source = HelpScoutSource::with_http_client(config, mock);
        source.fetch_issues().await.unwrap();
        let urls: Vec<String> = source
            .http
            .requests_to("GET", "/v2/conversations?")
            .into_iter()
            .map(|request| request.url)
            .collect();
        assert_eq!(urls.len(), 2);
        assert!(
            urls[0].contains("mailbox=249877&status=pending"),
            "{urls:?}"
        );
        assert!(urls[1].contains("mailbox=249891&status=active"), "{urls:?}");
    }

    #[tokio::test]
    async fn test_fetch_issues_skips_security_mailbox_while_inbox_disabled() {
        let mut config = security_config();
        config.security.enabled = false;
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            (
                "/v2/conversations?",
                200,
                r#"{"_embedded":{"conversations":[]}}"#,
            ),
        ]);
        let source = HelpScoutSource::with_http_client(config, mock);
        source.fetch_issues().await.unwrap();
        let lists = source.http.requests_to("GET", "/v2/conversations?");
        assert_eq!(lists.len(), 1);
        assert!(lists[0].url.contains("mailbox=249877"));
    }

    #[tokio::test]
    async fn test_list_conversations_excludes_security_mailbox() {
        let support = r#"{"_embedded":{"conversations":[{"id":1,"mailboxId":249877}]}}"#;
        let leaked = r#"{"_embedded":{"conversations":[{"id":2,"mailboxId":249891},{"id":3,"mailboxId":249877}]}}"#;
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("mailbox=249877", 200, leaked),
            ("mailbox=249891", 200, support),
        ]);
        let source = HelpScoutSource::with_http_client(security_config(), mock);
        let issues = source.list_conversations(None).await.unwrap();
        let ids: Vec<&str> = issues.iter().map(|issue| issue.id.as_str()).collect();
        assert_eq!(ids, vec!["3"]);
        assert!(source.http.requests_to("GET", "mailbox=249891").is_empty());

        let issues = source.list_conversations(Some("all")).await.unwrap();
        assert_eq!(issues.len(), 1);
        assert!(source.http.requests_to("GET", "mailbox=249891").is_empty());
    }

    #[test]
    fn test_matches_criteria_per_mailbox_tags_and_fail_closed() {
        let source =
            HelpScoutSource::with_http_client(security_config(), MockHttpClient::new(vec![]));
        let mut security = Issue::new("1", "HS-1", "t", "u", "helpscout");
        security.set_metadata("mailbox_id", SECURITY_MAILBOX);
        assert!(source.matches_criteria(&security).matches);

        let mut support = Issue::new("2", "HS-2", "t", "u", "helpscout");
        support.set_metadata("mailbox_id", SUPPORT_MAILBOX);
        support.set_metadata("tags", vec!["question".to_string()]);
        assert!(!source.matches_criteria(&support).matches);
        support.set_metadata("tags", vec!["Claude".to_string()]);
        assert!(source.matches_criteria(&support).matches);

        let mut unknown = Issue::new("3", "HS-3", "t", "u", "helpscout");
        unknown.set_metadata("tags", vec!["claude".to_string()]);
        assert!(!source.matches_criteria(&unknown).matches);
    }

    const SECURITY_CONVERSATION: &str = r#"{"_embedded":{"conversations":[
        {"id":42,"number":7,"subject":"IDOR","status":"active","mailboxId":249891,
         "preview":"preview text","createdAt":"2026-11-05T10:00:00Z",
         "primaryCustomer":{"id":99,"email":"r@x.io"},
         "_embedded":{"threads":[
            {"type":"customer","createdAt":"2026-11-05T12:00:00Z",
             "body":"<p>Follow&nbsp;up &amp; <b>more</b></p>",
             "_embedded":{"attachments":[{"id":2,"filename":"poc.mp4","mimeType":"video/mp4"}]}},
            {"type":"note","createdAt":"2026-11-05T11:00:00Z","body":"internal only"},
            {"type":"message","createdAt":"2026-11-05T10:30:00Z","body":"agent reply"},
            {"type":"customer","createdAt":"2026-11-05T10:00:00Z",
             "body":"<div>Steps:</div><ol><li>log in</li><li>call &lt;api&gt;</li></ol><script>alert(1)</script><style>p{}</style>",
             "_embedded":{"attachments":[{"id":1,"filename":"request.txt","mimeType":"text/plain"}]}}
         ]}}
    ]}}"#;

    #[tokio::test]
    async fn test_security_conversation_maps_all_customer_threads() {
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("mailbox=249891", 200, SECURITY_CONVERSATION),
            (
                "mailbox=249877",
                200,
                r#"{"_embedded":{"conversations":[]}}"#,
            ),
        ]);
        let source = HelpScoutSource::with_http_client(security_config(), mock);
        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 1);
        let issue = &issues[0];
        assert_eq!(
            issue.description.as_deref(),
            Some("Steps:\nlog in\ncall <api>\n\n---\n\nFollow up & more")
        );
        assert_eq!(
            issue
                .get_metadata::<String>(MAILBOX_PROFILE_METADATA_KEY)
                .as_deref(),
            Some("security")
        );
        assert_eq!(
            issue.get_metadata::<Vec<String>>(ATTACHMENTS_METADATA_KEY),
            Some(vec!["request.txt".to_string(), "poc.mp4".to_string()])
        );
        assert_eq!(
            issue.get_metadata::<u32>(CUSTOMER_THREAD_COUNT_METADATA_KEY),
            Some(2)
        );
        assert_eq!(
            issue.get_metadata::<String>("customer_id").as_deref(),
            Some("99")
        );
    }

    #[tokio::test]
    async fn test_support_conversation_description_is_unchanged() {
        let support = SECURITY_CONVERSATION.replace("249891", "249877");
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("mailbox=249877", 200, &support),
            (
                "mailbox=249891",
                200,
                r#"{"_embedded":{"conversations":[]}}"#,
            ),
        ]);
        let source = HelpScoutSource::with_http_client(security_config(), mock);
        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 1);
        let issue = &issues[0];
        assert!(issue
            .description
            .as_deref()
            .is_some_and(|description| description.starts_with("<div>Steps:</div>")));
        assert_eq!(
            issue
                .get_metadata::<String>(MAILBOX_PROFILE_METADATA_KEY)
                .as_deref(),
            Some("support")
        );
    }

    #[tokio::test]
    async fn test_polled_mailbox_fills_a_missing_mailbox_id() {
        let list = r#"{"_embedded":{"conversations":[{"id":5,"preview":"<b>hi</b>"}]}}"#;
        let mock = MockHttpClient::new(vec![
            ("/v2/oauth2/token", 200, TOKEN_BODY),
            ("mailbox=249891", 200, list),
            (
                "mailbox=249877",
                200,
                r#"{"_embedded":{"conversations":[]}}"#,
            ),
        ]);
        let source = HelpScoutSource::with_http_client(security_config(), mock);
        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(
            issues[0].get_metadata::<String>("mailbox_id").as_deref(),
            Some(SECURITY_MAILBOX)
        );
        assert_eq!(issues[0].description.as_deref(), Some("hi"));
        assert_eq!(
            issues[0].get_metadata::<u32>(CUSTOMER_THREAD_COUNT_METADATA_KEY),
            Some(0)
        );
    }

    #[test]
    fn test_customer_threads_fall_back_to_reverse_api_order() {
        let threads = vec![
            HsThread {
                thread_type: Some("customer".into()),
                body: Some("newest".into()),
                ..Default::default()
            },
            HsThread {
                thread_type: Some("customer".into()),
                body: Some("oldest".into()),
                created_at: Some("2026-01-01T00:00:00Z".into()),
                ..Default::default()
            },
        ];
        let ordered: Vec<&str> = customer_threads_oldest_first(&threads)
            .iter()
            .filter_map(|thread| thread.body.as_deref())
            .collect();
        assert_eq!(ordered, vec!["oldest", "newest"]);
    }

    #[test]
    fn test_security_description_is_capped() {
        let body = "a".repeat(SECURITY_DESCRIPTION_MAX_CHARS);
        let threads = [
            HsThread {
                thread_type: Some("customer".into()),
                body: Some(body.clone()),
                ..Default::default()
            },
            HsThread {
                thread_type: Some("customer".into()),
                body: Some("é".repeat(10)),
                ..Default::default()
            },
        ];
        let references: Vec<&HsThread> = threads.iter().collect();
        let description = security_description(&references).unwrap();
        assert_eq!(description.chars().count(), SECURITY_DESCRIPTION_MAX_CHARS);
    }

    #[test]
    fn test_html_to_text() {
        assert_eq!(html_to_text("plain"), "plain");
        assert_eq!(html_to_text("a<br>b<br/>c"), "a\nb\nc");
        assert_eq!(html_to_text("1 < 2 and 3 > 2"), "1 < 2 and 3 > 2");
        assert_eq!(html_to_text("<p>x</p><!-- hidden --><p>y</p>"), "x\n\ny");
        assert_eq!(
            html_to_text("<SCRIPT type=x>steal()</SCRIPT>ok<style>.a{}</style>"),
            "ok"
        );
        assert_eq!(
            html_to_text("&#65;&#x42;&quot;&#39;&bogus;&"),
            "AB\"'&bogus;&"
        );
        assert_eq!(html_to_text("<p>  </p><p></p><p>z</p>"), "z");
        assert_eq!(html_to_text("unterminated <a href"), "unterminated");
    }

    #[test]
    fn test_classify_reply_failure_treats_unknown_failures_as_unconfirmed() {
        assert!(matches!(
            classify_reply_failure(&abnegate_http::Error::InvalidUrl),
            ReporterReply::Rejected(_)
        ));
        assert!(matches!(
            classify_reply_failure(&abnegate_http::Error::TooManyRedirects),
            ReporterReply::Unconfirmed(_)
        ));
        assert!(matches!(
            classify_reply_failure(&abnegate_http::Error::oversized_body(10)),
            ReporterReply::Unconfirmed(_)
        ));
    }
}
