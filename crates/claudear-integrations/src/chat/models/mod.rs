//! Model browsing, searching, and downloading for the chat feature.

pub mod download;
pub mod providers;
pub mod types;

pub use download::DownloadProgress;
pub use providers::HuggingFaceProvider;
pub use types::*;

use std::sync::LazyLock;

/// Shared HTTP client for model provider API calls.
pub static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    claudear_core::tls::client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("Failed to build HTTP client")
});
