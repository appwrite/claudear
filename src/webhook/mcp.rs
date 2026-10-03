//! Built-in MCP (Model Context Protocol) search server.
//!
//! Exposes the existing semantic search services - code search and the Discord
//! knowledgebase - as MCP tools over the Streamable HTTP transport, mounted at
//! `POST /mcp` on the same HTTP server as the webhooks and dashboard.
//!
//! This is a minimal, dependency-free implementation of the subset of the MCP
//! JSON-RPC surface that a tool-only server needs: `initialize`, `tools/list`,
//! `tools/call`, and `ping`. Notifications (requests without an `id`, e.g.
//! `notifications/initialized`) are accepted and acknowledged with `202`.
//!
//! Only tools backed by a real, query-string search index are exposed. Today
//! that means code and Discord; other sources (HelpScout, Sentry, Linear) are
//! ingested as issues but have no clean query-search primitive, so they are not
//! surfaced here. Adding a new tool is a matter of extending `tool_definitions`
//! and `call_tool`.

use super::cf_access::CfAccessVerifier;
use super::server::AppState;
use crate::config::McpSearchServerConfig;
use crate::knowledgebase::DiscordSearchService;
use crate::repo::code_index::CodeSearchService;
use crate::source::IssueSource;
use crate::storage::FixAttemptTracker;
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
};
use claudear_integrations::discord::{DiscordChannel, DiscordClient, DiscordMessage};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

/// Header Cloudflare Access sets with its signed identity JWT.
const CF_ACCESS_HEADER: &str = "cf-access-jwt-assertion";

/// The slice of the Discord bot client the list tool needs. A trait so the
/// request path can be tested with a fake, without a live bot token.
#[async_trait::async_trait]
pub(crate) trait DiscordMessageLister: Send + Sync {
    async fn list_channel_messages(
        &self,
        channel_id: &str,
        limit: usize,
    ) -> crate::error::Result<Vec<DiscordMessage>>;
    async fn list_guild_channels(
        &self,
        guild_id: &str,
    ) -> crate::error::Result<Vec<DiscordChannel>>;
}

#[async_trait::async_trait]
impl DiscordMessageLister for DiscordClient {
    async fn list_channel_messages(
        &self,
        channel_id: &str,
        limit: usize,
    ) -> crate::error::Result<Vec<DiscordMessage>> {
        DiscordClient::list_channel_messages(self, channel_id, limit).await
    }
    async fn list_guild_channels(
        &self,
        guild_id: &str,
    ) -> crate::error::Result<Vec<DiscordChannel>> {
        DiscordClient::list_guild_channels(self, guild_id).await
    }
}

/// Hex-encoded SHA-256 of a personal access token, matching what storage holds.
pub(crate) fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Protocol versions this handler implements. The newest (first) is advertised
/// when a client requests a version we do not recognise.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_PROTOCOL_VERSION: &str = SUPPORTED_PROTOCOL_VERSIONS[0];

/// Everything an MCP request needs from the server, borrowed from `AppState`.
///
/// Kept separate from `AppState` so the whole request path can be exercised in
/// tests without constructing the full webhook server state.
struct McpContext<'a> {
    cfg: &'a McpSearchServerConfig,
    code_search: Option<&'a CodeSearchService>,
    discord_search: Option<&'a DiscordSearchService>,
    tracker: &'a dyn FixAttemptTracker,
    cf_verifier: Option<&'a CfAccessVerifier>,
    /// HelpScout source for live conversation listing (as an `IssueSource`).
    helpscout: Option<&'a dyn IssueSource>,
    /// Discord bot client for live message listing.
    discord_lister: Option<&'a dyn DiscordMessageLister>,
    /// Guild id used to resolve channel names to ids for Discord listing.
    discord_guild_id: Option<&'a str>,
}

/// Outcome of handling a POST: either a bare 202 ack (for notifications) or a
/// JSON-RPC body to return with 200.
enum PostOutcome {
    Accepted,
    Json(Value),
}

/// Reject `GET /mcp`: this server does not offer the optional server-initiated
/// SSE stream, only request/response over POST.
pub(crate) async fn mcp_get_handler() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {
                "code": -32000,
                "message": "This MCP endpoint only supports POST (no SSE stream)."
            }
        })),
    )
        .into_response()
}

/// Handle a single JSON-RPC request posted to `/mcp`.
pub(crate) async fn mcp_post_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let ctx = McpContext {
        cfg: &state.config.mcp_server,
        code_search: state.code_search_service.as_deref(),
        discord_search: state.discord_search_service.as_deref(),
        tracker: state.tracker.as_ref(),
        cf_verifier: state.mcp_cf_verifier.as_deref(),
        helpscout: state.helpscout_source.as_deref(),
        discord_lister: state.discord_lister.as_deref(),
        discord_guild_id: state.discord_guild_id.as_deref(),
    };
    handle_request(&ctx, &headers, &body).await
}

/// Enforce the request gate (Origin, Cloudflare Access, bearer token) and then
/// dispatch. Independent of axum extractors so the full HTTP path — including
/// authentication outcomes — can be exercised in tests.
async fn handle_request(ctx: &McpContext<'_>, headers: &HeaderMap, body: &[u8]) -> Response {
    // DNS-rebinding guard: a browser always sends `Origin`, so reject any origin
    // not explicitly allow-listed. CLI / server-to-server clients send none and
    // are allowed through.
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if !origin_allowed(&ctx.cfg.allowed_origins, origin) {
        return forbidden("Origin not allowed");
    }

    // Enforce that the request transited Cloudflare Access (e.g. WARP) when
    // required, before any token/tool work.
    if ctx.cfg.require_cloudflare_access {
        if let Err(resp) = enforce_cloudflare_access(headers, ctx.cf_verifier).await {
            return resp;
        }
    }

    // Require a per-user personal access token when configured, and identify the
    // caller for attribution.
    if ctx.cfg.require_auth {
        match authenticate_bearer(headers, ctx.tracker) {
            Ok(user) => {
                tracing::info!(
                    component = "mcp",
                    user = %user.email,
                    "Authenticated MCP request"
                );
            }
            Err(resp) => return resp,
        }
    }

    match process_post(ctx, body).await {
        PostOutcome::Accepted => StatusCode::ACCEPTED.into_response(),
        PostOutcome::Json(value) => Json(value).into_response(),
    }
}

/// Core request processing, independent of axum extractors so it can be tested
/// directly against its JSON output.
async fn process_post(ctx: &McpContext<'_>, body: &[u8]) -> PostOutcome {
    let request: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(e) => {
            return PostOutcome::Json(rpc_error(Value::Null, -32700, format!("Parse error: {e}")));
        }
    };

    // JSON-RPC batching was removed in the 2025-06-18 spec; we only accept a
    // single request object and reject arrays explicitly.
    if request.is_array() {
        return PostOutcome::Json(rpc_error(
            Value::Null,
            -32600,
            "Batch requests are not supported".to_string(),
        ));
    }

    // The payload must be a JSON-RPC 2.0 request object. Anything else (null,
    // numbers, bare objects without a valid envelope) is an Invalid Request.
    let Some(obj) = request.as_object() else {
        return PostOutcome::Json(rpc_error(
            Value::Null,
            -32600,
            "Invalid Request: expected a JSON-RPC object".to_string(),
        ));
    };

    // An `id`, when present, must be a string, number, or null — never an
    // object/array. Its absence marks a notification. Extract it up front so we
    // can echo a valid id back even in envelope errors.
    let id_present = obj.contains_key("id");
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    let id_valid = matches!(id, Value::String(_) | Value::Number(_) | Value::Null);
    let reply_id = if id_valid { id.clone() } else { Value::Null };

    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return PostOutcome::Json(rpc_error(
            reply_id,
            -32600,
            "Invalid Request: jsonrpc must be \"2.0\"".to_string(),
        ));
    }

    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return PostOutcome::Json(rpc_error(
            reply_id,
            -32600,
            "Invalid Request: missing method".to_string(),
        ));
    };

    if id_present && !id_valid {
        return PostOutcome::Json(rpc_error(
            Value::Null,
            -32600,
            "Invalid Request: id must be a string, number, or null".to_string(),
        ));
    }

    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    // A valid request without an `id` member is a notification: acknowledge with
    // no JSON-RPC body.
    if !id_present {
        return PostOutcome::Accepted;
    }
    let id = reply_id;

    let response = match method {
        "initialize" => rpc_result(id, initialize_result(&params)),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tool_definitions(ctx.cfg) })),
        "tools/call" => match call_tool(ctx, &params).await {
            Ok(result) => rpc_result(id, result),
            Err(ToolError::Protocol { code, message }) => rpc_error(id, code, message),
        },
        other => rpc_error(id, -32601, format!("Method not found: {other}")),
    };

    PostOutcome::Json(response)
}

/// Whether a request's `Origin` is acceptable. No origin (non-browser client)
/// is always allowed; a present origin must be in the allow-list.
fn origin_allowed(allowed: &[String], origin: Option<&str>) -> bool {
    match origin {
        None => true,
        Some(origin) => allowed.iter().any(|a| a == origin),
    }
}

/// Build a `401 Unauthorized` JSON-RPC response advertising Bearer auth.
fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(rpc_error(Value::Null, -32001, message.to_string())),
    )
        .into_response()
}

/// Build a `403 Forbidden` JSON-RPC response.
fn forbidden(message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(rpc_error(Value::Null, -32001, message.to_string())),
    )
        .into_response()
}

