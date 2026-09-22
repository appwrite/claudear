//! Engine layer for claudear.
//!
//! This crate provides the watcher, processing pipeline, API server,
//! IPC server/client, housekeeping, and retry management.

pub mod agent_classifier;
pub mod api;
pub mod api_events;
pub mod discord_index;
pub mod housekeeping;
pub mod intent;
pub mod ipc;
pub mod llm_agent_runner;
pub mod llm_analyzer;
pub mod llm_classifier;
pub mod processing;
pub mod repo_index;
pub mod retry;
pub mod watcher;
