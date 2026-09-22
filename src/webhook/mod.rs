//! Webhook handlers and HTTP server.
//!
//! Handlers and types are provided by `claudear_integrations::webhook`.
//! The webhook server (which orchestrates processing) lives here.

pub mod self_test;
mod server;

// Re-export everything from the integrations crate's webhook module
pub use claudear_integrations::webhook::*;

// Export the server (which depends on the processing module in this crate)
pub use server::WebhookServer;
