//! Configuration loading, validation, and user registry for claudear.

pub mod config;
pub mod environment;
pub mod users;

pub use config::*;
pub use users::{ResolvedUser, UserRegistry};
