//! Shared HTTP types used by source and client HTTP abstractions.

use crate::error::{Error, Result};
use async_trait::async_trait;

/// HTTP response abstraction for testability.
///
/// Used by source adapters (Linear, Sentry) and API clients (Discord)
/// to abstract over the actual HTTP client for unit testing.
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    /// Check if the status is successful (2xx).
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Check if the status is 404 Not Found.
    pub fn is_not_found(&self) -> bool {
        self.status == 404
    }

    /// Parse the body as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(&self.body)
            .map_err(|e| Error::Other(format!("JSON parse error: {}", e)))
    }
}

/// Trait for HTTP client operations to enable testing.
#[async_trait]
pub trait HttpClient: Send + Sync {
    /// Perform a GET request with headers.
    async fn get(&self, url: &str, headers: Vec<(&str, String)>) -> Result<HttpResponse>;

    /// Perform a POST request with headers and a body.
    async fn post(
        &self,
        url: &str,
        headers: Vec<(&str, String)>,
        body: &str,
    ) -> Result<HttpResponse> {
        let _ = (url, headers, body);
        Err(Error::Other(
            "POST not supported by this HTTP client".into(),
        ))
    }

    /// Perform a PUT request with headers and a body.
    async fn put(
        &self,
        url: &str,
        headers: Vec<(&str, String)>,
        body: &str,
    ) -> Result<HttpResponse> {
        let _ = (url, headers, body);
        Err(Error::Other("PUT not supported by this HTTP client".into()))
    }

    /// Perform a PATCH request with headers and a body.
    async fn patch(
        &self,
        url: &str,
        headers: Vec<(&str, String)>,
        body: &str,
    ) -> Result<HttpResponse> {
        let _ = (url, headers, body);
        Err(Error::Other(
            "PATCH not supported by this HTTP client".into(),
        ))
    }

    /// Perform a DELETE request with headers.
    async fn delete(&self, url: &str, headers: Vec<(&str, String)>) -> Result<HttpResponse> {
        let _ = (url, headers);
        Err(Error::Other(
            "DELETE not supported by this HTTP client".into(),
        ))
    }
}

/// Default HTTP client using reqwest.
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl ReqwestHttpClient {
    /// Create a new reqwest-based HTTP client.
    pub fn new() -> Self {
        crate::tls::ensure_crypto_provider();

        Self {
            client: crate::tls::client_builder()
                .timeout(std::time::Duration::from_secs(30))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| crate::tls::client()),
        }
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn get(&self, url: &str, headers: Vec<(&str, String)>) -> Result<HttpResponse> {
        let mut request = self.client.get(url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }

    async fn post(
        &self,
        url: &str,
        headers: Vec<(&str, String)>,
        body: &str,
    ) -> Result<HttpResponse> {
        let mut request = self.client.post(url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.body(body.to_string()).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }

    async fn put(
        &self,
        url: &str,
        headers: Vec<(&str, String)>,
        body: &str,
    ) -> Result<HttpResponse> {
        let mut request = self.client.put(url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.body(body.to_string()).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }

    async fn patch(
        &self,
        url: &str,
        headers: Vec<(&str, String)>,
        body: &str,
    ) -> Result<HttpResponse> {
        let mut request = self.client.patch(url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.body(body.to_string()).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }

    async fn delete(&self, url: &str, headers: Vec<(&str, String)>) -> Result<HttpResponse> {
        let mut request = self.client.delete(url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_response_success_200() {
        let resp = HttpResponse {
            status: 200,
            body: String::new(),
        };
        assert!(resp.is_success());
    }

    #[test]
    fn test_http_response_success_299() {
        let resp = HttpResponse {
            status: 299,
            body: String::new(),
        };
        assert!(resp.is_success());
    }

    #[test]
    fn test_http_response_failure_400() {
        let resp = HttpResponse {
            status: 400,
            body: String::new(),
        };
        assert!(!resp.is_success());
    }

    #[test]
    fn test_http_response_failure_500() {
        let resp = HttpResponse {
            status: 500,
            body: String::new(),
        };
        assert!(!resp.is_success());
    }

    #[test]
    fn test_http_response_json_valid() {
        let resp = HttpResponse {
            status: 200,
            body: r#"{"key": "value"}"#.to_string(),
        };
        let parsed: std::collections::HashMap<String, String> = resp.json().unwrap();
        assert_eq!(parsed.get("key").unwrap(), "value");
    }

    #[test]
    fn test_http_response_json_invalid() {
        let resp = HttpResponse {
            status: 200,
            body: "not json".to_string(),
        };
        let result: Result<serde_json::Value> = resp.json();
        assert!(result.is_err());
    }
}
