//! API route handlers for the chat feature.
//!
//! Every route reads the signed-in user from a [`ChatViewer`] request
//! extension, which the layer mounting this router must insert; without it a
//! request fails rather than running unscoped. Chat sessions belong to the
//! user who created them:
//!
//! - Only the owner lists, reads or attaches the websocket to an owned
//!   session. Admins cannot read other users' history.
//! - The owner, or any admin, may delete a session.
//! - Sessions from before sessions had owners are listed, read, attached to
//!   and deleted only by admins.
//! - A session the caller may not read or delete answers 404, never 403,
//!   exactly as a missing one does, so its existence does not leak; over the
//!   websocket both get the same error chunk saying the session was not found.
//! - Only the server names sessions: a websocket message without a session id
//!   starts a new session under a server-generated id, owned by the caller,
//!   and one with an id only continues an existing session; it never creates
//!   one.
//! - Each read, append and delete applies the caller's scope in the same SQL
//!   statement, so no ownership check can go stale before the act.

use super::service::{ChatActor, ChatService, SessionTarget};
use super::types::*;
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Extension, Json, Router,
};
use claudear_core::types::ChatViewer;
use claudear_storage::FixAttemptTracker;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const HISTORY_LIMIT: usize = 100;

/// Shared state for chat API handlers.
#[derive(Clone)]
pub struct ChatState {
    pub chat_service: Arc<ChatService>,
    pub tracker: Arc<dyn FixAttemptTracker>,
}

/// Create the chat API router.
pub fn create_chat_router(state: ChatState) -> Router {
    Router::new()
        .route("/api/chat/ws", get(chat_ws_handler))
        .route("/api/chat/models", get(list_models_handler))
        .route("/api/chat/sessions", get(list_sessions_handler))
        .route(
            "/api/chat/sessions/{id}",
            get(get_session_handler).delete(delete_session_handler),
        )
        .with_state(state)
}

/// WebSocket handler for streaming chat.
async fn chat_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ChatState>,
    Extension(viewer): Extension<ChatViewer>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_chat_ws(socket, state, viewer))
}

