//! Core foundation types for the claudear application.
//!
//! This crate provides the shared types, error handling, secret management,
//! and template rendering used across all claudear crates.

pub mod error;
pub mod platform;
pub mod secret;
pub mod security;
pub mod templates;
pub mod tls;
pub mod types;

pub use error::{Error, Result};
pub use secret::SecretValue;