/// Extract the `Authorization: Bearer <token>` value, if present.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Resolve the per-user token on the request to its owner, or return a ready
/// `401` response.
// The error is a full axum Response, which is large by nature; that is fine for
// a per-request auth gate.
#[allow(clippy::result_large_err)]
fn authenticate_bearer(
    headers: &HeaderMap,
    tracker: &dyn FixAttemptTracker,
) -> Result<claudear_storage::UserRow, Response> {
    let Some(token) = bearer_token(headers) else {
        return Err(unauthorized(
            "Missing bearer token. Create one in the portal and send it as Authorization: Bearer <token>.",
        ));
    };
    match tracker.get_user_by_api_token_hash(&hash_token(token)) {
        Ok(Some(user)) => Ok(user),
        Ok(None) => Err(unauthorized("Invalid or expired token")),
        Err(e) => {
            tracing::error!(component = "mcp", error = %e, "Token lookup failed");
            Err(unauthorized("Token verification failed"))
        }
    }
}

/// Verify the Cloudflare Access JWT on the request, or return a ready `403`.
#[allow(clippy::result_large_err)]
async fn enforce_cloudflare_access(
    headers: &HeaderMap,
    verifier: Option<&CfAccessVerifier>,
) -> Result<(), Response> {
    let Some(verifier) = verifier else {
        return Err(forbidden(
            "Cloudflare Access is required but not configured on the server",
        ));
    };
    let Some(jwt) = headers.get(CF_ACCESS_HEADER).and_then(|v| v.to_str().ok()) else {
        return Err(forbidden(
            "Missing Cloudflare Access assertion; request must transit Cloudflare Access",
        ));
    };
    match verifier.verify(jwt).await {
        Ok(claims) => {
            tracing::debug!(
                component = "mcp",
                principal = %claims.principal(),
                "Cloudflare Access verified"
            );
            Ok(())
        }
        Err(e) => {
            tracing::warn!(component = "mcp", error = %e, "Cloudflare Access verification failed");
            Err(forbidden("Invalid Cloudflare Access assertion"))
        }
    }
}

/// Resolve the protocol version to advertise: echo the client's request when we
/// support it, otherwise fall back to our newest supported version so the client
/// can decide whether to proceed.
fn negotiate_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|v| {
            SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .find(|s| *s == v)
        })
        .unwrap_or(DEFAULT_PROTOCOL_VERSION)
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty());

    json!({
        "protocolVersion": negotiate_version(requested),
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "claudear-search",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// Build the list of exposed tools based on configuration.
fn tool_definitions(cfg: &McpSearchServerConfig) -> Vec<Value> {
    let mut tools = Vec::new();

    if cfg.expose_code {
        tools.push(json!({
            "name": "code_search",
            "description": "Semantic search over indexed source code. Returns the \
                most relevant code chunks for a natural-language or code query, \
                each with its file path, line range and similarity score.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language or code query." },
                    "repo": { "type": "string", "description": "Optional repository name (e.g. 'org/repo') to scope the search." },
                    "limit": { "type": "integer", "description": "Max results to return.", "minimum": 1 }
                },
                "required": ["query"]
            }
        }));

        tools.push(json!({
            "name": "find_symbol",
            "description": "Find code symbols (functions, classes, structs, traits, \
                etc.) by name via exact substring match. Returns each match's \
                kind, file, line range and signature.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Symbol name or substring to match." },
                    "kind": {
                        "type": "string",
                        "description": "Optional symbol kind filter.",
                        "enum": ["function", "class", "method", "struct", "impl", "interface", "trait", "enum", "module", "constant"]
                    },
                    "repo": { "type": "string", "description": "Optional repository name to scope the search." },
                    "limit": { "type": "integer", "description": "Max results to return.", "minimum": 1 }
                },
                "required": ["name"]
            }
        }));
    }

    if cfg.expose_discord {
        tools.push(json!({
            "name": "discord_search",
            "description": "Semantic search over the indexed Discord knowledgebase. \
                Returns the most relevant past conversations with participants, \
                timestamps and jump links.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language query." },
                    "channel_id": { "type": "string", "description": "Optional Discord channel id to scope the search." },
                    "channel": { "type": "string", "description": "Optional channel name to scope the search (resolved against indexed channels; matched loosely ignoring case/emoji/punctuation)." },
                    "limit": { "type": "integer", "description": "Max results to return.", "minimum": 1 }
                },
                "required": ["query"]
            }
        }));

        tools.push(json!({
            "name": "discord_list_messages",
            "description": "List the most recent messages in a Discord channel (live, \
                not semantic). Identify the channel by `channel_id`, or by `channel` \
                name which is resolved within the configured guild.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel_id": { "type": "string", "description": "Discord channel id." },
                    "channel": { "type": "string", "description": "Channel name (resolved to an id within the guild)." },
                    "limit": { "type": "integer", "description": "Max messages to return (newest first).", "minimum": 1 }
                },
                "required": []
            }
        }));
    }

    if cfg.expose_helpscout {
        tools.push(json!({
            "name": "helpscout_list_conversations",
            "description": "List current HelpScout conversations (live) from the \
                configured mailboxes, newest first, with status, customer and link.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": { "type": "string", "description": "Filter by conversation status; 'all' for every status. Defaults to the configured trigger status.", "enum": ["active", "pending", "closed", "spam", "open", "all"] },
                    "limit": { "type": "integer", "description": "Max conversations to return.", "minimum": 1 }
                },
                "required": []
            }
        }));
    }

    tools
}

/// A JSON-RPC protocol-level error. Tool *execution* failures are not protocol
/// errors - they are returned as a successful result with `isError: true`.
enum ToolError {
    Protocol { code: i64, message: String },
}

async fn call_tool(ctx: &McpContext<'_>, params: &Value) -> Result<Value, ToolError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::Protocol {
            code: -32602,
            message: "Missing tool name".to_string(),
        })?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    match name {
        "code_search" if ctx.cfg.expose_code => Ok(code_search(ctx, &args).await),
        "find_symbol" if ctx.cfg.expose_code => Ok(find_symbol(ctx, &args)),
        "discord_search" if ctx.cfg.expose_discord => Ok(discord_search(ctx, &args).await),
        "discord_list_messages" if ctx.cfg.expose_discord => {
            Ok(discord_list_messages(ctx, &args).await)
        }
        "helpscout_list_conversations" if ctx.cfg.expose_helpscout => {
            Ok(helpscout_list_conversations(ctx, &args).await)
        }
        other => Err(ToolError::Protocol {
            code: -32602,
            message: format!("Unknown tool: {other}"),
        }),
    }
}

async fn code_search(ctx: &McpContext<'_>, args: &Value) -> Value {
    let Some(service) = ctx.code_search else {
        return tool_error(
            "Code search is unavailable (code indexing or embeddings not configured).",
        );
    };
    let Some(query) = str_arg(args, "query") else {
        return tool_error("Missing required argument: query");
    };

    let repo_id = match resolve_repo(ctx, args) {
        Ok(id) => id,
        Err(msg) => return tool_error(&msg),
    };
    let limit = ctx.cfg.resolve_limit(usize_arg(args, "limit"));

    match service.search(query, repo_id, limit).await {
        Ok(results) => tool_text(format_code_results(&results, &repo_name_map(ctx.tracker))),
        Err(e) => tool_error(&format!("Code search failed: {e}")),
    }
}

fn find_symbol(ctx: &McpContext<'_>, args: &Value) -> Value {
    let Some(service) = ctx.code_search else {
        return tool_error("Symbol search is unavailable (code indexing not configured).");
    };
    let Some(name) = str_arg(args, "name") else {
        return tool_error("Missing required argument: name");
    };

    let kind = match str_arg(args, "kind") {
        Some(k) => match claudear_core::types::SymbolKind::from_str_loose(k) {
            Some(kind) => Some(kind),
            None => return tool_error(&format!("Unknown symbol kind: {k}")),
        },
        None => None,
    };
    let repo_id = match resolve_repo(ctx, args) {
        Ok(id) => id,
        Err(msg) => return tool_error(&msg),
    };

    let requested = usize_arg(args, "limit");
    match service.find_symbol(name, kind, repo_id) {
        Ok(mut symbols) => {
            // The storage layer caps symbol lookups generously (up to 100); apply
            // the configured limit here so this tool is bounded like the others.
            let total = symbols.len();
            symbols.truncate(ctx.cfg.resolve_limit(requested));
            let shown = symbols.len();
            let mut out = format_symbols(&symbols, &repo_name_map(ctx.tracker));
            out.push_str(&cap_note(total, shown, requested, ctx.cfg.max_limit));
            tool_text(out)
        }
        Err(e) => tool_error(&format!("Symbol search failed: {e}")),
    }
}

async fn discord_search(ctx: &McpContext<'_>, args: &Value) -> Value {
    let Some(service) = ctx.discord_search else {
        return tool_error("Discord search is unavailable (Discord knowledgebase not configured).");
    };
    let Some(query) = str_arg(args, "query") else {
        return tool_error("Missing required argument: query");
    };

    // Scope by channel_id (preferred) or channel name (resolved against the
    // indexed-channel registry, loosely matched). A present-but-invalid value is
    // an error rather than a silent all-channel search.
    let channel_id = match resolve_search_channel(ctx, args) {
        Ok(c) => c,
        Err(msg) => return tool_error(&msg),
    };
    let limit = ctx.cfg.resolve_limit(usize_arg(args, "limit"));

    match service.search(query, channel_id.as_deref(), limit).await {
        Ok(results) if results.is_empty() => {
            tool_text("No relevant Discord discussions found.".to_string())
        }
        Ok(results) => tool_text(crate::knowledgebase::format_discord_search_context(
            &results,
        )),
        Err(e) => tool_error(&format!("Discord search failed: {e}")),
    }
}

