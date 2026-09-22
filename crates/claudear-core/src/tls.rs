//! The process-wide rustls crypto provider.
//!
//! reqwest is built with `rustls-no-provider`, so nothing installs a provider
//! on its own and constructing a client panics with "no process-level
//! CryptoProvider available" until one is. Installing it from `main` covers
//! the binary and nothing else — every unit test that builds a client runs in
//! a process that never saw `main` — so the install lives here, next to the
//! client factory that library code goes through.

use std::sync::Once;

static INSTALL: Once = Once::new();

/// Install ring as the process-wide provider, once.
///
/// Safe to call from anywhere and at any time: repeat calls are no-ops, and a
/// provider another caller installed first is left alone.
pub fn ensure_crypto_provider() {
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Build a reqwest client, with the provider installed first.
///
/// Every client in the workspace is built through here or through
/// [`client_builder`]; calling `reqwest::Client::new` directly reintroduces
/// the panic this module exists to prevent.
pub fn client() -> reqwest::Client {
    ensure_crypto_provider();
    reqwest::Client::new()
}

/// Start building a reqwest client, with the provider installed first.
pub fn client_builder() -> reqwest::ClientBuilder {
    ensure_crypto_provider();
    reqwest::Client::builder()
}