/// Handle a WebSocket connection for chat.
async fn handle_chat_ws(mut socket: WebSocket, state: ChatState, viewer: ChatViewer) {
    loop {
        let msg = match socket.recv().await {
            Some(Ok(m)) => m,
            _ => break,
        };

        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };

        // Parse as tagged WsClientMessage; fall back to legacy ChatRequest
        let request = match serde_json::from_str::<WsClientMessage>(&text) {
            Ok(WsClientMessage::Chat { request }) => request,
            Ok(WsClientMessage::Stop {}) => {
                // No active generation — ignore
                continue;
            }
            Err(_) => {
                // Try legacy untagged ChatRequest for backward compatibility
                match serde_json::from_str::<ChatRequest>(&text) {
                    Ok(r) => r,
                    Err(e) => {
                        let error_chunk = ChatChunk {
                            delta: String::new(),
                            done: true,
                            sources: None,
                            session_id: None,
                            error: Some(format!("Invalid request: {e}")),
                            stopped: None,
                        };
                        let _ = socket
                            .send(Message::Text(
                                serde_json::to_string(&error_chunk).unwrap().into(),
                            ))
                            .await;
                        continue;
                    }
                }
            }
        };

        let mut target = SessionTarget::requested(request.session_id);
        let created_session_id = match &target {
            SessionTarget::New(id) => Some(id.clone()),
            SessionTarget::Existing(_) => None,
        };

        // Prepare the chat (session, user msg, RAG, prompt, LLM)
        let prepared = match state
            .chat_service
            .prepare_chat(
                ChatActor::Viewer(&viewer),
                &mut target,
                &request.message,
                request.repo_id,
                request.params.as_ref(),
            )
            .await
        {
            Ok(p) => p,
            Err(e) => {
                let error_chunk = ChatChunk {
                    delta: String::new(),
                    done: true,
                    sources: None,
                    session_id: created_session_id,
                    error: Some(e.to_string()),
                    stopped: None,
                };
                let _ = socket
                    .send(Message::Text(
                        serde_json::to_string(&error_chunk).unwrap().into(),
                    ))
                    .await;
                continue;
            }
        };

        let session_id = prepared.session_id.clone();

        // Set up streaming channel and cancel flag
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(32);
        let cancel = Arc::new(AtomicBool::new(false));

        // Spawn LLM generation on a blocking thread
        let llm = prepared.llm.clone();
        let prompt = prepared.prompt.clone();
        let gen_params = prepared.gen_params.clone();
        let cancel_clone = cancel.clone();
        let gen_handle = tokio::task::spawn_blocking(move || {
            llm.complete_streaming_channel(&prompt, &gen_params, tx, cancel_clone)
        });

        // Stream tokens to client via select!
        let mut accumulated = String::new();
        let mut first_chunk = true;
        let mut was_stopped = false;
        let mut ws_broken = false;

        loop {
            tokio::select! {
                token = rx.recv() => {
                    match token {
                        Some(tok) => {
                            accumulated.push_str(&tok);
                            let chunk = ChatChunk {
                                delta: tok,
                                done: false,
                                sources: None,
                                session_id: if first_chunk { Some(session_id.clone()) } else { None },
                                error: None,
                                stopped: None,
                            };
                            first_chunk = false;

                            if socket
                                .send(Message::Text(
                                    serde_json::to_string(&chunk).unwrap().into(),
                                ))
                                .await
                                .is_err()
                            {
                                cancel.store(true, Ordering::Relaxed);
                                ws_broken = true;
                                break;
                            }
                        }
                        None => {
                            // Channel closed — generation finished
                            break;
                        }
                    }
                }
                ws_msg = socket.recv() => {
                    match ws_msg {
                        Some(Ok(Message::Text(t))) => {
                            if let Ok(WsClientMessage::Stop {}) = serde_json::from_str::<WsClientMessage>(&t) {
                                cancel.store(true, Ordering::Relaxed);
                                was_stopped = true;
                                // Don't break — keep draining rx until channel closes
                            }
                            // Ignore other messages during streaming
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            cancel.store(true, Ordering::Relaxed);
                            ws_broken = true;
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        // Drain remaining tokens from channel after cancel/finish
        while let Ok(tok) = rx.try_recv() {
            accumulated.push_str(&tok);
        }

        // Wait for the blocking task to complete
        let _ = gen_handle.await;

        if ws_broken {
            // Still save whatever was generated
            let _ = state
                .chat_service
                .save_assistant_message(&prepared, &accumulated);
            break;
        }

        // Send final done chunk
        let done_chunk = ChatChunk {
            delta: String::new(),
            done: true,
            sources: Some(prepared.sources.clone()),
            session_id: if first_chunk {
                Some(session_id.clone())
            } else {
                None
            },
            error: None,
            stopped: if was_stopped { Some(true) } else { None },
        };
        let _ = socket
            .send(Message::Text(
                serde_json::to_string(&done_chunk).unwrap().into(),
            ))
            .await;

        // Save the (possibly partial) response
        let _ = state
            .chat_service
            .save_assistant_message(&prepared, &accumulated);
    }
}

/// List available models.
async fn list_models_handler(State(state): State<ChatState>) -> Json<ModelsResponse> {
    // Check if a download is in progress
    let (status, download_progress) = if let Some((downloaded, total, completed, failed)) =
        state.chat_service.download_status()
    {
        if completed {
            (ModelStatus::NotLoaded, None)
        } else if failed {
            (ModelStatus::Error, None)
        } else {
            let pct = if total > 0 {
                ((downloaded as f64 / total as f64) * 100.0).min(100.0) as u8
            } else {
                0
            };
            (ModelStatus::Downloading, Some(pct))
        }
    } else if state.chat_service.is_model_loaded() {
        (ModelStatus::Ready, None)
    } else if state.chat_service.is_model_available() {
        (ModelStatus::NotLoaded, None)
    } else {
        (ModelStatus::Error, None)
    };

    Json(ModelsResponse {
        models: vec![ModelInfo {
            name: state.chat_service.model_name(),
            path: String::new(), // Don't expose filesystem paths
            status,
            context_length: state.chat_service.context_length(),
            download_progress,
        }],
    })
}

/// List the caller's chat sessions, and for an admin the ownerless ones.
async fn list_sessions_handler(
    State(state): State<ChatState>,
    Extension(viewer): Extension<ChatViewer>,
) -> Result<Json<Vec<ChatSession>>, StatusCode> {
    state
        .tracker
        .list_chat_sessions(&viewer.scope())
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Get a session the caller may read, with its message history.
async fn get_session_handler(
    Path(id): Path<String>,
    State(state): State<ChatState>,
    Extension(viewer): Extension<ChatViewer>,
) -> Result<Json<ChatSession>, StatusCode> {
    let scope = viewer.scope();
    let session = state
        .tracker
        .get_chat_session(&id, &scope)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let messages = state
        .tracker
        .get_chat_history(&id, &scope, HISTORY_LIMIT)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ChatSession {
        messages,
        ..session
    }))
}

/// Delete a session the caller may delete.
async fn delete_session_handler(
    Path(id): Path<String>,
    State(state): State<ChatState>,
    Extension(viewer): Extension<ChatViewer>,
) -> StatusCode {
    match state
        .tracker
        .delete_chat_session(&id, &viewer.delete_scope())
    {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_chunk_serialization_minimal() {
        let chunk = ChatChunk {
            delta: "Hello".to_string(),
            done: false,
            sources: None,
            session_id: None,
            error: None,
            stopped: None,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("\"delta\":\"Hello\""));
        assert!(json.contains("\"done\":false"));
        // skip_serializing_if = "Option::is_none" should omit None fields
        assert!(!json.contains("sources"));
        assert!(!json.contains("session_id"));
        assert!(!json.contains("error"));
    }

    #[test]
    fn chat_chunk_serialization_with_all_fields() {
        let chunk = ChatChunk {
            delta: "world".to_string(),
            done: true,
            sources: Some(vec![ChatSource {
                file_path: "src/main.rs".to_string(),
                start_line: 1,
                end_line: 10,
                symbol_name: Some("main".to_string()),
                similarity: 0.95,
            }]),
            session_id: Some("sess-123".to_string()),
            error: Some("test error".to_string()),
            stopped: None,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("\"done\":true"));
        assert!(json.contains("\"session_id\":\"sess-123\""));
        assert!(json.contains("\"error\":\"test error\""));
        assert!(json.contains("\"file_path\":\"src/main.rs\""));
        assert!(json.contains("\"symbol_name\":\"main\""));
    }

    #[test]
    fn chat_chunk_error_only() {
        let chunk = ChatChunk {
            delta: String::new(),
            done: true,
            sources: None,
            session_id: Some("sess-err".to_string()),
            error: Some("Something went wrong".to_string()),
            stopped: None,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("\"error\":\"Something went wrong\""));
        assert!(json.contains("\"delta\":\"\""));
        assert!(json.contains("\"done\":true"));
    }

    #[test]
    fn chat_request_deserialization_minimal() {
        let json = r#"{"message": "Hello"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "Hello");
        assert!(req.session_id.is_none());
        assert!(req.repo_id.is_none());
        assert!(req.max_context_chunks.is_none());
        assert!(req.params.is_none());
    }

    #[test]
    fn chat_request_deserialization_full() {
        let json = r#"{
            "session_id": "sess-1",
            "message": "Explain the code",
            "repo_id": 42,
            "max_context_chunks": 5,
            "params": {
                "max_tokens": 1024,
                "temperature": 0.5,
                "top_p": 0.8
            }
        }"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.session_id, Some("sess-1".to_string()));
        assert_eq!(req.message, "Explain the code");
        assert_eq!(req.repo_id, Some(42));
        assert_eq!(req.max_context_chunks, Some(5));
        let params = req.params.unwrap();
        assert_eq!(params.max_tokens, Some(1024));
        assert!((params.temperature.unwrap() - 0.5).abs() < f32::EPSILON);
        assert!((params.top_p.unwrap() - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn chat_request_invalid_json_errors() {
        let json = r#"{"not_a_message": "oops"}"#;
        let result: Result<ChatRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn chat_source_serialization() {
        let source = ChatSource {
            file_path: "src/lib.rs".to_string(),
            start_line: 10,
            end_line: 20,
            symbol_name: None,
            similarity: 0.88,
        };
        let json = serde_json::to_string(&source).unwrap();
        assert!(json.contains("\"file_path\":\"src/lib.rs\""));
        assert!(json.contains("\"start_line\":10"));
        assert!(json.contains("\"end_line\":20"));
        // symbol_name is None with skip_serializing_if
        assert!(!json.contains("symbol_name"));
    }

    #[test]
    fn chat_source_deserialization() {
        let json = r#"{
            "file_path": "src/main.rs",
            "start_line": 1,
            "end_line": 5,
            "symbol_name": "process",
            "similarity": 0.75
        }"#;
        let source: ChatSource = serde_json::from_str(json).unwrap();
        assert_eq!(source.file_path, "src/main.rs");
        assert_eq!(source.start_line, 1);
        assert_eq!(source.end_line, 5);
        assert_eq!(source.symbol_name, Some("process".to_string()));
        assert!((source.similarity - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn generation_params_override_partial() {
        let json = r#"{"max_tokens": 512}"#;
        let params: GenerationParamsOverride = serde_json::from_str(json).unwrap();
        assert_eq!(params.max_tokens, Some(512));
        assert!(params.temperature.is_none());
        assert!(params.top_p.is_none());
    }

    #[test]
    fn generation_params_override_all_fields() {
        let json = r#"{"max_tokens": 2048, "temperature": 0.9, "top_p": 0.95}"#;
        let params: GenerationParamsOverride = serde_json::from_str(json).unwrap();
        assert_eq!(params.max_tokens, Some(2048));
        assert!((params.temperature.unwrap() - 0.9).abs() < f32::EPSILON);
        assert!((params.top_p.unwrap() - 0.95).abs() < f32::EPSILON);
    }

    #[test]
    fn generation_params_override_empty() {
        let json = r#"{}"#;
        let params: GenerationParamsOverride = serde_json::from_str(json).unwrap();
        assert!(params.max_tokens.is_none());
        assert!(params.temperature.is_none());
        assert!(params.top_p.is_none());
    }

    #[test]
    fn models_response_serialization() {
        let resp = ModelsResponse {
            models: vec![ModelInfo {
                name: "test-model".to_string(),
                path: String::new(),
                status: ModelStatus::Ready,
                context_length: 4096,
                download_progress: None,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"name\":\"test-model\""));
        assert!(json.contains("\"status\":\"ready\""));
        assert!(json.contains("\"context_length\":4096"));
    }

    #[test]
    fn models_response_serialization_not_loaded() {
        let resp = ModelsResponse {
            models: vec![ModelInfo {
                name: "model".to_string(),
                path: String::new(),
                status: ModelStatus::NotLoaded,
                context_length: 2048,
                download_progress: None,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"notloaded\""));
    }

    #[test]
    fn models_response_serialization_error_status() {
        let resp = ModelsResponse {
            models: vec![ModelInfo {
                name: "model".to_string(),
                path: String::new(),
                status: ModelStatus::Error,
                context_length: 0,
                download_progress: None,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"error\""));
    }

    #[test]
    fn models_response_serialization_loading() {
        let resp = ModelsResponse {
            models: vec![ModelInfo {
                name: "model".to_string(),
                path: String::new(),
                status: ModelStatus::Loading,
                context_length: 8192,
                download_progress: None,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"loading\""));
    }

    #[test]
    fn chat_session_serialization() {
        let session = ChatSession {
            id: "sess-abc".to_string(),
            repo_id: Some(1),
            owner_user_id: Some(7),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            messages: vec![],
        };
        let json = serde_json::to_string(&session).unwrap();
        assert!(json.contains("\"id\":\"sess-abc\""));
        assert!(json.contains("\"repo_id\":1"));
        assert!(json.contains("\"owner_user_id\":7"));
    }

    #[test]
    fn chat_session_deserialization_with_messages() {
        let json = r#"{
            "id": "sess-1",
            "repo_id": null,
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T01:00:00Z",
            "messages": [
                {
                    "id": 1,
                    "role": "user",
                    "content": "Hello",
                    "created_at": "2024-01-01T00:00:00Z"
                },
                {
                    "id": 2,
                    "role": "assistant",
                    "content": "Hi!",
                    "created_at": "2024-01-01T00:00:01Z"
                }
            ]
        }"#;
        let session: ChatSession = serde_json::from_str(json).unwrap();
        assert_eq!(session.id, "sess-1");
        assert!(session.repo_id.is_none());
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, ChatRole::User);
        assert_eq!(session.messages[0].content, "Hello");
        assert_eq!(session.messages[1].role, ChatRole::Assistant);
    }

    #[test]
    fn chat_message_serialization() {
        let msg = ChatMessage {
            id: Some(42),
            role: ChatRole::User,
            content: "What is this function?".to_string(),
            sources_json: Some(r#"[{"file": "main.rs"}]"#.to_string()),
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"What is this function?\""));
        assert!(json.contains("sources_json"));
    }

    #[test]
    fn chat_message_without_sources() {
        let msg = ChatMessage {
            id: None,
            role: ChatRole::Assistant,
            content: "Here is the explanation".to_string(),
            sources_json: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"assistant\""));
        // sources_json has skip_serializing_if = "Option::is_none"
        assert!(!json.contains("sources_json"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    #[ignore = "requires embedding model download"]
    fn create_chat_router_constructs_successfully() {
        use claudear_analysis::feedback::EmbeddingClient;
        use claudear_analysis::repo::code_index::CodeSearchService;
        use claudear_config::config::{ChatConfig, LlmModelConfig};
        use claudear_storage::SqliteTracker;

        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(SqliteTracker::in_memory().unwrap());
        let config = ChatConfig::default();
        let llm_config = LlmModelConfig::default();
        let emb_client = Arc::new(EmbeddingClient::new(Default::default()).unwrap());
        let search = CodeSearchService::new(tracker.clone(), emb_client);
        let chat_service = Arc::new(ChatService::new(
            config,
            llm_config,
            search,
            tracker.clone(),
        ));

        let state = ChatState {
            chat_service,
            tracker,
        };

        let _router = create_chat_router(state);
        // If we got here without panicking, the router was created successfully
    }
}
