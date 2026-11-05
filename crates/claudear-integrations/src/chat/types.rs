//! Types for the local code chat feature.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::llm::LlmEngine;

pub use claudear_core::types::{ChatMessage, ChatRole, ChatSession};

/// Incoming chat request (via WebSocket).
#[derive(Debug, Clone, Deserialize)]
pub struct ChatRequest {
    /// Session ID. None = create a new session.
    pub session_id: Option<String>,
    /// The user's message.
    pub message: String,
    /// Repository to scope the search to. None = search all repos.
    pub repo_id: Option<i64>,
    /// Max code chunks to retrieve per query.
    pub max_context_chunks: Option<usize>,
    /// Generation parameter overrides.
    pub params: Option<GenerationParamsOverride>,
}

/// Client-to-server WebSocket message.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsClientMessage {
    /// A chat request from the client.
    Chat {
        #[serde(flatten)]
        request: ChatRequest,
    },
    /// Stop the current generation.
    Stop {},
}

/// Optional generation parameter overrides sent by client.
#[derive(Debug, Clone, Deserialize)]
pub struct GenerationParamsOverride {
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
}

/// A streaming response chunk sent to the client.
#[derive(Debug, Clone, Serialize)]
pub struct ChatChunk {
    /// Token text delta.
    pub delta: String,
    /// Whether this is the final chunk.
    pub done: bool,
    /// Sources attached to the final chunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<ChatSource>>,
    /// Session ID (sent on first chunk of a new session).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Error message if something went wrong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether generation was stopped by the user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped: Option<bool>,
}

/// A source reference from the code search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSource {
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    pub similarity: f32,
}

/// Parameters controlling LLM text generation.
#[derive(Debug, Clone)]
pub struct GenerationParams {
    pub max_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub stop_sequences: Vec<String>,
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            max_tokens: 2048,
            temperature: 0.7,
            top_p: 0.9,
            stop_sequences: Vec::new(),
        }
    }
}

/// Result of preparing a chat request (before LLM generation).
pub struct PreparedChat {
    pub session_id: String,
    pub scope: claudear_core::types::ChatSessionScope,
    pub prompt: String,
    pub sources: Vec<ChatSource>,
    pub gen_params: super::llm::GenerationParams,
    pub llm: Arc<LlmEngine>,
}

/// Models response for the /api/chat/models endpoint.
#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub models: Vec<ModelInfo>,
}

/// Information about an available model.
#[derive(Debug, Serialize)]
pub struct ModelInfo {
    pub name: String,
    pub path: String,
    pub status: ModelStatus,
    pub context_length: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_progress: Option<u8>,
}

/// Status of a loaded model.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ModelStatus {
    Ready,
    Loading,
    Downloading,
    NotLoaded,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_role_roundtrip() {
        assert_eq!(ChatRole::parse("user"), Some(ChatRole::User));
        assert_eq!(ChatRole::parse("assistant"), Some(ChatRole::Assistant));
        assert_eq!(ChatRole::parse("system"), None);
        assert_eq!(ChatRole::User.as_str(), "user");
        assert_eq!(ChatRole::Assistant.as_str(), "assistant");
    }

    #[test]
    fn test_chat_chunk_serialization() {
        let chunk = ChatChunk {
            delta: "Hello".into(),
            done: false,
            sources: None,
            session_id: None,
            error: None,
            stopped: None,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("\"delta\":\"Hello\""));
        assert!(!json.contains("sources"));
        assert!(!json.contains("session_id"));
        assert!(!json.contains("stopped"));
    }

    #[test]
    fn test_chat_request_deserialization() {
        let json = r#"{"message": "What does main do?", "repo_id": 1}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "What does main do?");
        assert_eq!(req.repo_id, Some(1));
        assert!(req.session_id.is_none());
    }

    #[test]
    fn test_generation_params_default() {
        let params = GenerationParams::default();
        assert_eq!(params.max_tokens, 2048);
        assert!((params.temperature - 0.7).abs() < f32::EPSILON);
        assert!((params.top_p - 0.9).abs() < f32::EPSILON);
        assert!(params.stop_sequences.is_empty());
    }

    #[test]
    fn test_ws_client_message_chat() {
        let json = r#"{"type": "chat", "message": "Hello"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::Chat { request } => {
                assert_eq!(request.message, "Hello");
                assert!(request.session_id.is_none());
            }
            WsClientMessage::Stop {} => panic!("Expected Chat variant"),
        }
    }

    #[test]
    fn test_ws_client_message_chat_with_session() {
        let json =
            r#"{"type": "chat", "message": "Explain this", "session_id": "s1", "repo_id": 5}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::Chat { request } => {
                assert_eq!(request.message, "Explain this");
                assert_eq!(request.session_id, Some("s1".to_string()));
                assert_eq!(request.repo_id, Some(5));
            }
            WsClientMessage::Stop {} => panic!("Expected Chat variant"),
        }
    }

    #[test]
    fn test_ws_client_message_stop() {
        let json = r#"{"type": "stop"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, WsClientMessage::Stop {}));
    }

    #[test]
    fn test_ws_client_message_invalid_type() {
        let json = r#"{"type": "unknown"}"#;
        let result: Result<WsClientMessage, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_chat_chunk_stopped_field_serialization() {
        let chunk = ChatChunk {
            delta: String::new(),
            done: true,
            sources: None,
            session_id: None,
            error: None,
            stopped: Some(true),
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("\"stopped\":true"));
    }
}
