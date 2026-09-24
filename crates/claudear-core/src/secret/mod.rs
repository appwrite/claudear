//! Secret value wrapper type for secure handling of sensitive strings.
//!
//! `SecretValue` provides:
//! - Redacted `Debug` and `Display` — prints `[REDACTED]` instead of the actual value
//! - Zeroize-on-drop — securely clears memory when dropped
//! - Explicit access via `.expose()` — forces conscious decision to reveal the secret
//! - Serde support — deserializes/serializes as a plain string for TOML round-tripping
//! - Constant-time equality — uses `subtle::ConstantTimeEq` to prevent timing attacks
//! - Encryption at rest — AES-256-GCM encryption with `ENC[v1:...]` format
//! - Redaction — [`Redactor`] masks credentials in untrusted text before it
//!   leaves Claudear

pub mod encryption;
mod redactor;

pub use redactor::Redactor;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// What every redacted secret is replaced with.
pub const REDACTED: &str = "[REDACTED]";

/// A wrapper around a secret string that provides secure handling.
///
/// Secrets are redacted in debug/display output, zeroized on drop,
/// and require explicit `.expose()` to access the inner value.
#[derive(Clone)]
pub struct SecretValue {
    inner: String,
}

impl SecretValue {
    /// Create a new `SecretValue` from a string.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            inner: value.into(),
        }
    }

    /// Expose the secret value as a string slice.
    ///
    /// This is the only way to access the inner value and should be
    /// called only at the point of use (HTTP headers, HMAC, etc.).
    pub fn expose(&self) -> &str {
        &self.inner
    }

    /// Returns `true` if the secret is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.inner.zeroize();
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

impl std::fmt::Display for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

impl PartialEq for SecretValue {
    fn eq(&self, other: &Self) -> bool {
        self.inner.as_bytes().ct_eq(other.inner.as_bytes()).into()
    }
}

impl Eq for SecretValue {}

impl From<String> for SecretValue {
    fn from(s: String) -> Self {
        SecretValue::new(s)
    }
}

impl From<&str> for SecretValue {
    fn from(s: &str) -> Self {
        SecretValue::new(s)
    }
}

impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(SecretValue::new(s))
    }
}

impl Serialize for SecretValue {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.inner.serialize(serializer)
    }
}

/// Extension trait for `Option<SecretValue>` to mirror common `Option<String>` patterns.
pub trait OptionalSecretExt {
    /// Expose the inner secret as `Option<&str>`, analogous to `Option<String>::as_deref()`.
    fn expose_as_deref(&self) -> Option<&str>;
}

impl OptionalSecretExt for Option<SecretValue> {
    fn expose_as_deref(&self) -> Option<&str> {
        self.as_ref().map(|s| s.expose())
    }
}

/// Known secret prefixes/patterns to redact.
const SECRET_PATTERNS: &[&str] = &[
    "ghp_",        // GitHub personal access token
    "gho_",        // GitHub OAuth token, as `gh auth token` prints
    "ghr_",        // GitHub refresh token
    "ghs_",        // GitHub App server-to-server token
    "ghu_",        // GitHub App user-to-server token
    "github_pat_", // GitHub fine-grained PAT
    "glpat-",      // GitLab personal access token
    "lin_api_",    // Linear API key
    "xoxb-",       // Slack bot token
    "xoxp-",       // Slack user token
    "xoxa-",       // Slack app token
    "xoxr-",       // Slack refresh token
    "sntryu_",     // Sentry user token
    "sntrys_",     // Sentry system token
    "sk-ant-",     // Anthropic API key or OAuth token
    "-----BEGIN",  // PEM private key
    "ENC[v1:",     // Encrypted secret prefix
];

/// Redact known secret patterns from a string.
///
/// Replaces any occurrence of a known secret prefix (plus the characters up to
/// the next delimiter) with [`REDACTED`]. [`Redactor`] applies it after its own
/// rules.
pub fn redact_secrets(input: &str) -> String {
    let mut output = input.to_string();
    for pattern in SECRET_PATTERNS {
        while let Some(start) = output.find(pattern) {
            let after_pattern = start + pattern.len();
            let end = output[after_pattern..]
                .find(|c: char| {
                    c.is_whitespace()
                        || c == '"'
                        || c == '\''
                        || c == ','
                        || c == '}'
                        || c == ')'
                        || c == ']'
                })
                .map(|pos| after_pattern + pos)
                .unwrap_or(output.len());
            output.replace_range(start..end, REDACTED);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_redacted() {
        let secret = SecretValue::new("my-secret-token");
        assert_eq!(format!("{:?}", secret), "[REDACTED]");
    }

    #[test]
    fn test_display_redacted() {
        let secret = SecretValue::new("my-secret-token");
        assert_eq!(format!("{}", secret), "[REDACTED]");
    }

    #[test]
    fn test_expose() {
        let secret = SecretValue::new("my-secret-token");
        assert_eq!(secret.expose(), "my-secret-token");
    }

    #[test]
    fn test_is_empty() {
        assert!(SecretValue::new("").is_empty());
        assert!(!SecretValue::new("secret").is_empty());
    }

    #[test]
    fn test_constant_time_eq() {
        let a = SecretValue::new("secret");
        let b = SecretValue::new("secret");
        let c = SecretValue::new("different");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_serde_roundtrip() {
        #[derive(Serialize, Deserialize)]
        struct TestConfig {
            token: SecretValue,
        }

        let toml_str = r#"token = "my-token""#;
        let config: TestConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.token.expose(), "my-token");

        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.contains("my-token"));
    }

    #[test]
    fn test_optional_secret_ext() {
        let some: Option<SecretValue> = Some(SecretValue::new("token"));
        let none: Option<SecretValue> = None;

        assert_eq!(some.expose_as_deref(), Some("token"));
        assert_eq!(none.expose_as_deref(), None);
    }

    #[test]
    fn test_clone() {
        let original = SecretValue::new("secret");
        let cloned = original.clone();
        assert_eq!(cloned.expose(), "secret");
    }

    #[test]
    fn test_redact_github_token() {
        let input = "Authorization: Bearer ghp_abc123XYZ456";
        let output = redact_secrets(input);
        assert_eq!(output, "Authorization: Bearer [REDACTED]");
        assert!(!output.contains("ghp_"));
    }

    #[test]
    fn test_redact_gitlab_token() {
        let input = "token = \"glpat-mytoken123\"";
        let output = redact_secrets(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("glpat-"));
    }

    #[test]
    fn test_redact_slack_token() {
        let input = "Bot token: xoxb-123-456-abc";
        let output = redact_secrets(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("xoxb-"));
    }

    #[test]
    fn test_redact_multiple_secrets() {
        let input = "github=ghp_tok1 slack=xoxb-tok2";
        let output = redact_secrets(input);
        assert_eq!(output, "github=[REDACTED] slack=[REDACTED]");
    }

    #[test]
    fn test_redact_no_secrets() {
        let input = "No secrets in this log message";
        let output = redact_secrets(input);
        assert_eq!(output, input);
    }

    #[test]
    fn test_redact_pem_key() {
        let input = "key: -----BEGIN RSA PRIVATE KEY-----";
        let output = redact_secrets(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("-----BEGIN"));
    }

    #[test]
    fn test_redact_encrypted_prefix() {
        let input = "field = \"ENC[v1:abc123def456]\"";
        let output = redact_secrets(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("ENC[v1:"));
    }
}