/// Resolve the optional channel scope for `discord_search`: `channel_id` as-is,
/// or a `channel` name matched loosely against the indexed-channel registry.
fn resolve_search_channel(ctx: &McpContext<'_>, args: &Value) -> Result<Option<String>, String> {
    if let Some(id) = scoped_str_arg(args, "channel_id")? {
        return Ok(Some(id.to_string()));
    }
    if let Some(name) = scoped_str_arg(args, "channel")? {
        let channels = ctx
            .tracker
            .list_discord_channels()
            .map_err(|e| format!("Failed to list indexed channels: {e}"))?;
        // Prefer an exact (case-insensitive) name before a loose match.
        let exact = channels.iter().find(|c| {
            c.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        });
        let wanted = normalize_channel_name(name);
        return exact
            .or_else(|| {
                channels.iter().find(|c| {
                    c.name
                        .as_deref()
                        .is_some_and(|n| normalize_channel_name(n) == wanted)
                })
            })
            .map(|c| Some(c.channel_id.clone()))
            .ok_or_else(|| format!("Channel '{name}' not found in the indexed channels."));
    }
    Ok(None)
}

/// Live listing of recent messages in a Discord channel (not semantic search).
async fn discord_list_messages(ctx: &McpContext<'_>, args: &Value) -> Value {
    let Some(lister) = ctx.discord_lister else {
        return tool_error("Discord listing is unavailable (no Discord bot token configured).");
    };
    let channel_id = match resolve_discord_channel(ctx, lister, args).await {
        Ok(id) => id,
        Err(msg) => return tool_error(&msg),
    };
    let limit = ctx.cfg.resolve_limit(usize_arg(args, "limit"));

    match lister.list_channel_messages(&channel_id, limit).await {
        Ok(messages) => tool_text(format_discord_messages(&channel_id, &messages)),
        Err(e) => tool_error(&format!("Discord message listing failed: {e}")),
    }
}

/// Resolve the target channel id from `channel_id` (preferred) or `channel`
/// name (resolved within the configured guild). A present-but-blank value is an
/// error, as is the absence of both.
async fn resolve_discord_channel(
    ctx: &McpContext<'_>,
    lister: &dyn DiscordMessageLister,
    args: &Value,
) -> Result<String, String> {
    if let Some(id) = scoped_str_arg(args, "channel_id")? {
        return Ok(id.to_string());
    }
    if let Some(name) = scoped_str_arg(args, "channel")? {
        let guild = ctx.discord_guild_id.ok_or_else(|| {
            "Channel-name resolution needs a configured guild_id; pass 'channel_id' instead."
                .to_string()
        })?;
        let channels = lister
            .list_guild_channels(guild)
            .await
            .map_err(|e| format!("Failed to list Discord channels: {e}"))?;
        // Prefer an exact (case-insensitive) name, falling back to a loose match,
        // so `foo-bar` wins over `foobar` when both exist.
        let exact = channels.iter().find(|c| {
            c.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        });
        let wanted = normalize_channel_name(name);
        return exact
            .or_else(|| {
                channels.iter().find(|c| {
                    c.name
                        .as_deref()
                        .is_some_and(|n| normalize_channel_name(n) == wanted)
                })
            })
            .map(|c| c.id.clone())
            .ok_or_else(|| format!("Channel '{name}' not found in the configured guild."));
    }
    Err("Provide 'channel_id' or 'channel'.".to_string())
}

/// Normalize a channel name for loose matching: lowercase, keep alphanumerics
/// (Unicode-aware, so non-Latin names like `日本語` survive), dropping spaces,
/// punctuation and emoji. So "🗄-database", "Database", and "database" compare
/// equal, while distinct non-ASCII names stay distinct.
fn normalize_channel_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Live listing of HelpScout conversations from the configured mailboxes.
async fn helpscout_list_conversations(ctx: &McpContext<'_>, args: &Value) -> Value {
    let Some(source) = ctx.helpscout else {
        return tool_error("HelpScout listing is unavailable (HelpScout not configured).");
    };
    let requested = usize_arg(args, "limit");
    let limit = ctx.cfg.resolve_limit(requested);
    let status = str_arg(args, "status");

    match source.list_conversations(status).await {
        Ok(mut issues) => {
            // Results concatenate per-mailbox in config order; sort globally
            // newest-first (by update, then creation) before limiting so a later
            // mailbox's newer conversations aren't dropped.
            issues.sort_by(|a, b| {
                let ka = a.updated_at.or(a.created_at);
                let kb = b.updated_at.or(b.created_at);
                kb.cmp(&ka)
            });
            let total = issues.len();
            issues.truncate(limit);
            let mut out = format_helpscout_conversations(&issues);
            out.push_str(&cap_note(total, issues.len(), requested, ctx.cfg.max_limit));
            tool_text(out)
        }
        Err(e) => tool_error(&format!("HelpScout listing failed: {e}")),
    }
}

// --- argument helpers -------------------------------------------------------

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn usize_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|v| v as usize)
}

/// Resolve an optional string scope argument: absent → `Ok(None)`, a non-empty
/// string → `Ok(Some(..))`, and anything else present (wrong type, blank) →
/// `Err`, so a malformed scope cannot silently broaden the search.
fn scoped_str_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Some)
            .ok_or_else(|| format!("Argument '{key}' must be a non-empty string.")),
    }
}

/// Resolve an optional repo scope from the arguments into a repo id.
///
/// Accepts either `repo_id` (integer) directly, or `repo` (name) which is
/// looked up in the index. Returns `Ok(None)` only when neither key is present.
/// A key that is present but malformed (wrong type, blank name, unknown repo) is
/// an `Err` rather than a silent fall-through to an unscoped search, which would
/// otherwise leak results from other repositories.
fn resolve_repo(ctx: &McpContext<'_>, args: &Value) -> Result<Option<i64>, String> {
    if let Some(value) = args.get("repo_id") {
        return match value.as_i64() {
            Some(id) => Ok(Some(id)),
            None => Err("Argument 'repo_id' must be an integer.".to_string()),
        };
    }
    if let Some(value) = args.get("repo") {
        let name = value.as_str().map(str::trim).filter(|s| !s.is_empty());
        let Some(name) = name else {
            return Err("Argument 'repo' must be a non-empty string.".to_string());
        };
        // Resolve by id only: a repo populated purely by code indexing may lack
        // discovery-index metadata, which get_indexed_repo requires.
        return match ctx.tracker.get_repo_id_by_name(name) {
            Ok(Some(id)) => Ok(Some(id)),
            Ok(None) => Err(format!("Repository '{name}' is not indexed.")),
            Err(e) => Err(format!("Failed to look up repository '{name}': {e}")),
        };
    }
    Ok(None)
}

// --- result formatting ------------------------------------------------------

