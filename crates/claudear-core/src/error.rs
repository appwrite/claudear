//! Error types for the claudear application.

use abnegate_http::Error as HttpError;
use thiserror::Error;

/// Main error type for the application.
#[derive(Error, Debug)]
pub enum Error {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Source error ({source_name}): {message}")]
    Source {
        source_name: String,
        message: String,
    },

    #[error("Webhook error: {0}")]
    Webhook(String),

    #[error("Claude runner error: {0}")]
    Runner(String),

    #[error("Notifier error ({notifier}): {message}")]
    Notifier { notifier: String, message: String },

    #[error("Issue not found: {source_name}:{issue_id}")]
    IssueNotFound {
        source_name: String,
        issue_id: String,
    },

    #[error("Invalid signature")]
    InvalidSignature,

    #[error("Network error: {0}")]
    Network(String),

    #[error("API error: {0}")]
    Api(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Git error: {0}")]
    Git(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn source(source_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Source {
            source_name: source_name.into(),
            message: message.into(),
        }
    }

    pub fn webhook(message: impl Into<String>) -> Self {
        Self::Webhook(message.into())
    }

    pub fn runner(message: impl Into<String>) -> Self {
        Self::Runner(message.into())
    }

    pub fn notifier(notifier: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Notifier {
            notifier: notifier.into(),
            message: message.into(),
        }
    }

    pub fn issue_not_found(source_name: impl Into<String>, issue_id: impl Into<String>) -> Self {
        Self::IssueNotFound {
            source_name: source_name.into(),
            issue_id: issue_id.into(),
        }
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::Network(message.into())
    }

    pub fn api(message: impl Into<String>) -> Self {
        Self::Api(message.into())
    }

    pub fn storage(message: impl Into<String>) -> Self {
        Self::Storage(message.into())
    }

    pub fn git(message: impl Into<String>) -> Self {
        Self::Git(message.into())
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::Io(std::io::Error::other(message.into()))
    }

    pub fn database(message: impl Into<String>) -> Self {
        Self::Database(message.into())
    }
}

#[cfg(feature = "sqlite")]
impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Error::Database(error.to_string())
    }
}

impl From<HttpError> for Error {
    fn from(error: HttpError) -> Self {
        match error {
            HttpError::Request(source) | HttpError::UnreadableBody(source) => Self::Http(source),
            HttpError::TooManyRedirects => Self::Network(error.to_string()),
            _ => Self::Other(error.to_string()),
        }
    }
}

