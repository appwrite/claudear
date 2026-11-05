//! Local code chat with RAG-powered LLM inference.
//!
//! Combines the existing code indexing pipeline (tree-sitter + fastembed + vectorlite)
//! with local LLM inference (llama-cpp-2) to provide a conversational code assistant.

pub mod llm;
pub mod models;
pub mod prompt;
pub mod routes;
pub mod service;
pub mod types;

pub use routes::{create_chat_router, ChatState};
pub use service::{ChatActor, ChatService, SessionTarget, CHAT_SESSION_NOT_FOUND};
pub use types::*;
