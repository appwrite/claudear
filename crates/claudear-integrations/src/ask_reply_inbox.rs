//! Shared in-memory inbox for ask-loop reply matching across channels.
//!
//! This lets source adapters (which receive inbound user messages) and notifier
//! adapters (which poll for ask replies) share minimal state without directly
//! depending on each other's concrete types.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Normalized inbound Telegram message used for ask-reply matching.
#[derive(Debug, Clone)]
pub(crate) struct TelegramInboundMessage {
    #[expect(dead_code)]
    pub message_id: i64,
    pub chat_id: i64,
    pub responder_id: Option<String>,
    pub responder_username: Option<String>,
    pub text: String,
    pub replied_at: DateTime<Utc>,
    pub reply_to_message_id: Option<i64>,
    pub reply_to_text: Option<String>,
    pub reply_to_is_bot: Option<bool>,
}

/// Normalized inbound WhatsApp message used for ask-reply matching.
#[derive(Debug, Clone)]
pub(crate) struct WhatsAppInboundMessage {
    #[expect(dead_code)]
    pub message_id: String,
    pub from: String,
    pub text: String,
    pub replied_at: DateTime<Utc>,
    pub context_message_id: Option<String>,
}

#[derive(Default)]
struct InboxState {
    ask_delivery_ids: HashMap<String, Vec<String>>,
    ask_poll_channels: HashMap<String, String>,
    telegram_messages: Vec<TelegramInboundMessage>,
    whatsapp_messages: Vec<WhatsAppInboundMessage>,
}

fn state() -> &'static Mutex<InboxState> {
    static STATE: OnceLock<Mutex<InboxState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(InboxState::default()))
}

fn ask_key(channel: &str, correlation_id: &str) -> String {
    format!("{channel}:{correlation_id}")
}

/// Remember an outbound ask message ID for later reply matching.
pub(crate) fn remember_ask_delivery_id(channel: &str, correlation_id: &str, message_id: String) {
    if message_id.trim().is_empty() {
        return;
    }

    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    let ids = guard
        .ask_delivery_ids
        .entry(ask_key(channel, correlation_id))
        .or_default();
    if !ids.iter().any(|existing| existing == &message_id) {
        ids.push(message_id);
        if ids.len() > 32 {
            let drop_n = ids.len() - 32;
            ids.drain(0..drop_n);
        }
    }

    if guard.ask_delivery_ids.len() > 512 {
        // Simple opportunistic cleanup: remove empty entries first, then oldest-ish by key order.
        guard.ask_delivery_ids.retain(|_, v| !v.is_empty());
        while guard.ask_delivery_ids.len() > 512 {
            if let Some(key) = guard.ask_delivery_ids.keys().next().cloned() {
                guard.ask_delivery_ids.remove(&key);
            } else {
                break;
            }
        }
    }
}

/// Get remembered outbound ask message IDs for a channel/correlation pair.
pub(crate) fn ask_delivery_ids(channel: &str, correlation_id: &str) -> Vec<String> {
    let guard = state().lock().unwrap_or_else(|e| e.into_inner());
    guard
        .ask_delivery_ids
        .get(&ask_key(channel, correlation_id))
        .cloned()
        .unwrap_or_default()
}

/// Remember the actual channel ID where an ask message was delivered.
///
/// This is needed when a webhook posts to a channel that differs from the
/// configured `channel_id` (e.g. Discord webhook targets a different channel).
pub(crate) fn remember_ask_poll_channel(
    channel: &str,
    correlation_id: &str,
    poll_channel_id: String,
) {
    if poll_channel_id.trim().is_empty() {
        return;
    }
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    guard
        .ask_poll_channels
        .insert(ask_key(channel, correlation_id), poll_channel_id);
    if guard.ask_poll_channels.len() > 512 {
        let live_keys: std::collections::HashSet<String> =
            guard.ask_delivery_ids.keys().cloned().collect();
        guard.ask_poll_channels.retain(|k, _| live_keys.contains(k));
    }
}

/// Get the remembered poll channel for a channel/correlation pair.
pub(crate) fn ask_poll_channel(channel: &str, correlation_id: &str) -> Option<String> {
    let guard = state().lock().unwrap_or_else(|e| e.into_inner());
    guard
        .ask_poll_channels
        .get(&ask_key(channel, correlation_id))
        .cloned()
}

/// Record an inbound Telegram message for ask-reply matching.
pub(crate) fn record_telegram_message(message: TelegramInboundMessage) {
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    guard.telegram_messages.push(message);
    if guard.telegram_messages.len() > 2048 {
        let drop_n = guard.telegram_messages.len() - 2048;
        guard.telegram_messages.drain(0..drop_n);
    }
}

/// Snapshot recent inbound Telegram messages.
pub(crate) fn telegram_messages_since(since: DateTime<Utc>) -> Vec<TelegramInboundMessage> {
    let guard = state().lock().unwrap_or_else(|e| e.into_inner());
    guard
        .telegram_messages
        .iter()
        .filter(|m| m.replied_at >= since)
        .cloned()
        .collect()
}

/// Record an inbound WhatsApp message for ask-reply matching.
pub(crate) fn record_whatsapp_message(message: WhatsAppInboundMessage) {
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    guard.whatsapp_messages.push(message);
    if guard.whatsapp_messages.len() > 2048 {
        let drop_n = guard.whatsapp_messages.len() - 2048;
        guard.whatsapp_messages.drain(0..drop_n);
    }
}

/// Snapshot recent inbound WhatsApp messages.
pub(crate) fn whatsapp_messages_since(since: DateTime<Utc>) -> Vec<WhatsAppInboundMessage> {
    let guard = state().lock().unwrap_or_else(|e| e.into_inner());
    guard
        .whatsapp_messages
        .iter()
        .filter(|m| m.replied_at >= since)
        .cloned()
        .collect()
}

#[cfg(test)]
pub(crate) fn clear_for_tests() -> std::sync::MutexGuard<'static, ()> {
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state_guard = state().lock().unwrap_or_else(|e| e.into_inner());
    *state_guard = InboxState::default();
    guard
}