/// Map every indexed repo id to its name, for labelling results. Falls back to
/// an empty map (results then show `repo#<id>`) if the lookup fails.
fn repo_name_map(tracker: &dyn FixAttemptTracker) -> HashMap<i64, String> {
    // Reads only (id, name) so repos populated purely by code indexing (which
    // lack discovery-index metadata) are still labelled with their name.
    tracker
        .list_repo_id_names()
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// A display label for a repo id: its name, or `repo#<id>` when unknown. Results
/// can span repositories (when `repo` is omitted), so a repo-relative path alone
/// is ambiguous — every result carries this prefix.
fn repo_label(repo_names: &HashMap<i64, String>, repo_id: i64) -> String {
    repo_names
        .get(&repo_id)
        .cloned()
        .unwrap_or_else(|| format!("repo#{repo_id}"))
}

fn format_code_results(
    results: &[claudear_core::types::CodeSearchResult],
    repo_names: &HashMap<i64, String>,
) -> String {
    use std::fmt::Write;

    if results.is_empty() {
        return "No matching code found.".to_string();
    }

    let mut out = format!("Found {} code match(es):\n", results.len());
    for (i, result) in results.iter().enumerate() {
        let chunk = &result.chunk;
        let _ = write!(
            out,
            "\n### {}. {}:{}:{}-{} ({}, {:.0}% match)",
            i + 1,
            repo_label(repo_names, chunk.repo_id),
            chunk.file_path,
            chunk.start_line,
            chunk.end_line,
            chunk.language,
            result.score * 100.0,
        );
        if let Some(symbol) = chunk.symbol_name.as_ref().filter(|s| !s.is_empty()) {
            let _ = write!(out, " - `{symbol}`");
        }
        // Truncate long chunks on a UTF-8 boundary to keep responses bounded.
        let text = truncate_on_boundary(&chunk.chunk_text, 1500);
        let _ = write!(out, "\n```{}\n{}\n```\n", chunk.language, text);
    }
    out
}

fn format_symbols(
    symbols: &[claudear_core::types::CodeSymbol],
    repo_names: &HashMap<i64, String>,
) -> String {
    use std::fmt::Write;

    if symbols.is_empty() {
        return "No matching symbols found.".to_string();
    }

    let mut out = format!("Found {} symbol(s):\n", symbols.len());
    for symbol in symbols {
        let _ = write!(
            out,
            "\n- **{}** ({}) - {}:{}:{}-{}",
            symbol.symbol_name,
            symbol.symbol_kind,
            repo_label(repo_names, symbol.repo_id),
            symbol.file_path,
            symbol.start_line,
            symbol.end_line,
        );
        if let Some(parent) = symbol.parent_symbol.as_ref().filter(|p| !p.is_empty()) {
            let _ = write!(out, " (in `{parent}`)");
        }
        if let Some(sig) = symbol.signature.as_ref().filter(|s| !s.is_empty()) {
            let _ = write!(out, "\n  `{}`", truncate_on_boundary(sig, 300));
        }
    }
    out
}

fn format_discord_messages(channel_id: &str, messages: &[DiscordMessage]) -> String {
    use std::fmt::Write;

    if messages.is_empty() {
        return format!("No messages found in channel {channel_id}.");
    }

    let mut out = format!(
        "{} recent message(s) in channel {channel_id} (newest first):\n",
        messages.len()
    );
    for msg in messages {
        let author = msg
            .author
            .as_ref()
            .map(|u| u.username.as_str())
            .unwrap_or("unknown");
        let content = truncate_on_boundary(&msg.content, 500);
        let _ = write!(out, "\n**{}** · {}\n{}", author, msg.timestamp, content);
        if !msg.embeds.is_empty() {
            let _ = write!(out, "\n({} embed(s))", msg.embeds.len());
        }
        out.push('\n');
    }
    out
}

fn format_helpscout_conversations(issues: &[claudear_core::types::Issue]) -> String {
    use std::fmt::Write;

    if issues.is_empty() {
        return "No HelpScout conversations found.".to_string();
    }

    let mut out = format!("Found {} HelpScout conversation(s):\n", issues.len());
    for issue in issues {
        let _ = write!(out, "\n- **{}** {}", issue.short_id, issue.title);
        if let Some(status) = issue
            .get_metadata::<String>("status")
            .filter(|s| !s.is_empty())
        {
            let _ = write!(out, " [{status}]");
        }
        if let Some(email) = issue
            .get_metadata::<String>("customer_email")
            .filter(|s| !s.is_empty())
        {
            let _ = write!(out, " — {email}");
        }
        if !issue.url.is_empty() {
            let _ = write!(out, "\n  {}", issue.url);
        }
    }
    out
}

fn truncate_on_boundary(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…\n(truncated)", &text[..end])
}

// --- JSON-RPC / MCP envelope helpers ----------------------------------------

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A trailing note making result capping visible: shown vs. available, and when
/// a requested limit exceeded the server cap. Empty when nothing was capped.
fn cap_note(
    total_available: usize,
    shown: usize,
    requested: Option<usize>,
    max_limit: usize,
) -> String {
    if total_available > shown {
        format!(
            "\n\n_(showing {shown} of {total_available}; raise `limit` for more, up to max_limit={max_limit}.)_"
        )
    } else if requested.is_some_and(|r| r > max_limit) {
        format!("\n\n_(requested limit exceeds max_limit={max_limit}; showing {shown}.)_")
    } else {
        String::new()
    }
}

/// Build a successful `tools/call` result carrying a single text block.
fn tool_text(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

/// Build a `tools/call` result flagged as a tool execution error. Per the MCP
/// spec these are reported in the result (not as a JSON-RPC error) so the model
/// can see and react to them.
fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledgebase::{DiscordIndexer, DiscordMessageInput};
    use crate::repo::code_index::{CodeIndexer, CodeSearchService};
    use crate::storage::{SqliteTracker, UserStore};
    use async_trait::async_trait;
    use axum::http::HeaderName;
    use claudear_core::types::{Issue, MatchPriority, MatchResult};
    use claudear_integrations::discord::DiscordUser;

    /// Build a live search environment (embedding client + a tracker with the
    /// vectorlite extension loaded). Returns `None` when either is unavailable,
    /// so the populated semantic-search tests skip locally but run in CI (where
    /// both are installed) and there assert real results.
    fn live_search_env() -> Option<(
        Arc<dyn FixAttemptTracker>,
        Arc<crate::feedback::EmbeddingClient>,
    )> {
        let emb = try_embedding_client()?;
        let sqlite = SqliteTracker::in_memory().unwrap();
        if !sqlite.vectorlite_available() {
            return None;
        }
        let tracker: Arc<dyn FixAttemptTracker> = Arc::new(sqlite);
        Some((tracker, emb))
    }
    use claudear_core::types::{CodeChunk, CodeSearchResult, CodeSymbol, Language, SymbolKind};
    use std::sync::Arc;

    /// Try to build an embedding client; returns `None` when the ONNX model is
    /// unavailable (common in CI), so tests that need a real search service can
    /// early-return rather than fail. Mirrors the analysis crate's test helper.
    fn try_embedding_client() -> Option<Arc<crate::feedback::EmbeddingClient>> {
        crate::feedback::EmbeddingClient::new(crate::feedback::EmbeddingConfig {
            pool_size: 1,
            ..Default::default()
        })
        .ok()
        .map(Arc::new)
    }

    fn cfg(expose_code: bool, expose_discord: bool) -> McpSearchServerConfig {
        McpSearchServerConfig {
            enabled: true,
            expose_code,
            expose_discord,
            // Off by default here so most tests stay focused on code/discord;
            // the HelpScout tests set it explicitly.
            expose_helpscout: false,
            ..Default::default()
        }
    }

    /// Build a context with no live search services, backed by an in-memory
    /// tracker. Tool calls therefore report their service as unavailable, which
    /// is enough to exercise the full request/dispatch path.
    fn test_ctx<'a>(
        cfg: &'a McpSearchServerConfig,
        tracker: &'a dyn FixAttemptTracker,
    ) -> McpContext<'a> {
        McpContext {
            cfg,
            code_search: None,
            discord_search: None,
            tracker,
            cf_verifier: None,
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        }
    }

    /// Post a request, defaulting the `jsonrpc` envelope field to "2.0" so each
    /// test can focus on its own fields. Pass it explicitly to test negotiation.
    async fn post(ctx: &McpContext<'_>, mut request: Value) -> Option<Value> {
        if let Some(obj) = request.as_object_mut() {
            obj.entry("jsonrpc").or_insert(json!("2.0"));
        }
        match process_post(ctx, request.to_string().as_bytes()).await {
            PostOutcome::Accepted => None,
            PostOutcome::Json(value) => Some(value),
        }
    }

    fn tool_names(tools: &[Value]) -> Vec<String> {
        tools
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    // --- pure helpers -------------------------------------------------------

    #[test]
    fn tool_definitions_respect_toggles() {
        let all = tool_names(&tool_definitions(&cfg(true, true)));
        assert_eq!(
            all,
            vec![
                "code_search",
                "find_symbol",
                "discord_search",
                "discord_list_messages"
            ]
        );

        let code_only = tool_names(&tool_definitions(&cfg(true, false)));
        assert_eq!(code_only, vec!["code_search", "find_symbol"]);

        let discord_only = tool_names(&tool_definitions(&cfg(false, true)));
        assert_eq!(
            discord_only,
            vec!["discord_search", "discord_list_messages"]
        );

        assert!(tool_definitions(&cfg(false, false)).is_empty());

        // HelpScout is a separate toggle.
        let hs = McpSearchServerConfig {
            enabled: true,
            expose_code: false,
            expose_discord: false,
            expose_helpscout: true,
            ..Default::default()
        };
        assert_eq!(
            tool_names(&tool_definitions(&hs)),
            vec!["helpscout_list_conversations"]
        );
    }

    #[test]
    fn tool_definitions_have_valid_schemas() {
        for tool in tool_definitions(&cfg(true, true)) {
            assert!(tool["name"].is_string());
            assert!(tool["description"].is_string());
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(tool["inputSchema"]["required"].is_array());
        }
    }

    #[test]
    fn negotiate_version_only_accepts_supported() {
        assert_eq!(negotiate_version(Some("2025-06-18")), "2025-06-18");
        assert_eq!(negotiate_version(Some("2024-11-05")), "2024-11-05");
        // Unknown or missing versions fall back to the newest supported one.
        assert_eq!(
            negotiate_version(Some("1999-01-01")),
            DEFAULT_PROTOCOL_VERSION
        );
        assert_eq!(negotiate_version(None), DEFAULT_PROTOCOL_VERSION);
    }

    #[test]
    fn origin_allowed_blocks_unknown_browser_origins() {
        let allowed = vec!["https://app.example.com".to_string()];
        // No Origin header (CLI/server client) is always allowed.
        assert!(origin_allowed(&allowed, None));
        assert!(origin_allowed(&allowed, Some("https://app.example.com")));
        assert!(!origin_allowed(&allowed, Some("https://evil.example.com")));
        // Empty allow-list blocks every browser origin.
        assert!(!origin_allowed(&[], Some("https://app.example.com")));
        assert!(origin_allowed(&[], None));
    }

    #[test]
    fn rpc_envelopes_are_well_formed() {
        let ok = rpc_result(json!(1), json!({ "x": 2 }));
        assert_eq!(ok["jsonrpc"], "2.0");
        assert_eq!(ok["id"], 1);
        assert_eq!(ok["result"]["x"], 2);
        assert!(ok.get("error").is_none());

        let err = rpc_error(json!("abc"), -32601, "nope".to_string());
        assert_eq!(err["id"], "abc");
        assert_eq!(err["error"]["code"], -32601);
        assert_eq!(err["error"]["message"], "nope");
        assert!(err.get("result").is_none());
    }

    #[test]
    fn tool_result_helpers_set_error_flag() {
        let ok = tool_text("hi".to_string());
        assert_eq!(ok["content"][0]["type"], "text");
        assert_eq!(ok["content"][0]["text"], "hi");
        assert!(ok.get("isError").is_none());

        let err = tool_error("boom");
        assert_eq!(err["isError"], true);
        assert_eq!(err["content"][0]["text"], "boom");
    }

    #[test]
    fn str_arg_trims_and_rejects_blank() {
        let args = json!({ "query": "  hello  ", "blank": "   ", "n": 3 });
        assert_eq!(str_arg(&args, "query"), Some("hello"));
        assert_eq!(str_arg(&args, "blank"), None);
        assert_eq!(str_arg(&args, "missing"), None);
        assert_eq!(str_arg(&args, "n"), None);
    }

    #[tokio::test]
    async fn resolve_repo_rejects_malformed_scope() {
        let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let known = tracker.get_or_create_repo_id("org/known").unwrap();
        let c = cfg(true, true);
        let ctx = McpContext {
            cfg: &c,
            code_search: None,
            discord_search: None,
            tracker: tracker.as_ref(),
            cf_verifier: None,
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        };

        // No scope keys → unscoped.
        assert_eq!(resolve_repo(&ctx, &json!({})), Ok(None));
        // Valid repo_id / repo.
        assert_eq!(resolve_repo(&ctx, &json!({ "repo_id": 5 })), Ok(Some(5)));
        assert_eq!(
            resolve_repo(&ctx, &json!({ "repo": "org/known" })),
            Ok(Some(known))
        );
        // Present-but-malformed must error, not silently broaden to all repos.
        assert!(resolve_repo(&ctx, &json!({ "repo_id": "x" })).is_err());
        assert!(resolve_repo(&ctx, &json!({ "repo": 123 })).is_err());
        assert!(resolve_repo(&ctx, &json!({ "repo": "   " })).is_err());
        assert!(resolve_repo(&ctx, &json!({ "repo": "org/missing" })).is_err());
    }

    #[test]
    fn usize_arg_parses_only_unsigned() {
        let args = json!({ "limit": 5, "neg": -1, "s": "7" });
        assert_eq!(usize_arg(&args, "limit"), Some(5));
        assert_eq!(usize_arg(&args, "neg"), None);
        assert_eq!(usize_arg(&args, "s"), None);
        assert_eq!(usize_arg(&args, "missing"), None);
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        assert_eq!(truncate_on_boundary("short", 10), "short");
        // "a" + "é"×10: 'a' at byte 0, then each 'é' spans 2 bytes. Byte offset 4
        // lands inside the second 'é', so truncation must step back to offset 3 -
        // a naive `&text[..4]` would panic. (Offset 5 is already a boundary and
        // would not exercise this path.)
        let s = "a".to_string() + &"é".repeat(10);
        let out = truncate_on_boundary(&s, 4);
        assert!(out.ends_with("(truncated)"));
        // Stepped back to the boundary at byte 3: "a" + one "é".
        assert!(out.starts_with("aé…"));
    }

    fn chunk() -> CodeChunk {
        CodeChunk {
            id: Some(1),
            repo_id: 1,
            file_path: "src/lib.rs".to_string(),
            chunk_type: "function".to_string(),
            symbol_name: Some("do_thing".to_string()),
            language: Language::Rust,
            start_line: 10,
            end_line: 20,
            chunk_text: "fn do_thing() {}".to_string(),
            context_text: String::new(),
            file_hash: "h".to_string(),
            content_hash: None,
        }
    }

    fn repo_names() -> HashMap<i64, String> {
        HashMap::from([(1, "org/repo".to_string())])
    }

    #[test]
    fn format_code_results_renders_matches() {
        let names = repo_names();
        assert_eq!(format_code_results(&[], &names), "No matching code found.");

        let results = vec![CodeSearchResult {
            chunk: chunk(),
            score: 0.9321,
        }];
        let out = format_code_results(&results, &names);
        // Repo name prefixes the (repo-relative) path so cross-repo results are
        // unambiguous.
        assert!(out.contains("org/repo:src/lib.rs:10-20"), "got: {out}");
        assert!(out.contains("93% match"));
        assert!(out.contains("do_thing"));
        assert!(out.contains("fn do_thing()"));

        // Unknown repo id falls back to repo#<id>.
        let out_unknown = format_code_results(&results, &HashMap::new());
        assert!(
            out_unknown.contains("repo#1:src/lib.rs"),
            "got: {out_unknown}"
        );
    }

    #[test]
    fn format_symbols_renders_matches() {
        let names = repo_names();
        assert_eq!(format_symbols(&[], &names), "No matching symbols found.");

        let symbols = vec![CodeSymbol {
            id: Some(1),
            repo_id: 1,
            file_path: "src/lib.rs".to_string(),
            symbol_name: "do_thing".to_string(),
            symbol_kind: SymbolKind::Function,
            parent_symbol: Some("Thing".to_string()),
            language: Language::Rust,
            start_line: 10,
            end_line: 20,
            signature: Some("fn do_thing()".to_string()),
        }];
        let out = format_symbols(&symbols, &names);
        assert!(out.contains("**do_thing** (function)"));
        assert!(out.contains("org/repo:src/lib.rs:10-20"), "got: {out}");
        assert!(out.contains("in `Thing`"));
        assert!(out.contains("fn do_thing()"));
    }

    // --- request-path (observable response) tests ---------------------------

    #[tokio::test]
    async fn initialize_returns_capabilities_and_negotiated_version() {
        let cfg = cfg(true, true);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker);

        let resp = post(
            &ctx,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2025-06-18" }
            }),
        )
        .await
        .expect("initialize returns a body");

        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(resp["result"]["serverInfo"]["name"], "claudear-search");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn initialize_downgrades_unknown_version() {
        let cfg = cfg(true, true);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker);

        let resp = post(
            &ctx,
            json!({ "id": 1, "method": "initialize", "params": { "protocolVersion": "3000-01-01" } }),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn tools_list_reflects_exposure_toggles() {
        let cfg = cfg(true, false);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker);

        let resp = post(&ctx, json!({ "id": 2, "method": "tools/list" }))
            .await
            .unwrap();
        let names = tool_names(resp["result"]["tools"].as_array().unwrap());
        assert_eq!(names, vec!["code_search", "find_symbol"]);
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        let cfg = cfg(true, true);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker);

        let resp = post(&ctx, json!({ "id": 9, "method": "ping" }))
            .await
            .unwrap();
        assert_eq!(resp["result"], json!({}));
    }

    #[tokio::test]
    async fn notification_is_acknowledged_without_body() {
        let cfg = cfg(true, true);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker);

        // No `id` => notification => 202 with no JSON body.
        let resp = post(&ctx, json!({ "method": "notifications/initialized" })).await;
        assert!(resp.is_none());
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let cfg = cfg(true, true);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker);

        let resp = post(&ctx, json!({ "id": 3, "method": "does/not/exist" }))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn parse_error_and_batch_are_rejected() {
        let cfg = cfg(true, true);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker);

        let PostOutcome::Json(parse) = process_post(&ctx, b"{ not json").await else {
            panic!("expected JSON body");
        };
        assert_eq!(parse["error"]["code"], -32700);

        let PostOutcome::Json(batch) = process_post(&ctx, b"[{}]").await else {
            panic!("expected JSON body");
        };
        assert_eq!(batch["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn invalid_envelopes_are_rejected() {
        let cfg = cfg(true, true);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker);

        // Non-object JSON values are Invalid Request, not notifications.
        for raw in [b"null".as_slice(), b"42", b"\"hi\"", b"{}"] {
            let PostOutcome::Json(v) = process_post(&ctx, raw).await else {
                panic!("expected JSON body for {raw:?}");
            };
            assert_eq!(v["error"]["code"], -32600, "payload {raw:?}");
        }

        // Wrong jsonrpc version is rejected (note: post() would inject "2.0", so
        // build the body explicitly here).
        let bad_version = json!({ "jsonrpc": "1.0", "id": 1, "method": "ping" });
        let PostOutcome::Json(v) = process_post(&ctx, bad_version.to_string().as_bytes()).await
        else {
            panic!("expected JSON body");
        };
        assert_eq!(v["error"]["code"], -32600);

        // An object/array id is invalid.
        let bad_id = json!({ "jsonrpc": "2.0", "id": {"x": 1}, "method": "ping" });
        let PostOutcome::Json(v) = process_post(&ctx, bad_id.to_string().as_bytes()).await else {
            panic!("expected JSON body");
        };
        assert_eq!(v["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn missing_jsonrpc_notification_is_still_rejected() {
        let cfg = cfg(true, true);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker);

        // A notification missing the jsonrpc field is an invalid envelope, not a
        // silent 202. (post() injects jsonrpc, so call process_post directly.)
        let raw = json!({ "method": "notifications/initialized" });
        let PostOutcome::Json(v) = process_post(&ctx, raw.to_string().as_bytes()).await else {
            panic!("expected JSON body");
        };
        assert_eq!(v["error"]["code"], -32600);

        // A well-formed notification (jsonrpc present, no id) is acked with 202.
        let ok = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(matches!(
            process_post(&ctx, ok.to_string().as_bytes()).await,
            PostOutcome::Accepted
        ));
    }

    #[tokio::test]
    async fn tool_call_reports_unavailable_service_as_error_result() {
        let cfg = cfg(true, true);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker);

        let resp = post(
            &ctx,
            json!({
                "id": 4,
                "method": "tools/call",
                "params": { "name": "code_search", "arguments": { "query": "foo" } }
            }),
        )
        .await
        .unwrap();

        // Service unavailable is a tool-execution error, not a JSON-RPC error.
        assert!(resp.get("error").is_none());
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unavailable"));
    }

    #[test]
    fn hash_token_is_stable_and_distinct() {
        assert_eq!(hash_token("cldr_abc"), hash_token("cldr_abc"));
        assert_ne!(hash_token("cldr_abc"), hash_token("cldr_xyz"));
        // 32-byte SHA-256 -> 64 hex chars.
        assert_eq!(hash_token("x").len(), 64);
    }

    #[test]
    fn bearer_token_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer_token(&h), None);
        h.insert(header::AUTHORIZATION, "Bearer   tok123  ".parse().unwrap());
        assert_eq!(bearer_token(&h), Some("tok123"));
        h.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer_token(&h), None);
    }

    #[test]
    fn authenticate_bearer_resolves_created_token() {
        let tracker = SqliteTracker::in_memory().unwrap();
        let user_id = tracker
            .create_user("dev@example.com", "hash", "Dev", "viewer")
            .unwrap();
        // Mint a token the way the API does: store its hash + prefix.
        let secret = "cldr_secret_value";
        tracker
            .create_api_token(user_id, "laptop", &hash_token(secret), "cldr_secr", None)
            .unwrap();

        // No header -> 401.
        assert!(authenticate_bearer(&HeaderMap::new(), &tracker).is_err());

        // Wrong token -> 401.
        let mut bad = HeaderMap::new();
        bad.insert(header::AUTHORIZATION, "Bearer nope".parse().unwrap());
        assert!(authenticate_bearer(&bad, &tracker).is_err());

        // Correct token -> the owning user.
        let mut good = HeaderMap::new();
        good.insert(
            header::AUTHORIZATION,
            format!("Bearer {secret}").parse().unwrap(),
        );
        let user = authenticate_bearer(&good, &tracker).expect("valid token");
        assert_eq!(user.email, "dev@example.com");
    }

    // --- end-to-end gate tests through the HTTP handler (handle_request) -----

    const PING: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    #[tokio::test]
    async fn handler_requires_bearer_token() {
        let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let uid = tracker.create_user("d@e.com", "h", "D", "viewer").unwrap();
        let secret = "cldr_handler_secret";
        tracker
            .create_api_token(uid, "t", &hash_token(secret), "cldr_h", None)
            .unwrap();

        let cfg = cfg(true, true); // require_auth is true by default
        let ctx = McpContext {
            cfg: &cfg,
            code_search: None,
            discord_search: None,
            tracker: tracker.as_ref(),
            cf_verifier: None,
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        };

        // No token → 401.
        let resp = handle_request(&ctx, &HeaderMap::new(), PING).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Valid token → 200.
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {secret}").parse().unwrap(),
        );
        let resp = handle_request(&ctx, &h, PING).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn handler_rejects_disallowed_origin() {
        let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut cfg = cfg(true, true);
        cfg.require_auth = false; // isolate the Origin check
        cfg.allowed_origins = vec!["https://ok.example".to_string()];
        let ctx = McpContext {
            cfg: &cfg,
            code_search: None,
            discord_search: None,
            tracker: tracker.as_ref(),
            cf_verifier: None,
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        };

        let mut bad = HeaderMap::new();
        bad.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert_eq!(
            handle_request(&ctx, &bad, PING).await.status(),
            StatusCode::FORBIDDEN
        );

        let mut ok = HeaderMap::new();
        ok.insert(header::ORIGIN, "https://ok.example".parse().unwrap());
        assert_eq!(
            handle_request(&ctx, &ok, PING).await.status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn handler_enforces_cloudflare_access_when_required() {
        let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let mut cfg = cfg(true, true);
        cfg.require_auth = false;
        cfg.require_cloudflare_access = true;

        // No verifier configured → the gate fails closed with 403.
        let ctx = McpContext {
            cfg: &cfg,
            code_search: None,
            discord_search: None,
            tracker: tracker.as_ref(),
            cf_verifier: None,
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        };
        assert_eq!(
            handle_request(&ctx, &HeaderMap::new(), PING).await.status(),
            StatusCode::FORBIDDEN
        );

        // With a *configured* verifier: a missing assertion and a malformed
        // assertion are both rejected (a genuine assertion is verified end-to-end
        // in cf_access's own tests, which hold the signing key).
        let verifier = CfAccessVerifier::new(&crate::config::CloudflareAccessConfig {
            team_domain: "team.cloudflareaccess.com".to_string(),
            audience: "aud-tag".to_string(),
        });
        let ctx = McpContext {
            cfg: &cfg,
            code_search: None,
            discord_search: None,
            tracker: tracker.as_ref(),
            cf_verifier: Some(&verifier),
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        };
        // Missing Cf-Access-Jwt-Assertion header → 403.
        assert_eq!(
            handle_request(&ctx, &HeaderMap::new(), PING).await.status(),
            StatusCode::FORBIDDEN
        );
        // Malformed assertion → 403 (fails before any network).
        let mut h = HeaderMap::new();
        h.insert(
            HeaderName::from_static("cf-access-jwt-assertion"),
            "not-a-jwt".parse().unwrap(),
        );
        assert_eq!(
            handle_request(&ctx, &h, PING).await.status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn find_symbol_tool_scopes_by_repo_and_applies_limit() {
        // Needs a real CodeSearchService (construction requires an embedding
        // client); skip where the model is unavailable, like the analysis tests.
        let Some(emb) = try_embedding_client() else {
            return;
        };
        let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let repo_id = tracker.get_or_create_repo_id("org/repo").unwrap();
        let other_id = tracker.get_or_create_repo_id("org/other").unwrap();

        let sym = |name: &str, rid: i64| CodeSymbol {
            id: None,
            repo_id: rid,
            file_path: format!("src/{name}.rs"),
            symbol_name: name.to_string(),
            symbol_kind: SymbolKind::Function,
            parent_symbol: None,
            language: Language::Rust,
            start_line: 1,
            end_line: 2,
            signature: Some(format!("fn {name}()")),
        };
        // The out-of-repo symbol sorts FIRST ("handle_0" < "handle_a"), so if the
        // repo filter were dropped it would appear in the (name-ordered) top-2 and
        // fail the assertions below.
        tracker
            .save_code_symbols(&[
                sym("handle_a", repo_id),
                sym("handle_b", repo_id),
                sym("handle_c", repo_id),
                sym("handle_0", other_id),
            ])
            .unwrap();

        let service = CodeSearchService::new(tracker.clone(), emb);
        // max_limit caps results below the number of matches.
        let cfg = McpSearchServerConfig {
            enabled: true,
            expose_code: true,
            expose_discord: false,
            default_limit: 10,
            max_limit: 2,
            ..Default::default()
        };
        let ctx = McpContext {
            cfg: &cfg,
            code_search: Some(&service),
            discord_search: None,
            tracker: tracker.as_ref(),
            cf_verifier: None,
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        };

        let resp = post(
            &ctx,
            json!({
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "find_symbol",
                    "arguments": { "name": "handle_", "repo": "org/repo", "limit": 5 }
                }
            }),
        )
        .await
        .unwrap();

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        // max_limit=2 caps the three in-repo matches down to two, and repo scoping
        // keeps the first-sorting out-of-repo "handle_0" out entirely (losing the
        // filter would put handle_0 in the top-2 and fail here).
        assert!(text.contains("Found 2 symbol(s)"), "got: {text}");
        assert!(
            text.contains("handle_a") && text.contains("handle_b"),
            "got: {text}"
        );
        assert!(!text.contains("handle_0"), "other repo leaked: {text}");

        // An unknown repo name is a tool error, not a protocol error.
        let bad = post(
            &ctx,
            json!({
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "find_symbol",
                    "arguments": { "name": "handle_", "repo": "org/missing" }
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(bad["result"]["isError"], true);
    }

    #[tokio::test]
    async fn code_search_tool_returns_populated_results() {
        let Some((tracker, emb)) = live_search_env() else {
            return;
        };

        // Index the target repo with three matching files, and a second repo with
        // its own match, so we can prove both repo scoping and limit forwarding.
        let indexer = CodeIndexer::new(tracker.clone(), emb.clone());
        let repo_dir = tempfile::tempdir().unwrap();
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(
                repo_dir.path().join(name),
                format!(
                    "/// Parse the application configuration from disk ({name}).\n\
                     pub fn parse_config_{n}(path: &str) -> Config {{ load(path) }}\n",
                    n = name.trim_end_matches(".rs")
                ),
            )
            .unwrap();
        }
        indexer
            .index_repo("org/repo", repo_dir.path())
            .await
            .unwrap();

        let other_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            other_dir.path().join("elsewhere.rs"),
            "/// Parse the application configuration from disk (other).\n\
             pub fn parse_config_other(path: &str) -> Config { load(path) }\n",
        )
        .unwrap();
        indexer
            .index_repo("org/other", other_dir.path())
            .await
            .unwrap();

        let service = CodeSearchService::new(tracker.clone(), emb);
        // max_limit below the number of in-repo matches, so the cap is observable.
        let c = McpSearchServerConfig {
            enabled: true,
            expose_code: true,
            expose_discord: false,
            default_limit: 10,
            max_limit: 2,
            ..Default::default()
        };
        let ctx = McpContext {
            cfg: &c,
            code_search: Some(&service),
            discord_search: None,
            tracker: tracker.as_ref(),
            cf_verifier: None,
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        };

        let resp = post(
            &ctx,
            json!({
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "code_search",
                    "arguments": { "query": "parse configuration", "repo": "org/repo", "limit": 10 }
                }
            }),
        )
        .await
        .unwrap();

        assert!(resp.get("error").is_none());
        assert_eq!(resp["result"].get("isError"), None);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        // Limit forwarding: three files match but max_limit caps the result at two.
        assert!(
            text.contains("Found 2 code match(es)"),
            "limit not applied: {text}"
        );
        // Repo scoping: only the target repo appears, never org/other.
        assert!(text.contains("org/repo"), "missing repo label: {text}");
        assert!(!text.contains("org/other"), "repo scope leaked: {text}");

        // Forwarding is also observable on the error paths: a missing query and a
        // malformed scope must surface as tool errors.
        let missing_query = post(
            &ctx,
            json!({ "id": 2, "method": "tools/call",
                    "params": { "name": "code_search", "arguments": {} } }),
        )
        .await
        .unwrap();
        assert_eq!(missing_query["result"]["isError"], true);

        let bad_scope = post(
            &ctx,
            json!({ "id": 3, "method": "tools/call",
                    "params": { "name": "code_search",
                                "arguments": { "query": "x", "repo": "   " } } }),
        )
        .await
        .unwrap();
        assert_eq!(bad_scope["result"]["isError"], true);
    }

    #[tokio::test]
    async fn discord_search_tool_returns_populated_results() {
        let Some((tracker, emb)) = live_search_env() else {
            return;
        };

        let indexer = DiscordIndexer::new(tracker.clone(), emb.clone());
        let msg = |id: &str, ts: &str| DiscordMessageInput {
            message_id: id.to_string(),
            channel_id: "chan1".to_string(),
            guild_id: "guild1".to_string(),
            channel_name: "general".to_string(),
            is_thread: false,
            author: "alice".to_string(),
            content: format!("deploy rollback discussion {id}"),
            timestamp: ts.to_string(),
            reply_to: None,
        };
        // Timestamps >10 min apart force separate chunks, so chan1 has two
        // matches and the limit cap is observable.
        indexer
            .index(
                "chan1",
                false,
                vec![
                    msg("1", "2024-01-01T10:00:00Z"),
                    msg("2", "2024-01-01T12:00:00Z"),
                    msg("3", "2024-01-01T14:00:00Z"),
                ],
            )
            .await
            .unwrap();
        // A second channel whose messages ALSO match the query, so if the channel
        // filter were dropped these would rank into the results and be detected.
        let msg2 = |id: &str, ts: &str| DiscordMessageInput {
            channel_id: "chan2".to_string(),
            content: format!("deploy rollback incident {id}"),
            ..msg(id, ts)
        };
        indexer
            .index(
                "chan2",
                false,
                vec![
                    msg2("4", "2024-01-02T10:00:00Z"),
                    msg2("5", "2024-01-02T12:00:00Z"),
                ],
            )
            .await
            .unwrap();

        let service = crate::knowledgebase::DiscordSearchService::new(tracker.clone(), emb);
        // High limit so every match could surface: the channel filter is the only
        // thing keeping chan2 out, making a dropped filter observable.
        let c = McpSearchServerConfig {
            enabled: true,
            expose_code: false,
            expose_discord: true,
            default_limit: 10,
            max_limit: 10,
            ..Default::default()
        };
        let ctx = McpContext {
            cfg: &c,
            code_search: None,
            discord_search: Some(&service),
            tracker: tracker.as_ref(),
            cf_verifier: None,
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        };

        let resp = post(
            &ctx,
            json!({
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "discord_search",
                    "arguments": { "query": "deploy rollback", "channel_id": "chan1", "limit": 10 }
                }
            }),
        )
        .await
        .unwrap();

        // Vector search is available (live_search_env gated on it), so results are
        // populated and the channel scope is forwarded: even though chan2 also
        // matches the query, the retrieved context references chan1 and never
        // chan2 — so dropping the filter would fail this.
        assert!(resp.get("error").is_none());
        assert_eq!(resp["result"].get("isError"), None);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("Relevant Discussions"),
            "expected populated results: {text}"
        );
        assert!(text.contains("chan1"), "got: {text}");
        assert!(!text.contains("chan2"), "channel scope leaked: {text}");

        // Limit forwarding: with max_limit = 1 the multi-chunk chan1 result set is
        // capped to a single entry (results are numbered "### 1.", "### 2." ...).
        let capped = McpSearchServerConfig {
            max_limit: 1,
            ..c.clone()
        };
        let ctx_capped = McpContext {
            cfg: &capped,
            code_search: None,
            discord_search: Some(&service),
            tracker: tracker.as_ref(),
            cf_verifier: None,
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        };
        let resp = post(
            &ctx_capped,
            json!({
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "discord_search",
                    "arguments": { "query": "deploy rollback", "channel_id": "chan1", "limit": 10 }
                }
            }),
        )
        .await
        .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("### 1."), "expected a result entry: {text}");
        assert!(!text.contains("### 2."), "limit not applied: {text}");

        // Forwarding is observable without vector results: a missing query and a
        // malformed channel_id scope must surface as tool errors.
        let missing_query = post(
            &ctx,
            json!({ "id": 2, "method": "tools/call",
                    "params": { "name": "discord_search", "arguments": {} } }),
        )
        .await
        .unwrap();
        assert_eq!(missing_query["result"]["isError"], true);

        let bad_scope = post(
            &ctx,
            json!({ "id": 3, "method": "tools/call",
                    "params": { "name": "discord_search",
                                "arguments": { "query": "x", "channel_id": 123 } } }),
        )
        .await
        .unwrap();
        assert_eq!(bad_scope["result"]["isError"], true);
    }

    #[tokio::test]
    async fn tool_call_rejects_unknown_or_disabled_tool() {
        let cfg = cfg(true, false); // discord disabled
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker);

        let resp = post(
            &ctx,
            json!({
                "id": 5,
                "method": "tools/call",
                "params": { "name": "discord_search", "arguments": { "query": "x" } }
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    // --- list tools (HelpScout + Discord) -----------------------------------

    /// A fake HelpScout `IssueSource` returning canned conversations and
    /// recording the status passed to `list_conversations`.
    struct FakeHelpScout {
        issues: Vec<Issue>,
        last_status: std::sync::Mutex<Option<String>>,
    }

    impl FakeHelpScout {
        fn new(issues: Vec<Issue>) -> Self {
            Self {
                issues,
                last_status: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl IssueSource for FakeHelpScout {
        fn name(&self) -> &str {
            "helpscout"
        }
        fn display_name(&self) -> &str {
            "HelpScout"
        }
        async fn fetch_issues(&self) -> crate::error::Result<Vec<Issue>> {
            Ok(self.issues.clone())
        }
        async fn list_conversations(
            &self,
            status: Option<&str>,
        ) -> crate::error::Result<Vec<Issue>> {
            *self.last_status.lock().unwrap() = status.map(str::to_string);
            Ok(self.issues.clone())
        }
        fn matches_criteria(&self, _issue: &Issue) -> MatchResult {
            MatchResult {
                matches: false,
                reason: String::new(),
                priority: MatchPriority::default(),
            }
        }
        async fn build_issue_context(&self, _issue: &Issue) -> crate::error::Result<String> {
            Ok(String::new())
        }
        async fn get_issue(&self, _issue_id: &str) -> crate::error::Result<Issue> {
            Err(crate::error::Error::Other("not implemented".into()))
        }
    }

    fn hs_issue(short: &str, title: &str, status: &str, email: &str, minutes_ago: i64) -> Issue {
        let mut issue = Issue::new(
            short,
            short,
            title,
            "https://secure.helpscout.net/conversation/1",
            "helpscout",
        );
        issue.set_metadata("status", status.to_string());
        issue.set_metadata("customer_email", email.to_string());
        issue.updated_at = Some(chrono::Utc::now() - chrono::Duration::minutes(minutes_ago));
        issue
    }

    #[tokio::test]
    async fn helpscout_list_tool_returns_conversations_and_applies_limit() {
        let cfg = McpSearchServerConfig {
            enabled: true,
            expose_code: false,
            expose_discord: false,
            expose_helpscout: true,
            max_limit: 2,
            ..Default::default()
        };
        // Deliberately out of order: HS-2 is newest, HS-1 oldest. Newest-first
        // ordering + limit=2 must keep HS-2 and HS-3 and drop the oldest (HS-1),
        // not just the last in input order.
        let source = FakeHelpScout::new(vec![
            hs_issue("HS-1", "Login broken", "active", "a@x.com", 60),
            hs_issue("HS-2", "Billing question", "pending", "b@x.com", 1),
            hs_issue("HS-3", "Feature request", "active", "c@x.com", 10),
        ]);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = McpContext {
            cfg: &cfg,
            code_search: None,
            discord_search: None,
            tracker: &tracker,
            cf_verifier: None,
            helpscout: Some(&source),
            discord_lister: None,
            discord_guild_id: None,
        };

        let resp = post(
            &ctx,
            json!({ "id": 1, "method": "tools/call",
                    "params": { "name": "helpscout_list_conversations", "arguments": { "limit": 10 } } }),
        )
        .await
        .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        // Limit caps 3 -> 2, and fields render.
        assert!(
            text.contains("Found 2 HelpScout conversation(s)"),
            "got: {text}"
        );
        assert!(text.contains("[pending]") && text.contains("b@x.com"));
        // Global newest-first: HS-1 (oldest) is dropped, and HS-2 precedes HS-3.
        assert!(!text.contains("HS-1"), "oldest not dropped: {text}");
        let pos2 = text.find("HS-2").expect("HS-2 present");
        let pos3 = text.find("HS-3").expect("HS-3 present");
        assert!(pos2 < pos3, "not newest-first: {text}");
    }

    #[tokio::test]
    async fn helpscout_list_tool_reports_unavailable() {
        let cfg = McpSearchServerConfig {
            enabled: true,
            expose_helpscout: true,
            ..Default::default()
        };
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = McpContext {
            cfg: &cfg,
            code_search: None,
            discord_search: None,
            tracker: &tracker,
            cf_verifier: None,
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        };
        let resp = post(
            &ctx,
            json!({ "id": 1, "method": "tools/call",
                    "params": { "name": "helpscout_list_conversations", "arguments": {} } }),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }

    /// A fake Discord lister that returns channel-specific messages, keyed by
    /// channel id, and only answers channel lookups for its configured guild.
    /// This lets tests verify that a channel *name* resolves to the right *id*.
    struct FakeDiscord {
        guild_id: String,
        by_channel: std::collections::HashMap<String, Vec<DiscordMessage>>,
        channels: Vec<DiscordChannel>,
    }

    #[async_trait]
    impl DiscordMessageLister for FakeDiscord {
        async fn list_channel_messages(
            &self,
            channel_id: &str,
            limit: usize,
        ) -> crate::error::Result<Vec<DiscordMessage>> {
            Ok(self
                .by_channel
                .get(channel_id)
                .map(|msgs| msgs.iter().take(limit).cloned().collect())
                .unwrap_or_default())
        }
        async fn list_guild_channels(
            &self,
            guild_id: &str,
        ) -> crate::error::Result<Vec<DiscordChannel>> {
            if guild_id != self.guild_id {
                return Ok(Vec::new());
            }
            Ok(self.channels.clone())
        }
    }

    fn dmsg(id: &str, channel_id: &str, author: &str, content: &str) -> DiscordMessage {
        DiscordMessage {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
            author: Some(DiscordUser {
                id: "u1".to_string(),
                username: author.to_string(),
                discriminator: "0".to_string(),
                avatar: None,
                bot: false,
            }),
            content: content.to_string(),
            timestamp: "2024-01-01T10:00:00Z".to_string(),
            message_reference: None,
            thread: None,
            webhook_id: None,
            embeds: Vec::new(),
            mentions: Vec::new(),
        }
    }

    fn dchan(id: &str, name: &str) -> DiscordChannel {
        DiscordChannel {
            id: id.to_string(),
            channel_type: 0,
            guild_id: Some("guild1".to_string()),
            name: Some(name.to_string()),
            parent_id: None,
        }
    }

    #[tokio::test]
    async fn discord_list_messages_tool_lists_and_resolves_names() {
        let cfg = cfg(false, true);
        let lister = FakeDiscord {
            guild_id: "guild1".to_string(),
            by_channel: std::collections::HashMap::from([
                (
                    "100".to_string(),
                    vec![dmsg("1", "100", "alice", "deploy pipeline is green")],
                ),
                (
                    "200".to_string(),
                    vec![dmsg("2", "200", "bob", "lunch plans today")],
                ),
            ]),
            channels: vec![dchan("100", "engineering"), dchan("200", "random")],
        };
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = McpContext {
            cfg: &cfg,
            code_search: None,
            discord_search: None,
            tracker: &tracker,
            cf_verifier: None,
            helpscout: None,
            discord_lister: Some(&lister),
            discord_guild_id: Some("guild1"),
        };

        // By channel id -> that channel's messages.
        let resp = post(
            &ctx,
            json!({ "id": 1, "method": "tools/call",
                    "params": { "name": "discord_list_messages", "arguments": { "channel_id": "100", "limit": 10 } } }),
        )
        .await
        .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("deploy pipeline is green"), "got: {text}");

        // By channel name -> resolves "engineering" to id 100, returning *its*
        // messages and not the "random" channel's (so wrong resolution fails).
        let resp = post(
            &ctx,
            json!({ "id": 2, "method": "tools/call",
                    "params": { "name": "discord_list_messages", "arguments": { "channel": "engineering" } } }),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"].get("isError"), None);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("deploy pipeline is green"), "got: {text}");
        assert!(
            !text.contains("lunch plans"),
            "resolved to wrong channel: {text}"
        );

        // Unknown channel name -> tool error.
        let resp = post(
            &ctx,
            json!({ "id": 3, "method": "tools/call",
                    "params": { "name": "discord_list_messages", "arguments": { "channel": "nope" } } }),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);

        // Neither channel nor channel_id -> tool error.
        let resp = post(
            &ctx,
            json!({ "id": 4, "method": "tools/call",
                    "params": { "name": "discord_list_messages", "arguments": {} } }),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }

    #[tokio::test]
    async fn discord_list_messages_tool_reports_unavailable() {
        let cfg = cfg(false, true);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = test_ctx(&cfg, &tracker); // discord_lister None
        let resp = post(
            &ctx,
            json!({ "id": 1, "method": "tools/call",
                    "params": { "name": "discord_list_messages", "arguments": { "channel_id": "100" } } }),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }

    #[test]
    fn format_list_outputs_render() {
        assert_eq!(
            format_helpscout_conversations(&[]),
            "No HelpScout conversations found."
        );
        let hs =
            format_helpscout_conversations(&[hs_issue("HS-9", "Oops", "active", "z@x.com", 0)]);
        assert!(hs.contains("HS-9") && hs.contains("Oops") && hs.contains("[active]"));

        assert!(format_discord_messages("100", &[]).contains("No messages"));
        let dm = format_discord_messages("100", &[dmsg("1", "100", "alice", "hello world")]);
        assert!(dm.contains("alice") && dm.contains("hello world"));
    }

    // --- follow-up enhancements: fuzzy names, status filter, cap notes -------

    #[test]
    fn normalize_channel_name_is_loose() {
        let want = normalize_channel_name("database");
        assert_eq!(normalize_channel_name("🗄-database"), want);
        assert_eq!(normalize_channel_name("Database"), want);
        assert_eq!(normalize_channel_name("DATA_BASE"), want); // underscores dropped
                                                               // Different words stay distinct.
        assert_ne!(normalize_channel_name("databases"), want);
        // Non-ASCII letters are preserved (not collapsed to empty), so distinct
        // non-Latin names remain distinct.
        assert!(!normalize_channel_name("日本語").is_empty());
        assert_ne!(
            normalize_channel_name("日本語"),
            normalize_channel_name("中文")
        );
    }

    #[test]
    fn cap_note_reports_truncation_and_cap() {
        // Truncated: showing fewer than available.
        assert!(cap_note(10, 3, Some(3), 50).contains("showing 3 of 10"));
        // Requested beyond the cap.
        assert!(cap_note(2, 2, Some(100), 50).contains("max_limit=50"));
        // Nothing capped.
        assert_eq!(cap_note(2, 2, Some(2), 50), "");
    }

    #[tokio::test]
    async fn helpscout_list_forwards_status_filter() {
        let cfg = McpSearchServerConfig {
            enabled: true,
            expose_helpscout: true,
            ..Default::default()
        };
        let source = FakeHelpScout::new(vec![hs_issue("HS-1", "x", "closed", "a@x.com", 1)]);
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = McpContext {
            cfg: &cfg,
            code_search: None,
            discord_search: None,
            tracker: &tracker,
            cf_verifier: None,
            helpscout: Some(&source),
            discord_lister: None,
            discord_guild_id: None,
        };
        let resp = post(
            &ctx,
            json!({ "id": 1, "method": "tools/call",
                    "params": { "name": "helpscout_list_conversations", "arguments": { "status": "all" } } }),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"].get("isError"), None);
        assert_eq!(
            source.last_status.lock().unwrap().as_deref(),
            Some("all"),
            "status not forwarded"
        );
    }

    #[tokio::test]
    async fn discord_search_resolves_channel_name_loosely() {
        // Seed the indexed-channel registry with an emoji/case channel name.
        let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        tracker
            .upsert_discord_channel(
                "999",
                Some("guild1"),
                None,
                Some("🗄-Database"),
                Some(0),
                claudear_core::types::DiscordChannelKind::Channel,
                false,
            )
            .unwrap();
        let cfg = cfg(false, true);
        let ctx = McpContext {
            cfg: &cfg,
            code_search: None,
            discord_search: None,
            tracker: tracker.as_ref(),
            cf_verifier: None,
            helpscout: None,
            discord_lister: None,
            discord_guild_id: None,
        };

        // Loose name match resolves to the channel id.
        assert_eq!(
            resolve_search_channel(&ctx, &json!({ "channel": "database" })).unwrap(),
            Some("999".to_string())
        );
        // channel_id passed through as-is.
        assert_eq!(
            resolve_search_channel(&ctx, &json!({ "channel_id": "42" })).unwrap(),
            Some("42".to_string())
        );
        // Unknown name errors rather than silently searching everything.
        assert!(resolve_search_channel(&ctx, &json!({ "channel": "nope" })).is_err());
        // No scope -> None.
        assert_eq!(resolve_search_channel(&ctx, &json!({})).unwrap(), None);

        // Exact name wins over a looser collision. Seed both "foo-bar" (id 1)
        // and "foobar" (id 2); the registry orders by name so "foo-bar" is first.
        // Request the EXACT "foobar": a loose-only resolver would wrongly return
        // the first normalized match ("foo-bar", id 1), so only exact-before-loose
        // yields id 2 — making this fail without the exact step.
        for (id, name) in [("1", "foo-bar"), ("2", "foobar")] {
            tracker
                .upsert_discord_channel(
                    id,
                    Some("guild1"),
                    None,
                    Some(name),
                    Some(0),
                    claudear_core::types::DiscordChannelKind::Channel,
                    false,
                )
                .unwrap();
        }
        assert_eq!(
            resolve_search_channel(&ctx, &json!({ "channel": "foobar" })).unwrap(),
            Some("2".to_string())
        );
    }

    #[tokio::test]
    async fn discord_list_resolves_emoji_channel_name() {
        let cfg = cfg(false, true);
        let lister = FakeDiscord {
            guild_id: "guild1".to_string(),
            by_channel: std::collections::HashMap::from([(
                "300".to_string(),
                vec![dmsg("1", "300", "alice", "db migration notes")],
            )]),
            channels: vec![dchan("300", "🗄-database")],
        };
        let tracker = SqliteTracker::in_memory().unwrap();
        let ctx = McpContext {
            cfg: &cfg,
            code_search: None,
            discord_search: None,
            tracker: &tracker,
            cf_verifier: None,
            helpscout: None,
            discord_lister: Some(&lister),
            discord_guild_id: Some("guild1"),
        };
        // "database" loosely matches the "🗄-database" channel.
        let resp = post(
            &ctx,
            json!({ "id": 1, "method": "tools/call",
                    "params": { "name": "discord_list_messages", "arguments": { "channel": "database" } } }),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"].get("isError"), None);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("db migration notes"));
    }
}