/// Result type alias for the application.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_config() {
        let err = Error::config("missing variable");
        assert!(matches!(err, Error::Config(_)));
        assert_eq!(err.to_string(), "Configuration error: missing variable");
    }

    #[test]
    fn test_error_source() {
        let err = Error::source("linear", "API rate limit");
        assert!(matches!(err, Error::Source { .. }));
        assert_eq!(err.to_string(), "Source error (linear): API rate limit");
    }

    #[test]
    fn test_error_webhook() {
        let err = Error::webhook("invalid payload");
        assert!(matches!(err, Error::Webhook(_)));
        assert_eq!(err.to_string(), "Webhook error: invalid payload");
    }

    #[test]
    fn test_error_runner() {
        let err = Error::runner("process crashed");
        assert!(matches!(err, Error::Runner(_)));
        assert_eq!(err.to_string(), "Claude runner error: process crashed");
    }

    #[test]
    fn test_error_notifier() {
        let err = Error::notifier("discord", "rate limited");
        assert!(matches!(err, Error::Notifier { .. }));
        assert_eq!(err.to_string(), "Notifier error (discord): rate limited");
    }

    #[test]
    fn test_error_issue_not_found() {
        let err = Error::issue_not_found("sentry", "12345");
        assert!(matches!(err, Error::IssueNotFound { .. }));
        assert_eq!(err.to_string(), "Issue not found: sentry:12345");
    }

    #[test]
    fn test_error_network() {
        let err = Error::network("connection refused");
        assert!(matches!(err, Error::Network(_)));
        assert_eq!(err.to_string(), "Network error: connection refused");
    }

    #[test]
    fn test_error_api() {
        let err = Error::api("rate limited");
        assert!(matches!(err, Error::Api(_)));
        assert_eq!(err.to_string(), "API error: rate limited");
    }

    #[test]
    fn test_error_invalid_signature() {
        let err = Error::InvalidSignature;
        assert!(matches!(err, Error::InvalidSignature));
        assert_eq!(err.to_string(), "Invalid signature");
    }

    #[test]
    fn test_error_other() {
        let err = Error::Other("something else".into());
        assert!(matches!(err, Error::Other(_)));
        assert_eq!(err.to_string(), "something else");
    }

    #[test]
    fn test_error_debug() {
        let err = Error::config("test");
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("Config"));
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn test_error_from_json() {
        let json_err = serde_json::from_str::<String>("not valid json").unwrap_err();
        let err: Error = json_err.into();
        assert!(matches!(err, Error::Json(_)));
    }

    #[test]
    fn test_error_config_string_conversion() {
        let err = Error::config(String::from("dynamic message"));
        assert_eq!(err.to_string(), "Configuration error: dynamic message");
    }

    #[test]
    fn test_error_config_str_conversion() {
        let err = Error::config("static message");
        assert_eq!(err.to_string(), "Configuration error: static message");
    }

    #[test]
    fn test_error_source_display_format() {
        let err = Error::source("github", "token expired");
        // Check the formatted output matches expected pattern
        let display = err.to_string();
        assert!(display.contains("github"));
        assert!(display.contains("token expired"));
    }

    #[test]
    fn test_error_notifier_display_format() {
        let err = Error::notifier("email", "SMTP connection failed");
        let display = err.to_string();
        assert!(display.contains("email"));
        assert!(display.contains("SMTP connection failed"));
    }

    #[test]
    fn test_result_type_alias() {
        fn test_fn() -> Result<i32> {
            Ok(42)
        }
        assert_eq!(test_fn().unwrap(), 42);
    }

    #[test]
    fn test_result_type_alias_error() {
        fn test_fn() -> Result<i32> {
            Err(Error::config("failed"))
        }
        assert!(test_fn().is_err());
    }

    #[test]
    fn test_error_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        // Error should be Send + Sync for use in async contexts
        // This will fail to compile if Error doesn't implement these traits
        assert_send_sync::<Error>();
    }

    #[test]
    fn test_error_source_empty_strings() {
        let err = Error::source("", "");
        assert!(err.to_string().contains("Source error"));
    }

    #[test]
    fn test_error_notifier_empty_strings() {
        let err = Error::notifier("", "");
        assert!(err.to_string().contains("Notifier error"));
    }

    #[test]
    fn test_error_issue_not_found_empty_strings() {
        let err = Error::issue_not_found("", "");
        assert!(err.to_string().contains("Issue not found"));
    }

    #[test]
    fn test_error_config_long_message() {
        let long_msg = "x".repeat(10000);
        let err = Error::config(&long_msg);
        assert_eq!(err.to_string().len(), 10000 + "Configuration error: ".len());
    }

    #[test]
    fn test_error_runner_special_chars() {
        let err = Error::runner("Error with <special> & \"chars\"");
        assert!(err.to_string().contains("<special>"));
        assert!(err.to_string().contains("&"));
    }

    #[test]
    fn test_error_webhook_unicode() {
        let err = Error::webhook("错误信息 🔥");
        assert!(err.to_string().contains("错误信息"));
    }

    #[test]
    fn test_error_network_multiline() {
        let err = Error::network("Line 1\nLine 2\nLine 3");
        assert!(err.to_string().contains("Line 1"));
    }

    #[test]
    fn test_result_map() {
        fn success_fn() -> Result<i32> {
            Ok(42)
        }

        let mapped = success_fn().map(|v| v * 2);
        assert_eq!(mapped.unwrap(), 84);
    }

    #[test]
    fn test_result_and_then() {
        fn double(x: i32) -> Result<i32> {
            Ok(x * 2)
        }

        let result: Result<i32> = Ok(21);
        let chained = result.and_then(double);
        assert_eq!(chained.unwrap(), 42);
    }

    #[test]
    fn test_error_from_database() {
        let err = Error::database("query failed");
        assert!(matches!(err, Error::Database(_)));
        assert!(err.to_string().contains("query failed"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_error_from_rusqlite() {
        let db_err = rusqlite::Error::InvalidQuery;
        let err: Error = db_err.into();
        assert!(matches!(err, Error::Database(_)));
    }

    #[test]
    fn test_error_equality_by_variant() {
        let err1 = Error::InvalidSignature;
        let err2 = Error::InvalidSignature;
        // They produce the same error message
        assert_eq!(err1.to_string(), err2.to_string());
    }

    #[test]
    fn test_error_config_from_owned_string() {
        let owned = String::from("owned message");
        let err = Error::config(owned);
        assert!(err.to_string().contains("owned message"));
    }

    #[test]
    fn test_error_all_constructors_return_error() {
        // Verify all constructors create valid Error variants
        let errors = vec![
            Error::config("test"),
            Error::source("src", "msg"),
            Error::webhook("test"),
            Error::runner("test"),
            Error::notifier("notify", "msg"),
            Error::issue_not_found("src", "id"),
            Error::network("test"),
            Error::api("test"),
            Error::InvalidSignature,
            Error::Other("test".into()),
        ];

        for err in errors {
            // All should produce non-empty error messages
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn test_error_display_vs_debug() {
        let err = Error::config("test message");
        let display = err.to_string();
        let debug = format!("{:?}", err);

        // Display is the user-friendly message
        assert!(display.contains("Configuration error"));
        // Debug contains variant name
        assert!(debug.contains("Config"));
    }

    #[test]
    #[expect(clippy::unnecessary_literal_unwrap)]
    fn test_result_unwrap_or() {
        let ok_result: Result<i32> = Ok(42);
        assert_eq!(ok_result.unwrap_or(0), 42);

        let err_result: Result<i32> = Err(Error::config("error"));
        assert_eq!(err_result.unwrap_or(0), 0);
    }

    #[test]
    fn test_result_is_ok_is_err() {
        let ok_result: Result<()> = Ok(());
        assert!(ok_result.is_ok());
        assert!(ok_result.is_ok());

        let err_result: Result<()> = Err(Error::config("error"));
        assert!(err_result.is_err());
        assert!(err_result.is_err());
    }

    #[test]
    fn test_error_source_with_newlines() {
        let err = Error::source("linear", "Error\nWith\nNewlines");
        assert!(err.to_string().contains("Error"));
    }

    #[test]
    fn test_error_notifier_various_notifiers() {
        for notifier in ["discord", "email", "sms", "push", "console"] {
            let err = Error::notifier(notifier, "test error");
            assert!(err.to_string().contains(notifier));
        }
    }

    #[test]
    fn test_error_source_various_sources() {
        for source in ["linear", "sentry", "github", "jira"] {
            let err = Error::source(source, "API error");
            assert!(err.to_string().contains(source));
        }
    }

    #[test]
    fn test_result_ok() {
        let result: Result<&str> = Ok("success");
        assert_eq!(result.ok(), Some("success"));

        let err_result: Result<&str> = Err(Error::config("error"));
        assert_eq!(err_result.ok(), None);
    }

    #[test]
    fn test_result_err() {
        let result: Result<i32> = Ok(42);
        assert!(result.err().is_none());

        let err_result: Result<i32> = Err(Error::config("error"));
        assert!(err_result.err().is_some());
    }

    #[test]
    fn test_error_invalid_signature_display() {
        let err = Error::InvalidSignature;
        assert_eq!(err.to_string(), "Invalid signature");
    }

    #[test]
    fn test_error_other_with_empty() {
        let err = Error::Other(String::new());
        assert!(err.to_string().is_empty());
    }

    #[test]
    fn test_error_chaining() {
        fn inner_op() -> Result<i32> {
            Err(Error::runner("inner failure"))
        }

        fn outer_op() -> Result<i32> {
            inner_op()
        }

        let result = outer_op();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("inner failure"));
    }

    #[test]
    fn test_error_storage() {
        let err = Error::storage("disk full");
        assert!(matches!(err, Error::Storage(_)));
        assert_eq!(err.to_string(), "Storage error: disk full");
    }

    #[test]
    fn test_error_git() {
        let err = Error::git("merge conflict");
        assert!(matches!(err, Error::Git(_)));
        assert_eq!(err.to_string(), "Git error: merge conflict");
    }

    #[test]
    fn test_error_io_helper() {
        let err = Error::io("permission denied");
        assert!(matches!(err, Error::Io(_)));
        assert!(err.to_string().contains("IO error"));
        assert!(err.to_string().contains("permission denied"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_error_from_rusqlite_query_returned_no_rows() {
        let db_err = rusqlite::Error::QueryReturnedNoRows;
        let err: Error = db_err.into();
        assert!(matches!(err, Error::Database(_)));
    }

    #[test]
    fn test_error_from_io_various_kinds() {
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::AlreadyExists,
        ] {
            let io_err = std::io::Error::new(kind, "test");
            let err: Error = io_err.into();
            assert!(matches!(err, Error::Io(_)));
        }
    }

    #[test]
    fn test_error_from_serde_json_eof() {
        let err: Error = serde_json::from_str::<serde_json::Value>("")
            .unwrap_err()
            .into();
        assert!(matches!(err, Error::Json(_)));
    }

    #[test]
    fn test_error_issue_not_found_display() {
        let err = Error::IssueNotFound {
            source_name: "sentry".to_string(),
            issue_id: "PROJ-999".to_string(),
        };
        assert_eq!(err.to_string(), "Issue not found: sentry:PROJ-999");
    }

    #[test]
    fn test_error_constructors_accept_both_str_and_string() {
        let _ = Error::config("str");
        let _ = Error::config(String::from("string"));
        let _ = Error::source("s", "m");
        let _ = Error::source(String::from("s"), String::from("m"));
        let _ = Error::storage("str");
        let _ = Error::storage(String::from("string"));
        let _ = Error::git("str");
        let _ = Error::git(String::from("string"));
        let _ = Error::io("str");
        let _ = Error::io(String::from("string"));
    }

    #[test]
    fn test_error_empty_string_variants() {
        let errors = vec![
            Error::config(""),
            Error::source("", ""),
            Error::webhook(""),
            Error::runner(""),
            Error::notifier("", ""),
            Error::issue_not_found("", ""),
            Error::network(""),
            Error::api(""),
            Error::storage(""),
            Error::git(""),
            Error::io(""),
            Error::Other(String::new()),
        ];
        for err in errors {
            let _ = err.to_string();
            let _ = format!("{:?}", err);
        }
    }

    #[test]
    fn test_error_is_std_error() {
        fn assert_std_error<T: std::error::Error>() {}
        assert_std_error::<Error>();
    }

    #[test]
    fn test_result_type_with_question_mark() {
        fn inner() -> Result<i32> {
            let value: i32 = serde_json::from_str("42")?;
            Ok(value)
        }
        assert_eq!(inner().unwrap(), 42);

        fn inner_fail() -> Result<i32> {
            let _: i32 = serde_json::from_str("invalid")?;
            Ok(0)
        }
        assert!(inner_fail().is_err());
    }

    async fn refused_connection() -> reqwest::Error {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("a client without a proxy builds")
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("nothing listens on port 1")
    }

    #[tokio::test]
    async fn a_transport_error_from_the_http_client_stays_an_http_error() {
        let error = Error::from(HttpError::from(refused_connection().await));

        assert!(matches!(error, Error::Http(_)), "{error:?}");
    }

    #[tokio::test]
    async fn an_unreadable_body_from_the_http_client_is_an_http_error() {
        let error = Error::from(HttpError::UnreadableBody(refused_connection().await));

        assert!(matches!(error, Error::Http(_)), "{error:?}");
    }

    #[test]
    fn a_redirect_loop_from_the_http_client_is_a_network_error() {
        let error = Error::from(HttpError::TooManyRedirects);

        assert!(matches!(error, Error::Network(_)), "{error:?}");
        assert_eq!(error.to_string(), "Network error: Too many redirects.");
    }

    #[test]
    fn a_json_error_from_the_http_client_keeps_its_parse_message() {
        let parse = serde_json::from_str::<serde_json::Value>("not json")
            .expect_err("the body is not JSON");

        let error = Error::from(HttpError::from(parse));

        assert!(matches!(error, Error::Other(_)), "{error:?}");
        assert!(
            error.to_string().starts_with("JSON parse error: "),
            "{error}"
        );
    }

    #[test]
    fn an_unsupported_method_from_the_http_client_is_other() {
        let error = Error::from(HttpError::Unsupported(reqwest::Method::POST));

        assert!(matches!(error, Error::Other(_)), "{error:?}");
        assert_eq!(
            error.to_string(),
            "POST is not supported by this HTTP client"
        );
    }

    #[test]
    fn an_oversized_body_from_the_http_client_is_other() {
        let error = Error::from(HttpError::oversized_body(16));

        assert!(matches!(error, Error::Other(_)), "{error:?}");
        assert_eq!(
            error.to_string(),
            "The response body is larger than 16 bytes."
        );
    }

    #[test]
    fn every_refused_destination_from_the_http_client_is_other() {
        for refusal in [
            HttpError::InvalidUrl,
            HttpError::InvalidHeaderName,
            HttpError::InvalidHeaderValue,
            HttpError::UnsupportedScheme,
            HttpError::EmbeddedCredentials,
            HttpError::MissingHost,
            HttpError::PrivateAddress,
            HttpError::InternalHost,
            HttpError::unfetchable_resolution("inside.example"),
        ] {
            let message = refusal.to_string();

            let error = Error::from(refusal);

            assert!(matches!(error, Error::Other(_)), "{error:?}");
            assert_eq!(error.to_string(), message);
        }
    }
}
