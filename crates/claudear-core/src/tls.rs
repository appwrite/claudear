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
