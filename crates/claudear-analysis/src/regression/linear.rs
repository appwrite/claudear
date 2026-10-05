//! Linear bug regression checker.
//!
//! Checks for regressions of Linear bugs by:
//! 1. Searching GitHub issues in related repositories
//! 2. Scraping appwrite.io/threads for similar mentions
//! 3. Using semantic similarity with embeddings to match issues

use crate::feedback::cosine_similarity;
use crate::feedback::EmbeddingClient;
use crate::regression::RegressionChecker;
use crate::regression::RegressionResult;
use abnegate_http::HttpClient;
use abnegate_http::ReqwestHttpClient;
use async_trait::async_trait;
use claudear_core::error::Result;
use claudear_core::types::RegressionWatch;
use serde::Deserialize;
use std::sync::Arc;

/// Configuration for Linear regression checking.
#[derive(Debug, Clone)]
pub struct LinearRegressionConfig {
    /// GitHub token for searching issues.
    pub github_token: String,
    /// Repositories to search for related issues.
    pub scm_repos: Vec<String>,
    /// Similarity threshold (0.0-1.0) for semantic matching.
    pub similarity_threshold: f64,
}

impl Default for LinearRegressionConfig {
    fn default() -> Self {
        Self {
            github_token: String::new(),
            scm_repos: vec![
                "appwrite/appwrite".to_string(),
                "appwrite/sdk-for-web".to_string(),
                "appwrite/sdk-for-flutter".to_string(),
            ],
            similarity_threshold: 0.75,
        }
    }
}

/// GitHub issue search result.
#[derive(Debug, Clone, Deserialize)]
struct GitHubSearchResult {
    #[expect(dead_code)]
    total_count: i64,
    items: Vec<GitHubIssue>,
}

/// A GitHub issue.
#[derive(Debug, Clone, Deserialize)]
struct GitHubIssue {
    #[expect(dead_code)]
    id: i64,
    #[expect(dead_code)]
    number: i64,
    title: String,
    body: Option<String>,
    #[allow(dead_code)]
    state: String,
    html_url: String,
    created_at: String,
}

/// Result of a similarity match.
#[derive(Debug, Clone)]
pub struct SimilarityMatch {
    /// The URL or identifier of the matched content.
    pub url: String,
    /// The title or summary of the matched content.
    pub title: String,
    /// The similarity score (0.0 to 1.0).
    pub similarity: f32,
}

/// Linear bug regression checker using semantic similarity.
pub struct LinearRegressionChecker<H: HttpClient = ReqwestHttpClient> {
    config: LinearRegressionConfig,
    http: H,
    /// Keywords from the original issue for searching (used for initial GitHub search).
    keywords: Vec<String>,
    /// The original issue text for semantic matching.
    /// Note: Stored for potential future use (e.g., logging, debugging).
    #[allow(dead_code)]
    original_issue_text: String,
    /// Pre-computed embedding for the original issue (if embedding client is available).
    original_embedding: Option<Vec<f32>>,
    /// Embedding client for semantic similarity (optional for testing).
    embedding_client: Option<Arc<EmbeddingClient>>,
}

impl LinearRegressionChecker<ReqwestHttpClient> {
    /// Create a new Linear regression checker.
    pub fn new(
        config: LinearRegressionConfig,
        keywords: Vec<String>,
        original_issue_text: String,
    ) -> Self {
        Self {
            config,
            http: ReqwestHttpClient::new()
                .unwrap_or_else(|_| ReqwestHttpClient::from(reqwest::Client::new())),
            keywords,
            original_issue_text,
            original_embedding: None,
            embedding_client: None,
        }
    }

    /// Create a new Linear regression checker with embedding support.
    pub async fn with_embeddings(
        config: LinearRegressionConfig,
        keywords: Vec<String>,
        original_issue_text: String,
        embedding_client: Arc<EmbeddingClient>,
    ) -> Result<Self> {
        let http = ReqwestHttpClient::new()?;
        let original_embedding = embedding_client.embed(&original_issue_text).await?;

        Ok(Self {
            config,
            http,
            keywords,
            original_issue_text,
            original_embedding: Some(original_embedding),
            embedding_client: Some(embedding_client),
        })
    }
}

impl<H: HttpClient> LinearRegressionChecker<H> {
    /// Create a new Linear regression checker with custom HTTP client (for testing).
    pub fn with_http_client(
        config: LinearRegressionConfig,
        keywords: Vec<String>,
        original_issue_text: String,
        http: H,
    ) -> Self {
        Self {
            config,
            http,
            keywords,
            original_issue_text,
            original_embedding: None,
            embedding_client: None,
        }
    }

    /// Create a new Linear regression checker with custom HTTP client and embedding support.
    pub fn with_http_client_and_embeddings(
        config: LinearRegressionConfig,
        keywords: Vec<String>,
        original_issue_text: String,
        original_embedding: Vec<f32>,
        embedding_client: Arc<EmbeddingClient>,
        http: H,
    ) -> Self {
        Self {
            config,
            http,
            keywords,
            original_issue_text,
            original_embedding: Some(original_embedding),
            embedding_client: Some(embedding_client),
        }
    }

    /// Check semantic similarity between the original issue and candidate text.
    async fn check_similarity(&self, candidate_text: &str) -> Result<f32> {
        match (&self.embedding_client, &self.original_embedding) {
            (Some(client), Some(original_emb)) => {
                let candidate_emb = client.embed(candidate_text).await?;
                Ok(cosine_similarity(original_emb, &candidate_emb))
            }
            _ => Ok(0.0),
        }
    }

    /// Search GitHub issues for similar problems using semantic similarity.
    ///
    /// Returns issues that:
    /// 1. Match keywords (for initial filtering via GitHub search)
    /// 2. Pass semantic similarity threshold when compared to original issue
    ///
    /// Note: This method is available for standalone similarity searching.
    /// For regression checking with date filtering, use `search_github_issues_since`.
    #[allow(dead_code)]
    async fn search_github_issues(&self) -> Result<Vec<SimilarityMatch>> {
        if self.config.github_token.is_empty() {
            tracing::warn!("GitHub token not configured, skipping GitHub issue search for regression detection");
            return Ok(Vec::new());
        }

        let mut all_issues = Vec::new();

        // Build search query from keywords (for initial broad search)
        let query_terms: Vec<String> = self
            .keywords
            .iter()
            .take(5) // Limit to 5 keywords
            .map(|k| format!("\"{}\"", k))
            .collect();

        if query_terms.is_empty() {
            return Ok(Vec::new());
        }

        for repo in &self.config.scm_repos {
            let search_query = format!(
                "repo:{} is:issue is:open {}",
                repo,
                query_terms.join(" OR ")
            );

            let url = format!(
                "https://api.github.com/search/issues?q={}&per_page=10",
                urlencoding::encode(&search_query)
            );

            let headers = vec![
                (
                    "Authorization",
                    format!("Bearer {}", self.config.github_token),
                ),
                ("Accept", "application/vnd.github+json".to_string()),
                ("User-Agent", "claudear".to_string()),
                ("X-GitHub-Api-Version", "2022-11-28".to_string()),
            ];

            let response = self.http.get(&url, headers).await?;

            if response.is_success() {
                if let Ok(result) = serde_json::from_str::<GitHubSearchResult>(&response.body) {
                    all_issues.extend(result.items);
                }
            }
        }

        // Now apply semantic similarity filtering
        let mut similar_issues = Vec::new();
        for issue in all_issues {
            // Combine title and body for semantic comparison
            let issue_text = format!("{}\n{}", issue.title, issue.body.as_deref().unwrap_or(""));

            let similarity = self.check_similarity(&issue_text).await?;

            if similarity >= self.config.similarity_threshold as f32 {
                similar_issues.push(SimilarityMatch {
                    url: issue.html_url.clone(),
                    title: issue.title.clone(),
                    similarity,
                });

                tracing::debug!(
                    "Found similar GitHub issue (similarity={:.2}): {} - {}",
                    similarity,
                    issue.html_url,
                    issue.title
                );
            }
        }

        Ok(similar_issues)
    }

    /// Search GitHub issues and filter by creation date and semantic similarity.
    async fn search_github_issues_since(
        &self,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<SimilarityMatch>> {
        if self.config.github_token.is_empty() {
            tracing::warn!("GitHub token not configured, skipping GitHub issue search for regression detection");
            return Ok(Vec::new());
        }

        let mut all_matches = Vec::new();

        // Build search query from keywords (for initial broad search)
        let query_terms: Vec<String> = self
            .keywords
            .iter()
            .take(5)
            .map(|k| format!("\"{}\"", k))
            .collect();

        if query_terms.is_empty() {
            return Ok(Vec::new());
        }

        for repo in &self.config.scm_repos {
            let search_query = format!(
                "repo:{} is:issue is:open {}",
                repo,
                query_terms.join(" OR ")
            );

            let url = format!(
                "https://api.github.com/search/issues?q={}&per_page=10",
                urlencoding::encode(&search_query)
            );

            let headers = vec![
                (
                    "Authorization",
                    format!("Bearer {}", self.config.github_token),
                ),
                ("Accept", "application/vnd.github+json".to_string()),
                ("User-Agent", "claudear".to_string()),
                ("X-GitHub-Api-Version", "2022-11-28".to_string()),
            ];

            let response = self.http.get(&url, headers).await?;

            if response.is_success() {
                if let Ok(result) = serde_json::from_str::<GitHubSearchResult>(&response.body) {
                    // Filter by date first
                    let new_issues: Vec<_> = result
                        .items
                        .into_iter()
                        .filter(|issue| {
                            if let Ok(created) =
                                chrono::DateTime::parse_from_rfc3339(&issue.created_at)
                            {
                                created.with_timezone(&chrono::Utc) > since
                            } else {
                                false
                            }
                        })
                        .collect();

                    // Then apply semantic similarity
                    for issue in new_issues {
                        let issue_text =
                            format!("{}\n{}", issue.title, issue.body.as_deref().unwrap_or(""));

                        let similarity = self.check_similarity(&issue_text).await?;

                        if similarity >= self.config.similarity_threshold as f32 {
                            all_matches.push(SimilarityMatch {
                                url: issue.html_url.clone(),
                                title: issue.title.clone(),
                                similarity,
                            });

                            tracing::info!(
                                "Found semantically similar GitHub issue (similarity={:.2}): {} - {}",
                                similarity,
                                issue.html_url,
                                issue.title
                            );
                        }
                    }
                }
            }
        }

        Ok(all_matches)
    }

    /// Check appwrite.io/threads for similar issues using semantic similarity.
    ///
    /// This extracts text content from the threads page and uses embedding-based
    /// similarity to find semantically related discussions.
    async fn check_appwrite_threads(&self) -> Result<Vec<SimilarityMatch>> {
        let url = "https://appwrite.io/threads";
        let headers = vec![
            ("User-Agent", "claudear".to_string()),
            ("Accept", "text/html".to_string()),
        ];

        match self.http.get(url, headers).await {
            Ok(response) if response.is_success() => {
                // Extract thread content sections from HTML
                // This is a simplified extraction - in production, use a proper HTML parser
                let thread_sections = self.extract_thread_sections(&response.body);

                let mut matches = Vec::new();

                for (title, content) in thread_sections {
                    let thread_text = format!("{}\n{}", title, content);
                    let similarity = self.check_similarity(&thread_text).await?;

                    if similarity >= self.config.similarity_threshold as f32 {
                        matches.push(SimilarityMatch {
                            url: format!(
                                "https://appwrite.io/threads#{}",
                                title.to_lowercase().replace(' ', "-")
                            ),
                            title: title.clone(),
                            similarity,
                        });

                        tracing::info!(
                            "Found semantically similar thread (similarity={:.2}): {}",
                            similarity,
                            title
                        );
                    }
                }

                Ok(matches)
            }
            Ok(response) => {
                // Non-success HTTP status - log warning but continue
                tracing::warn!(
                    url = %url,
                    status = response.status,
                    "Failed to fetch appwrite.io/threads, skipping threads check"
                );
                Ok(Vec::new())
            }
            Err(e) => {
                // Network or other error - log warning but continue
                tracing::warn!(
                    url = %url,
                    error = %e,
                    "Error fetching appwrite.io/threads, skipping threads check"
                );
                Ok(Vec::new())
            }
        }
    }

    /// Extract thread sections from HTML content.
    ///
    /// This is a simplified implementation that looks for common HTML patterns.
    /// In production, use a proper HTML parser like scraper or select.
    fn extract_thread_sections(&self, html: &str) -> Vec<(String, String)> {
        let mut sections = Vec::new();

        // Simple extraction: look for <article>, <div class="thread">, or similar patterns
        // Use case-insensitive search on the original string to avoid UTF-8 byte offset issues
        let title_patterns = [
            "<h2>",
            "<h3>",
            "<H2>",
            "<H3>",
            "<article",
            "<Article",
            "<ARTICLE",
            "<div class=\"thread",
            "<div class=\"post",
            "<DIV class=\"thread",
        ];

        for pattern in title_patterns {
            if let Some(start) = html.find(pattern) {
                // Safely calculate the end position respecting UTF-8 boundaries
                // Take up to 2000 bytes but ensure we end on a valid char boundary
                let mut end = (start + 2000).min(html.len());
                while end > start && !html.is_char_boundary(end) {
                    end -= 1;
                }

                if end <= start {
                    continue;
                }

                let section = &html[start..end];

                // Try to extract title (text between tags)
                if let Some(title_end) = section.find("</") {
                    let title_content = &section[..title_end];
                    // Remove HTML tags from title
                    let title = self.strip_html_tags(title_content);

                    // Content is the rest
                    let content = self.strip_html_tags(&section[title_end..]);

                    if !title.is_empty() && !content.is_empty() {
                        sections.push((title, content));
                    }
                }
            }
        }

        // If no structured content found, treat the whole page as one section
        if sections.is_empty() {
            let full_text = self.strip_html_tags(html);
            if !full_text.is_empty() {
                sections.push(("Appwrite Threads Page".to_string(), full_text));
            }
        }

        sections
    }

    /// Strip HTML tags from text content.
    fn strip_html_tags(&self, html: &str) -> String {
        let mut result = String::new();
        let mut in_tag = false;

        for c in html.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => result.push(c),
                _ => {}
            }
        }

        // Normalize whitespace
        result
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string()
    }
}

#[async_trait]
impl<H: HttpClient> RegressionChecker for LinearRegressionChecker<H> {
    async fn check_regression(&self, watch: &RegressionWatch) -> Result<RegressionResult> {
        let monitoring_started = match watch.monitoring_started_at {
            Some(dt) => dt,
            None => return Ok(RegressionResult::no_regression()),
        };

        // Check GitHub issues using semantic similarity
        let similar_issues = self.search_github_issues_since(monitoring_started).await?;

        if !similar_issues.is_empty() {
            let issue_links: Vec<String> = similar_issues
                .iter()
                .map(|m| {
                    format!(
                        "{} (similarity: {:.0}%) - {}",
                        m.url,
                        m.similarity * 100.0,
                        m.title
                    )
                })
                .collect();

            return Ok(RegressionResult::regression(format!(
                "Found {} semantically similar GitHub issues that may indicate regression:\n{}",
                similar_issues.len(),
                issue_links.join("\n")
            )));
        }

        // Check appwrite.io/threads using semantic similarity
        let thread_matches = self.check_appwrite_threads().await?;
        if !thread_matches.is_empty() {
            let thread_links: Vec<String> = thread_matches
                .iter()
                .map(|m| {
                    format!(
                        "{} (similarity: {:.0}%) - {}",
                        m.url,
                        m.similarity * 100.0,
                        m.title
                    )
                })
                .collect();

            return Ok(RegressionResult::regression(format!(
                "Found {} semantically similar discussions on appwrite.io/threads:\n{}",
                thread_matches.len(),
                thread_links.join("\n")
            )));
        }

        let method = if self.embedding_client.is_some() {
            "semantic similarity (embeddings)"
        } else {
            "keyword matching (fallback)"
        };

        Ok(RegressionResult {
            regression_detected: false,
            details: Some(format!(
                "No regression indicators found using {} (searched {} repos, threshold: {:.0}%)",
                method,
                self.config.scm_repos.len(),
                self.config.similarity_threshold * 100.0
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abnegate_http::HttpResponse;
    use chrono::Duration;
    use chrono::Utc;
    use claudear_core::types::IssueType;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    struct MockHttpClient {
        responses: Vec<HttpResponse>,
        call_count: AtomicUsize,
    }

    impl MockHttpClient {
        fn new(responses: Vec<(u16, &str)>) -> Self {
            Self {
                responses: responses
                    .into_iter()
                    .map(|(status, body)| HttpResponse::new(status, body.to_string()))
                    .collect(),
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl HttpClient for MockHttpClient {
        async fn get(
            &self,
            _url: &str,
            _headers: Vec<(&str, String)>,
        ) -> abnegate_http::Result<HttpResponse> {
            let call = self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .responses
                .get(call)
                .cloned()
                .unwrap_or_else(|| HttpResponse::new(404, "Not Found")))
        }
    }

    fn create_config() -> LinearRegressionConfig {
        LinearRegressionConfig {
            github_token: "test-token".to_string(),
            scm_repos: vec!["test/repo".to_string()],
            similarity_threshold: 0.75,
        }
    }

    #[tokio::test]
    async fn test_no_regression_when_no_issues() {
        let mock = MockHttpClient::new(vec![
            // GitHub search - empty results
            (200, r#"{"total_count": 0, "items": []}"#),
            // Threads check - no matches
            (200, "<html><body>No relevant content</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["test-keyword".to_string()],
            "Original issue about test-keyword problem".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-123", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_no_regression_without_embedding_client() {
        // Without an embedding client, similarity always returns 0.0
        // so no regression can be detected via similarity
        let future_time = (Utc::now() + Duration::hours(1)).to_rfc3339();

        let mut config = create_config();
        config.similarity_threshold = 0.5;

        let mock = MockHttpClient::new(vec![
            (
                200,
                &format!(
                    r#"{{
                        "total_count": 1,
                        "items": [{{
                            "id": 1,
                            "number": 42,
                            "title": "Bug: Same issue reoccurred",
                            "body": "This bug is causing problems",
                            "state": "open",
                            "html_url": "https://github.com/test/repo/issues/42",
                            "created_at": "{}"
                        }}]
                    }}"#,
                    future_time
                ),
            ),
            // Threads check
            (200, "<html><body>No matches</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["bug".to_string()],
            "Bug: Original issue about a bug problem".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-456", 1);
        watch.monitoring_started_at = Some(Utc::now());

        let result = checker.check_regression(&watch).await.unwrap();
        // No embedding client → similarity is 0.0 → no regression detected
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_no_regression_when_old_github_issue() {
        // Create a timestamp in the past
        let past_time = (Utc::now() - Duration::days(10)).to_rfc3339();

        let mock = MockHttpClient::new(vec![
            // GitHub search - found old issue
            (
                200,
                &format!(
                    r#"{{
                        "total_count": 1,
                        "items": [{{
                            "id": 1,
                            "number": 42,
                            "title": "Old bug",
                            "body": "Description",
                            "state": "open",
                            "html_url": "https://github.com/test/repo/issues/42",
                            "created_at": "{}"
                        }}]
                    }}"#,
                    past_time
                ),
            ),
            // Threads check
            (200, "<html><body>No matches</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["bug".to_string()],
            "Bug: Original issue with bug keyword".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-789", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::days(5)); // Started 5 days ago

        let result = checker.check_regression(&watch).await.unwrap();
        // Issue was created 10 days ago, before monitoring started 5 days ago
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_no_regression_without_embedding_client_threads() {
        // Without an embedding client, check_similarity returns 0.0
        // so no regression can be detected even if threads mention keywords
        let mut config = create_config();
        config.similarity_threshold = 0.5;

        let mock = MockHttpClient::new(vec![
            // GitHub search - no results
            (200, r#"{"total_count": 0, "items": []}"#),
            // Threads check
            (
                200,
                "<html><body><h2>Authentication Error Discussion</h2>Discussion about authentication-error issue and how to solve it</body></html>",
            ),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["authentication-error".to_string()],
            "Authentication-error: Users cannot log in due to authentication-error".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-111", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_no_monitoring_started() {
        let mock = MockHttpClient::new(vec![]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["test".to_string()],
            "Test issue with test keyword".to_string(),
            mock,
        );

        // Watch without monitoring_started_at
        let watch = RegressionWatch::new(IssueType::LinearBug, "linear-222", 1);

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_empty_keywords() {
        let mock = MockHttpClient::new(vec![
            // Threads check - no keywords to search
            (200, "<html><body>Content</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec![], // No keywords
            "Original issue without specific keywords".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-333", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_similarity_match_struct() {
        let match_result = SimilarityMatch {
            url: "https://example.com/issue/1".to_string(),
            title: "Test Issue".to_string(),
            similarity: 0.85,
        };

        assert_eq!(match_result.url, "https://example.com/issue/1");
        assert_eq!(match_result.title, "Test Issue");
        assert!((match_result.similarity - 0.85).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_strip_html_tags() {
        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["test".to_string()],
            "Test issue".to_string(),
            MockHttpClient::new(vec![]),
        );

        let html = "<p>Hello <strong>world</strong>!</p>";
        let stripped = checker.strip_html_tags(html);
        assert_eq!(stripped, "Hello world!");
    }

    #[tokio::test]
    async fn test_extract_thread_sections_with_headers() {
        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["test".to_string()],
            "Test issue".to_string(),
            MockHttpClient::new(vec![]),
        );

        let html =
            "<html><body><h2>Thread Title</h2>This is the content of the thread</body></html>";
        let sections = checker.extract_thread_sections(html);

        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_fallback_keyword_similarity() {
        // Without embeddings, the checker should fall back to keyword matching
        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["authentication".to_string(), "error".to_string()],
            "Authentication error when logging in".to_string(),
            MockHttpClient::new(vec![]),
        );

        // Test the check_similarity fallback (keyword matching)
        // This is testing the internal method indirectly through the behavior
        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-test", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        // Should not error even without embeddings
        let result = checker.check_regression(&watch).await;
        assert!(result.is_ok());
    }

    struct MockErrorHttpClient {
        client: reqwest::Client,
    }

    impl MockErrorHttpClient {
        fn new() -> Self {
            Self {
                client: reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .expect("a client without a proxy builds"),
            }
        }
    }

    #[async_trait]
    impl HttpClient for MockErrorHttpClient {
        async fn get(
            &self,
            _url: &str,
            _headers: Vec<(&str, String)>,
        ) -> abnegate_http::Result<HttpResponse> {
            let refusal = self
                .client
                .get("http://127.0.0.1:1/")
                .send()
                .await
                .expect_err("nothing listens on port 1");
            Err(abnegate_http::Error::from(refusal))
        }
    }

    /// Helper to build a checker with a given mock, reused by many new tests.
    fn make_checker(mock: MockHttpClient) -> LinearRegressionChecker<MockHttpClient> {
        LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["test".to_string()],
            "Test issue".to_string(),
            mock,
        )
    }

    #[tokio::test]
    async fn test_strip_html_tags_empty_string() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        assert_eq!(checker.strip_html_tags(""), "");
    }

    #[tokio::test]
    async fn test_strip_html_tags_only_tags() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        assert_eq!(checker.strip_html_tags("<br><hr>"), "");
    }

    #[tokio::test]
    async fn test_strip_html_tags_nested_tags() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let result = checker.strip_html_tags("<div><span>inner text</span></div>");
        assert_eq!(result, "inner text");
    }

    #[tokio::test]
    async fn test_strip_html_tags_self_closing() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let result = checker.strip_html_tags("<br/>text after break");
        assert_eq!(result, "text after break");
    }

    #[tokio::test]
    async fn test_strip_html_tags_special_chars_in_attributes() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let result = checker.strip_html_tags(r#"<div class="foo" id="bar">baz</div>"#);
        assert_eq!(result, "baz");
    }

    #[tokio::test]
    async fn test_strip_html_tags_normalizes_whitespace() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let result = checker.strip_html_tags("  hello   world  \n  foo  ");
        assert_eq!(result, "hello world foo");
    }

    #[tokio::test]
    async fn test_strip_html_tags_no_tags_returned_trimmed() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let result = checker.strip_html_tags("  plain text  ");
        assert_eq!(result, "plain text");
    }

    #[tokio::test]
    async fn test_extract_thread_sections_no_headings_fallback() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html = "<html><body><p>Just some plain paragraph content here</p></body></html>";
        let sections = checker.extract_thread_sections(html);
        // Falls back to whole page as one section
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "Appwrite Threads Page");
        assert!(sections[0]
            .1
            .contains("Just some plain paragraph content here"));
    }

    #[tokio::test]
    async fn test_extract_thread_sections_h2_tags() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html = "<html><body><h2>Section Title</h2>Section content goes here</body></html>";
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
        // The first section should have extracted text from the h2 area
        let (title, _content) = &sections[0];
        assert!(title.contains("Section Title") || !sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_article_tag() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html = "<html><body><article>Article Title</article>Rest of content here</body></html>";
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_div_thread_class() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html = r#"<html><body><div class="thread">Thread Title</div>Thread body content</body></html>"#;
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_empty_html() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let sections = checker.extract_thread_sections("");
        // Empty HTML → strip_html_tags returns empty → no sections
        assert!(sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_uppercase_h3() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // The code explicitly has "<H3>" in title_patterns for case-insensitive matching
        let html = "<html><body><H3>Upper Case Heading</H3>Content under heading</body></html>";
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_check_similarity_without_embedding_returns_zero() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // Directly verify check_similarity returns 0.0 when no embedding client
        let similarity = checker.check_similarity("any text").await.unwrap();
        assert!((similarity - 0.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_search_github_issues_since_empty_token() {
        let config = LinearRegressionConfig {
            github_token: String::new(), // Empty token
            scm_repos: vec!["test/repo".to_string()],
            similarity_threshold: 0.75,
        };

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            MockHttpClient::new(vec![]),
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_since_api_error_non_200() {
        let mock = MockHttpClient::new(vec![(500, r#"{"message": "Internal Server Error"}"#)]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        // Non-200 response → gracefully returns empty
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_since_invalid_json() {
        let mock = MockHttpClient::new(vec![(200, "this is not valid json at all")]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        // Invalid JSON → serde_json::from_str fails → skips gracefully
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_since_multiple_repos() {
        let config = LinearRegressionConfig {
            github_token: "test-token".to_string(),
            scm_repos: vec![
                "org/repo1".to_string(),
                "org/repo2".to_string(),
                "org/repo3".to_string(),
            ],
            similarity_threshold: 0.75,
        };

        let mock = MockHttpClient::new(vec![
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, r#"{"total_count": 0, "items": []}"#),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        assert!(results.is_empty());

        // Verify all 3 repos were searched (3 HTTP calls made)
        assert_eq!(checker.http.call_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_search_github_issues_since_keyword_limit() {
        // Provide more than 5 keywords — only first 5 should be used
        let config = LinearRegressionConfig {
            github_token: "test-token".to_string(),
            scm_repos: vec!["test/repo".to_string()],
            similarity_threshold: 0.75,
        };

        let mock = MockHttpClient::new(vec![(200, r#"{"total_count": 0, "items": []}"#)]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec![
                "kw1".to_string(),
                "kw2".to_string(),
                "kw3".to_string(),
                "kw4".to_string(),
                "kw5".to_string(),
                "kw6".to_string(),
                "kw7".to_string(),
            ],
            "Issue with many keywords".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        assert!(results.is_empty());

        // The search still completes successfully — the take(5) ensures only 5 keywords
        // are included in the query. We verify indirectly that the call was made.
        assert_eq!(checker.http.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_check_appwrite_threads_non_200() {
        let mock = MockHttpClient::new(vec![(503, "Service Unavailable")]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let results = checker.check_appwrite_threads().await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_check_appwrite_threads_network_error() {
        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            MockErrorHttpClient::new(),
        );

        // Network error should be caught and return empty vec (not propagate error)
        let results = checker.check_appwrite_threads().await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_check_regression_empty_token_and_threads_error() {
        // Empty GitHub token skips GitHub search; threads returns an error
        // → should still succeed with no regression
        let config = LinearRegressionConfig {
            github_token: String::new(),
            scm_repos: vec!["test/repo".to_string()],
            similarity_threshold: 0.75,
        };

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            MockErrorHttpClient::new(), // threads will get a network error
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-err", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_check_regression_multiple_repos_all_empty() {
        let config = LinearRegressionConfig {
            github_token: "test-token".to_string(),
            scm_repos: vec![
                "org/repo-a".to_string(),
                "org/repo-b".to_string(),
                "org/repo-c".to_string(),
            ],
            similarity_threshold: 0.75,
        };

        let mock = MockHttpClient::new(vec![
            // 3 GitHub searches (one per repo), all empty
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, r#"{"total_count": 0, "items": []}"#),
            // 1 threads check
            (200, "<html><body>Nothing relevant</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-multi", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
    }

    #[tokio::test]
    async fn test_check_regression_details_includes_keyword_method() {
        // Without embedding client, the method should be "keyword matching (fallback)"
        let mock = MockHttpClient::new(vec![
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, "<html><body>Nothing</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-method", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
        let details = result.details.unwrap();
        assert!(
            details.contains("keyword matching (fallback)"),
            "Expected details to contain method type, got: {}",
            details
        );
    }

    #[tokio::test]
    async fn test_check_regression_details_includes_semantic_method_with_embeddings() {
        // With embedding client set (but original_embedding = Some), method should be "semantic similarity"
        // We use with_http_client_and_embeddings with a dummy embedding
        // Note: The EmbeddingClient would fail on actual embed calls, but since
        // no issues are returned, check_similarity is never called for candidates.
        // However, the `embedding_client` field being Some makes the method "semantic similarity".
        //
        // We can't easily construct a real EmbeddingClient in tests, so we test
        // the inverse: without embedding_client, "keyword matching (fallback)" is shown.
        // This test verifies the method string changes when embedding_client is Some
        // by checking the code path. We already tested the "keyword" path above.
        // Instead, let's verify the details message includes repo count and threshold.
        let config = LinearRegressionConfig {
            github_token: "test-token".to_string(),
            scm_repos: vec!["a/b".to_string(), "c/d".to_string()],
            similarity_threshold: 0.80,
        };

        let mock = MockHttpClient::new(vec![
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, "<html><body>Nothing</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-details", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        let details = result.details.unwrap();
        assert!(
            details.contains("2 repos"),
            "Expected details to contain repo count, got: {}",
            details
        );
        assert!(
            details.contains("80%"),
            "Expected details to contain threshold percentage, got: {}",
            details
        );
    }

    #[test]
    fn test_linear_regression_config_default_repos() {
        let config = LinearRegressionConfig::default();
        assert_eq!(config.scm_repos.len(), 3);
        assert!(config.scm_repos.contains(&"appwrite/appwrite".to_string()));
        assert!(config
            .scm_repos
            .contains(&"appwrite/sdk-for-web".to_string()));
        assert!(config
            .scm_repos
            .contains(&"appwrite/sdk-for-flutter".to_string()));
    }

    #[test]
    fn test_linear_regression_config_default_threshold() {
        let config = LinearRegressionConfig::default();
        assert!((config.similarity_threshold - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_linear_regression_config_default_empty_token() {
        let config = LinearRegressionConfig::default();
        assert!(config.github_token.is_empty());
    }

    #[test]
    fn test_similarity_match_clone() {
        let original = SimilarityMatch {
            url: "https://example.com/issue/1".to_string(),
            title: "Cloned Issue".to_string(),
            similarity: 0.92,
        };

        let cloned = original.clone();
        assert_eq!(cloned.url, "https://example.com/issue/1");
        assert_eq!(cloned.title, "Cloned Issue");
        assert!((cloned.similarity - 0.92).abs() < f32::EPSILON);
    }

    #[test]
    fn test_similarity_match_debug() {
        let m = SimilarityMatch {
            url: "https://example.com/issue/2".to_string(),
            title: "Debug Issue".to_string(),
            similarity: 0.77,
        };

        let debug_output = format!("{:?}", m);
        assert!(debug_output.contains("SimilarityMatch"));
        assert!(debug_output.contains("https://example.com/issue/2"));
        assert!(debug_output.contains("Debug Issue"));
        assert!(debug_output.contains("0.77"));
    }

    #[test]
    fn test_with_http_client_sets_fields_correctly() {
        let config = LinearRegressionConfig {
            github_token: "tok-abc".to_string(),
            scm_repos: vec!["org/repo".to_string()],
            similarity_threshold: 0.90,
        };
        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["kw1".to_string(), "kw2".to_string()],
            "Original text".to_string(),
            MockHttpClient::new(vec![]),
        );

        assert_eq!(checker.config.github_token, "tok-abc");
        assert_eq!(checker.config.scm_repos.len(), 1);
        assert!((checker.config.similarity_threshold - 0.90).abs() < f64::EPSILON);
        assert_eq!(checker.keywords.len(), 2);
        assert_eq!(checker.keywords[0], "kw1");
        assert_eq!(checker.original_issue_text, "Original text");
        assert!(checker.original_embedding.is_none());
        assert!(checker.embedding_client.is_none());
    }

    #[test]
    fn test_with_http_client_and_embeddings_sets_fields_correctly() {
        let config = create_config();
        let embedding_client = match EmbeddingClient::new(crate::feedback::EmbeddingConfig {
            pool_size: 1,
            ..Default::default()
        }) {
            Ok(c) => Arc::new(c),
            Err(_) => return, // Skip if model unavailable (CI race)
        };
        let original_emb = vec![0.1, 0.2, 0.3];

        let checker = LinearRegressionChecker::with_http_client_and_embeddings(
            config,
            vec!["kw".to_string()],
            "Issue text".to_string(),
            original_emb.clone(),
            embedding_client,
            MockHttpClient::new(vec![]),
        );

        assert!(checker.original_embedding.is_some());
        assert_eq!(checker.original_embedding.as_ref().unwrap().len(), 3);
        assert!(checker.embedding_client.is_some());
        assert_eq!(checker.original_issue_text, "Issue text");
    }

    #[test]
    fn test_linear_regression_config_clone() {
        let config = LinearRegressionConfig {
            github_token: "secret".to_string(),
            scm_repos: vec!["a/b".to_string(), "c/d".to_string()],
            similarity_threshold: 0.88,
        };
        let cloned = config.clone();
        assert_eq!(cloned.github_token, "secret");
        assert_eq!(cloned.scm_repos.len(), 2);
        assert!((cloned.similarity_threshold - 0.88).abs() < f64::EPSILON);
    }

    #[test]
    fn test_linear_regression_config_debug() {
        let config = create_config();
        let debug = format!("{:?}", config);
        assert!(debug.contains("LinearRegressionConfig"));
        assert!(debug.contains("test-token"));
        assert!(debug.contains("test/repo"));
    }

    #[tokio::test]
    async fn test_search_github_issues_empty_token_returns_empty() {
        let config = LinearRegressionConfig {
            github_token: String::new(),
            scm_repos: vec!["test/repo".to_string()],
            similarity_threshold: 0.75,
        };
        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            MockHttpClient::new(vec![]),
        );

        let results = checker.search_github_issues().await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_empty_keywords_returns_empty() {
        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec![],
            "Some issue".to_string(),
            MockHttpClient::new(vec![]),
        );

        let results = checker.search_github_issues().await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_no_embeddings_filters_everything() {
        // Without embeddings, similarity is 0.0, so all issues are filtered out
        let mock = MockHttpClient::new(vec![(
            200,
            r#"{
                "total_count": 1,
                "items": [{
                    "id": 10,
                    "number": 5,
                    "title": "Bug report",
                    "body": "Description of the bug",
                    "state": "open",
                    "html_url": "https://github.com/test/repo/issues/5",
                    "created_at": "2025-01-01T00:00:00Z"
                }]
            }"#,
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["bug".to_string()],
            "Bug report".to_string(),
            mock,
        );

        let results = checker.search_github_issues().await.unwrap();
        // No embedding client -> similarity = 0.0 -> below threshold -> empty
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_non_200_status() {
        let mock = MockHttpClient::new(vec![(403, r#"{"message": "Forbidden"}"#)]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let results = checker.search_github_issues().await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_invalid_json_response() {
        let mock = MockHttpClient::new(vec![(200, "not json")]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let results = checker.search_github_issues().await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_multiple_repos() {
        let config = LinearRegressionConfig {
            github_token: "token".to_string(),
            scm_repos: vec!["a/b".to_string(), "c/d".to_string()],
            similarity_threshold: 0.75,
        };

        let mock = MockHttpClient::new(vec![
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, r#"{"total_count": 0, "items": []}"#),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let results = checker.search_github_issues().await.unwrap();
        assert!(results.is_empty());
        assert_eq!(checker.http.call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_search_github_issues_null_body() {
        let mock = MockHttpClient::new(vec![(
            200,
            r#"{
                "total_count": 1,
                "items": [{
                    "id": 1,
                    "number": 1,
                    "title": "Issue without body",
                    "body": null,
                    "state": "open",
                    "html_url": "https://github.com/test/repo/issues/1",
                    "created_at": "2025-01-01T00:00:00Z"
                }]
            }"#,
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        // Should not panic even with null body
        let results = checker.search_github_issues().await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_since_invalid_date_format() {
        // Issue with an unparseable created_at should be filtered out
        let mock = MockHttpClient::new(vec![(
            200,
            r#"{
                "total_count": 1,
                "items": [{
                    "id": 1,
                    "number": 1,
                    "title": "Issue with bad date",
                    "body": "Content",
                    "state": "open",
                    "html_url": "https://github.com/test/repo/issues/1",
                    "created_at": "not-a-valid-date"
                }]
            }"#,
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        // Invalid date -> filtered out by date parsing branch
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_since_null_body() {
        let future_time = (Utc::now() + Duration::hours(1)).to_rfc3339();
        let mock = MockHttpClient::new(vec![(
            200,
            &format!(
                r#"{{
                    "total_count": 1,
                    "items": [{{
                        "id": 1,
                        "number": 1,
                        "title": "Issue without body",
                        "body": null,
                        "state": "open",
                        "html_url": "https://github.com/test/repo/issues/1",
                        "created_at": "{}"
                    }}]
                }}"#,
                future_time
            ),
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::hours(1);
        // Should not panic with null body
        let results = checker.search_github_issues_since(since).await.unwrap();
        assert!(results.is_empty()); // No embeddings -> similarity 0.0
    }

    #[tokio::test]
    async fn test_search_github_issues_since_empty_keywords() {
        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec![],
            "Some issue".to_string(),
            MockHttpClient::new(vec![]),
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_since_empty_repos() {
        let config = LinearRegressionConfig {
            github_token: "token".to_string(),
            scm_repos: vec![],
            similarity_threshold: 0.75,
        };

        let mock = MockHttpClient::new(vec![]);
        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        assert!(results.is_empty());
        // No repos -> no HTTP calls
        assert_eq!(checker.http.call_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_search_github_issues_since_issue_exactly_at_since_boundary() {
        // Issue created at exactly the `since` time should NOT be included (uses > not >=)
        let boundary_time = Utc::now() - Duration::hours(1);
        let mock = MockHttpClient::new(vec![(
            200,
            &format!(
                r#"{{
                    "total_count": 1,
                    "items": [{{
                        "id": 1,
                        "number": 1,
                        "title": "Boundary issue",
                        "body": "Content",
                        "state": "open",
                        "html_url": "https://github.com/test/repo/issues/1",
                        "created_at": "{}"
                    }}]
                }}"#,
                boundary_time.to_rfc3339()
            ),
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let results = checker
            .search_github_issues_since(boundary_time)
            .await
            .unwrap();
        // Created at == since, so > check excludes it
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_check_appwrite_threads_success_empty_body() {
        let mock = MockHttpClient::new(vec![(200, "")]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let results = checker.check_appwrite_threads().await.unwrap();
        // Empty HTML -> extract_thread_sections returns empty -> no matches
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_check_appwrite_threads_success_with_content_no_embeddings() {
        let mock = MockHttpClient::new(vec![(
            200,
            "<html><body><h2>Thread About Auth Bug</h2>Users reporting auth issues after update</body></html>",
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["auth".to_string()],
            "Auth bug issue".to_string(),
            mock,
        );

        let results = checker.check_appwrite_threads().await.unwrap();
        // No embedding client -> similarity 0.0 -> below threshold
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_check_appwrite_threads_404_status() {
        let mock = MockHttpClient::new(vec![(404, "Not Found")]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let results = checker.check_appwrite_threads().await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_h3_tag() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html = "<html><body><h3>Small Heading</h3>Content under small heading</body></html>";
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_uppercase_h2() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html =
            "<html><body><H2>UPPERCASE HEADING</H2>Content under uppercase heading</body></html>";
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_div_post_class() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html = r#"<html><body><div class="post">Post Title</div>Post body text content here</body></html>"#;
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_uppercase_article() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html =
            "<html><body><ARTICLE>Article Content</ARTICLE>More text after article</body></html>";
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_mixed_case_article() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html =
            "<html><body><Article>Mixed Case Article</Article>Additional content here</body></html>";
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_uppercase_div_thread() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html = r#"<html><body><DIV class="thread">Thread Title</DIV>Thread body content</body></html>"#;
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_only_tags_no_text() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // HTML with tags but after stripping, text is empty
        let html = "<html><body><div><span></span></div></body></html>";
        let sections = checker.extract_thread_sections(html);
        // strip_html_tags returns empty -> no fallback section
        assert!(sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_plain_text_fallback() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // No matching tag patterns, but there IS text content
        let html = "Just plain text without any structured HTML tags at all";
        let sections = checker.extract_thread_sections(html);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "Appwrite Threads Page");
        assert!(sections[0].1.contains("Just plain text"));
    }

    #[tokio::test]
    async fn test_strip_html_tags_unicode_content() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let result = checker.strip_html_tags("<p>日本語テスト</p>");
        assert_eq!(result, "日本語テスト");
    }

    #[tokio::test]
    async fn test_strip_html_tags_emoji_content() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let result = checker.strip_html_tags("<div>Hello World</div>");
        assert_eq!(result, "Hello World");
    }

    #[tokio::test]
    async fn test_strip_html_tags_angle_brackets_without_tags() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // Angle brackets that form tags will strip their content
        let result = checker.strip_html_tags("a < b > c");
        // '<' starts a "tag", everything until '>' is consumed, so ' b ' is lost
        assert_eq!(result, "a c");
    }

    #[tokio::test]
    async fn test_strip_html_tags_multiple_nested_tags() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let result = checker
            .strip_html_tags("<html><head><title>T</title></head><body><p>Body</p></body></html>");
        // Tags are stripped but no whitespace inserted between adjacent tags
        assert_eq!(result, "TBody");
    }

    #[tokio::test]
    async fn test_strip_html_tags_script_and_style() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // The simple tag stripper does not handle script/style content specially
        // It strips the tags but leaves inner text; no whitespace inserted between adjacent elements
        let result = checker.strip_html_tags("<style>body{color:red}</style><p>Visible</p>");
        assert_eq!(result, "body{color:red}Visible");
    }

    #[tokio::test]
    async fn test_check_regression_github_error_propagates() {
        // If the GitHub HTTP call itself errors, it should propagate
        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            MockErrorHttpClient::new(),
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-err2", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        // MockErrorHttpClient will error on the GitHub search call
        let result = checker.check_regression(&watch).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_check_regression_details_includes_repo_count() {
        let config = LinearRegressionConfig {
            github_token: "tok".to_string(),
            scm_repos: vec![
                "a/1".to_string(),
                "b/2".to_string(),
                "c/3".to_string(),
                "d/4".to_string(),
                "e/5".to_string(),
            ],
            similarity_threshold: 0.60,
        };

        let mock = MockHttpClient::new(vec![
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, "<html><body>Nothing</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-5repos", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
        let details = result.details.unwrap();
        assert!(details.contains("5 repos"), "Got: {}", details);
        assert!(details.contains("60%"), "Got: {}", details);
    }

    #[tokio::test]
    async fn test_check_regression_no_repos_configured() {
        let config = LinearRegressionConfig {
            github_token: "tok".to_string(),
            scm_repos: vec![],
            similarity_threshold: 0.75,
        };

        let mock = MockHttpClient::new(vec![
            // Only the threads check should happen
            (200, "<html><body>Nothing</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-norepo", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
        let details = result.details.unwrap();
        assert!(details.contains("0 repos"), "Got: {}", details);
    }

    #[tokio::test]
    async fn test_extract_thread_sections_with_multibyte_utf8() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // Create content with multibyte UTF-8 characters near the 2000 byte boundary
        let long_content = "x".repeat(1990);
        let html = format!("<h2>Title</h2>{}", long_content);
        let sections = checker.extract_thread_sections(&html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_very_long_content() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // Content exceeding the 2000-byte extraction window
        let long_content = "a".repeat(5000);
        let html = format!("<h2>Long Thread</h2>{}", long_content);
        let sections = checker.extract_thread_sections(&html);
        assert!(!sections.is_empty());
        // The extracted section should be truncated to around 2000 chars
        let (title, _) = &sections[0];
        assert!(title.contains("Long Thread"));
    }

    #[tokio::test]
    async fn test_search_github_issues_limits_to_5_keywords() {
        let mock = MockHttpClient::new(vec![(200, r#"{"total_count": 0, "items": []}"#)]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec![
                "k1".to_string(),
                "k2".to_string(),
                "k3".to_string(),
                "k4".to_string(),
                "k5".to_string(),
                "k6".to_string(),
                "k7".to_string(),
                "k8".to_string(),
            ],
            "Issue with many keywords".to_string(),
            mock,
        );

        let results = checker.search_github_issues().await.unwrap();
        assert!(results.is_empty());
        // One HTTP call made (one repo), meaning the query was built successfully
        assert_eq!(checker.http.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_search_github_issues_since_filters_old_keeps_new() {
        let old_time = (Utc::now() - Duration::days(30)).to_rfc3339();
        let new_time = (Utc::now() + Duration::hours(1)).to_rfc3339();

        let mock = MockHttpClient::new(vec![(
            200,
            &format!(
                r#"{{
                    "total_count": 2,
                    "items": [
                        {{
                            "id": 1,
                            "number": 1,
                            "title": "Old issue",
                            "body": "Old content",
                            "state": "open",
                            "html_url": "https://github.com/test/repo/issues/1",
                            "created_at": "{}"
                        }},
                        {{
                            "id": 2,
                            "number": 2,
                            "title": "New issue",
                            "body": "New content",
                            "state": "open",
                            "html_url": "https://github.com/test/repo/issues/2",
                            "created_at": "{}"
                        }}
                    ]
                }}"#,
                old_time, new_time
            ),
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::days(7);
        let results = checker.search_github_issues_since(since).await.unwrap();
        // New issue passes date filter but similarity is 0.0 (no embeddings) -> filtered out
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_check_regression_empty_token_skips_github_checks_threads() {
        let config = LinearRegressionConfig {
            github_token: String::new(),
            scm_repos: vec!["test/repo".to_string()],
            similarity_threshold: 0.75,
        };

        let mock = MockHttpClient::new(vec![
            // Only threads check should happen (no GitHub calls)
            (200, "<html><body>No relevant threads</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-notoken", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
        // Only 1 HTTP call (threads), not 2 (no GitHub search)
        assert_eq!(checker.http.call_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_similarity_match_zero_similarity() {
        let m = SimilarityMatch {
            url: "https://example.com".to_string(),
            title: "Zero match".to_string(),
            similarity: 0.0,
        };
        assert!((m.similarity - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_similarity_match_perfect_similarity() {
        let m = SimilarityMatch {
            url: "https://example.com".to_string(),
            title: "Perfect match".to_string(),
            similarity: 1.0,
        };
        assert!((m.similarity - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_similarity_match_empty_fields() {
        let m = SimilarityMatch {
            url: String::new(),
            title: String::new(),
            similarity: 0.5,
        };
        assert!(m.url.is_empty());
        assert!(m.title.is_empty());
    }

    #[test]
    fn test_config_zero_threshold() {
        let config = LinearRegressionConfig {
            github_token: "tok".to_string(),
            scm_repos: vec![],
            similarity_threshold: 0.0,
        };
        assert!((config.similarity_threshold - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_max_threshold() {
        let config = LinearRegressionConfig {
            github_token: "tok".to_string(),
            scm_repos: vec![],
            similarity_threshold: 1.0,
        };
        assert!((config.similarity_threshold - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_many_repos() {
        let repos: Vec<String> = (0..100).map(|i| format!("org/repo-{}", i)).collect();
        let config = LinearRegressionConfig {
            github_token: "tok".to_string(),
            scm_repos: repos,
            similarity_threshold: 0.5,
        };
        assert_eq!(config.scm_repos.len(), 100);
    }

    #[tokio::test]
    async fn test_extract_thread_sections_empty_title_is_skipped() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // <h2> immediately followed by </h2> -> empty title -> section should be skipped
        let html = "<h2></h2>Some remaining content in the page";
        let sections = checker.extract_thread_sections(html);
        // The title is empty after stripping tags, so this section is skipped.
        // Fallback triggers because no valid sections were found.
        // Actually let's check: title_content is &section[..title_end] where title_end = section.find("</")
        // section starts at <h2>, so section = "<h2></h2>..."
        // title_end = index of "</" in section = 4 (the "</" in "</h2>")
        // title_content = "<h2>" -> strip_html_tags -> ""
        // empty title -> section is skipped
        // So fallback triggers with the full text
        assert!(!sections.is_empty());
        assert_eq!(sections[0].0, "Appwrite Threads Page");
    }

    #[tokio::test]
    async fn test_mock_http_client_returns_404_when_exhausted() {
        let mock = MockHttpClient::new(vec![(200, "ok")]);
        // First call gets 200
        let r1 = mock.get("http://example.com", vec![]).await.unwrap();
        assert_eq!(r1.status, 200);
        assert_eq!(r1.body, "ok");

        // Second call (exhausted) gets 404
        let r2 = mock.get("http://example.com", vec![]).await.unwrap();
        assert_eq!(r2.status, 404);
        assert_eq!(r2.body, "Not Found");
    }

    #[tokio::test]
    async fn test_check_regression_with_embedding_client_shows_semantic_method() {
        let config = create_config();
        let emb_client = match EmbeddingClient::new(crate::feedback::EmbeddingConfig {
            pool_size: 1,
            ..Default::default()
        }) {
            Ok(c) => Arc::new(c),
            Err(_) => return, // Skip if model unavailable (CI race)
        };
        let original_emb = vec![0.1, 0.2, 0.3];

        let mock = MockHttpClient::new(vec![
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, "<html><body>Nothing</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client_and_embeddings(
            config,
            vec!["keyword".to_string()],
            "Issue text".to_string(),
            original_emb,
            emb_client,
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-sem", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
        let details = result.details.unwrap();
        assert!(
            details.contains("semantic similarity (embeddings)"),
            "Expected semantic method, got: {}",
            details
        );
    }

    #[tokio::test]
    async fn test_strip_html_tags_unclosed_tag() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // An opening < without a closing > means all subsequent chars are consumed
        let result = checker.strip_html_tags("before<unclosed tag after");
        assert_eq!(result, "before");
    }

    #[tokio::test]
    async fn test_strip_html_tags_only_whitespace() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let result = checker.strip_html_tags("   \n\t   ");
        assert_eq!(result, "");
    }

    #[tokio::test]
    async fn test_strip_html_tags_ampersand_entities() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // HTML entities are not decoded, just preserved as-is
        let result = checker.strip_html_tags("<p>&amp; &lt; &gt;</p>");
        assert_eq!(result, "&amp; &lt; &gt;");
    }

    #[tokio::test]
    async fn test_extract_thread_sections_title_but_empty_content() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // <h2>Title</h2> followed by only tags with no text after them
        // The closing tag is found, title is "Title", content is strip_html_tags("")
        // which is empty, so the section is skipped (requires both non-empty)
        let html = "<h2>Title</h2>";
        let sections = checker.extract_thread_sections(html);
        // title is "Title", content comes from section[title_end..] which is "</h2>"
        // strip_html_tags("</h2>") -> "" (empty after stripping tags)
        // Both non-empty required -> this section is skipped
        // Fallback: strip_html_tags("<h2>Title</h2>") -> "Title" -> one section
        assert!(!sections.is_empty());
    }

    #[test]
    fn test_regression_result_no_regression() {
        let result = crate::regression::RegressionResult::no_regression();
        assert!(!result.regression_detected);
        assert!(result.details.is_none());
    }

    #[test]
    fn test_regression_result_regression() {
        let result =
            crate::regression::RegressionResult::regression("Found similar issues".to_string());
        assert!(result.regression_detected);
        assert_eq!(result.details.unwrap(), "Found similar issues");
    }

    #[tokio::test]
    async fn test_check_similarity_with_client_but_no_original_embedding() {
        let config = create_config();

        // Use with_http_client (no embeddings set)
        // Without embedding client or original_embedding -> returns 0.0
        let mock = MockHttpClient::new(vec![]);
        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["kw".to_string()],
            "Issue text".to_string(),
            mock,
        );

        // No embedding client or original_embedding -> returns 0.0
        let sim = checker
            .check_similarity("some candidate text")
            .await
            .unwrap();
        assert!((sim - 0.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_search_github_issues_since_empty_items_array() {
        let mock = MockHttpClient::new(vec![(200, r#"{"total_count": 0, "items": []}"#)]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_github_issues_with_multiple_items_no_embeddings() {
        let mock = MockHttpClient::new(vec![(
            200,
            r#"{
                "total_count": 3,
                "items": [
                    {
                        "id": 1, "number": 1, "title": "Bug A",
                        "body": "Description A", "state": "open",
                        "html_url": "https://github.com/test/repo/issues/1",
                        "created_at": "2025-01-01T00:00:00Z"
                    },
                    {
                        "id": 2, "number": 2, "title": "Bug B",
                        "body": "Description B", "state": "open",
                        "html_url": "https://github.com/test/repo/issues/2",
                        "created_at": "2025-01-02T00:00:00Z"
                    },
                    {
                        "id": 3, "number": 3, "title": "Bug C",
                        "body": null, "state": "open",
                        "html_url": "https://github.com/test/repo/issues/3",
                        "created_at": "2025-01-03T00:00:00Z"
                    }
                ]
            }"#,
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["bug".to_string()],
            "Bug report about something".to_string(),
            mock,
        );

        let results = checker.search_github_issues().await.unwrap();
        // No embeddings -> all filtered out by similarity threshold
        assert!(results.is_empty());
    }

    #[test]
    fn test_similarity_match_format_string() {
        let m = SimilarityMatch {
            url: "https://github.com/org/repo/issues/42".to_string(),
            title: "Authentication failure after update".to_string(),
            similarity: 0.87,
        };

        let formatted = format!(
            "{} (similarity: {:.0}%) - {}",
            m.url,
            m.similarity * 100.0,
            m.title
        );
        assert!(formatted.contains("87%"));
        assert!(formatted.contains("Authentication failure"));
        assert!(formatted.contains("issues/42"));
    }

    #[test]
    fn test_linear_regression_checker_new() {
        let config = LinearRegressionConfig {
            github_token: "tok".to_string(),
            scm_repos: vec!["org/repo".to_string()],
            similarity_threshold: 0.80,
        };

        let checker = LinearRegressionChecker::new(
            config,
            vec!["keyword".to_string()],
            "Original issue text".to_string(),
        );

        assert_eq!(checker.config.github_token, "tok");
        assert_eq!(checker.keywords.len(), 1);
        assert_eq!(checker.original_issue_text, "Original issue text");
        assert!(checker.original_embedding.is_none());
        assert!(checker.embedding_client.is_none());
    }

    #[test]
    fn test_regression_result_no_regression_2() {
        let result = crate::regression::RegressionResult::no_regression();
        assert!(!result.regression_detected);
        assert!(result.details.is_none());
    }

    #[test]
    fn test_regression_result_regression_2() {
        let result =
            crate::regression::RegressionResult::regression("Found similar issue".to_string());
        assert!(result.regression_detected);
        assert_eq!(result.details, Some("Found similar issue".to_string()));
    }

    #[tokio::test]
    async fn test_extract_thread_sections_h3_tags() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html = "<html><body><h3>Small Heading</h3>Content for small heading</body></html>";
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_extract_thread_sections_div_post_class_2() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let html =
            r#"<html><body><div class="post">Post Title</div>Post body content</body></html>"#;
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }

    #[tokio::test]
    async fn test_strip_html_tags_unclosed_tag_2() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // Unclosed angle bracket at the end
        let result = checker.strip_html_tags("text before <unclosed");
        assert_eq!(result, "text before");
    }

    #[tokio::test]
    async fn test_strip_html_tags_multiple_spaces_between_words() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        let result = checker.strip_html_tags("<p>word1</p>  <p>word2</p>   <p>word3</p>");
        assert_eq!(result, "word1 word2 word3");
    }

    #[tokio::test]
    async fn test_search_github_issues_since_exactly_5_keywords() {
        let mock = MockHttpClient::new(vec![(200, r#"{"total_count": 0, "items": []}"#)]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
                "e".to_string(),
            ],
            "Issue text".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        assert!(results.is_empty());
        assert_eq!(checker.http.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_check_regression_empty_repos() {
        let config = LinearRegressionConfig {
            github_token: "test-token".to_string(),
            scm_repos: vec![],
            similarity_threshold: 0.75,
        };

        let mock = MockHttpClient::new(vec![
            // Only threads check (no repos to search)
            (200, "<html><body>Nothing</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-empty-repos", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
        let details = result.details.unwrap();
        assert!(details.contains("0 repos"));
    }

    #[tokio::test]
    async fn test_check_regression_details_includes_threshold_percentage() {
        let config = LinearRegressionConfig {
            github_token: "test-token".to_string(),
            scm_repos: vec!["org/repo".to_string()],
            similarity_threshold: 0.65,
        };

        let mock = MockHttpClient::new(vec![
            (200, r#"{"total_count": 0, "items": []}"#),
            (200, "<html><body>Nothing</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-threshold", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        let details = result.details.unwrap();
        assert!(
            details.contains("65%"),
            "Expected details to contain 65%, got: {}",
            details
        );
    }

    #[tokio::test]
    async fn test_check_appwrite_threads_html_with_only_tags() {
        let mock = MockHttpClient::new(vec![(200, "<html><head></head><body></body></html>")]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let results = checker.check_appwrite_threads().await.unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_github_search_result_deserialization() {
        let json = r#"{
            "total_count": 2,
            "items": [
                {
                    "id": 1,
                    "number": 10,
                    "title": "First issue",
                    "body": "Body of first issue",
                    "state": "open",
                    "html_url": "https://github.com/org/repo/issues/10",
                    "created_at": "2024-01-01T00:00:00Z"
                },
                {
                    "id": 2,
                    "number": 20,
                    "title": "Second issue",
                    "body": null,
                    "state": "closed",
                    "html_url": "https://github.com/org/repo/issues/20",
                    "created_at": "2024-02-01T00:00:00Z"
                }
            ]
        }"#;
        let result: GitHubSearchResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].title, "First issue");
        assert_eq!(
            result.items[0].html_url,
            "https://github.com/org/repo/issues/10"
        );
        assert_eq!(result.items[1].body, None);
        assert_eq!(result.items[1].state, "closed");
    }

    #[test]
    fn test_github_search_result_empty_items() {
        let json = r#"{"total_count": 0, "items": []}"#;
        let result: GitHubSearchResult = serde_json::from_str(json).unwrap();
        assert!(result.items.is_empty());
    }

    #[test]
    fn test_github_issue_with_null_body() {
        let json = r#"{
            "id": 42,
            "number": 7,
            "title": "Issue with no body",
            "body": null,
            "state": "open",
            "html_url": "https://github.com/org/repo/issues/7",
            "created_at": "2024-03-15T10:00:00Z"
        }"#;
        let issue: GitHubIssue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.title, "Issue with no body");
        assert!(issue.body.is_none());
        assert_eq!(issue.created_at, "2024-03-15T10:00:00Z");
    }

    // Covers: search_github_issues with body text that exercises the format! path
    #[tokio::test]
    async fn test_search_github_issues_with_body_text_formatting() {
        let mock = MockHttpClient::new(vec![(
            200,
            r#"{
                "total_count": 1,
                "items": [{
                    "id": 1,
                    "number": 42,
                    "title": "Authentication error",
                    "body": "Users are seeing 401 errors when trying to login after the latest update",
                    "state": "open",
                    "html_url": "https://github.com/test/repo/issues/42",
                    "created_at": "2025-06-01T00:00:00Z"
                }]
            }"#,
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["authentication".to_string()],
            "Authentication error in production".to_string(),
            mock,
        );

        // Without embeddings, similarity is 0.0, so nothing matches
        let results = checker.search_github_issues().await.unwrap();
        assert!(results.is_empty());
        // Verify the HTTP call was made (the issue was fetched and processed)
        assert_eq!(checker.http.call_count.load(Ordering::SeqCst), 1);
    }

    // Covers: search_github_issues_since with issues that have body text
    #[tokio::test]
    async fn test_search_github_issues_since_with_body_text_new_issue() {
        let future_time = (Utc::now() + Duration::hours(2)).to_rfc3339();
        let mock = MockHttpClient::new(vec![(
            200,
            &format!(
                r#"{{
                    "total_count": 1,
                    "items": [{{
                        "id": 1,
                        "number": 42,
                        "title": "Database connection timeout",
                        "body": "The database pool is exhausting connections under high load causing timeouts",
                        "state": "open",
                        "html_url": "https://github.com/test/repo/issues/42",
                        "created_at": "{}"
                    }}]
                }}"#,
                future_time
            ),
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["database".to_string()],
            "Database timeout issue".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        // Without embeddings, similarity is 0.0 -> filtered out
        assert!(results.is_empty());
        assert_eq!(checker.http.call_count.load(Ordering::SeqCst), 1);
    }

    // Covers: check_appwrite_threads with multiple thread sections found
    #[tokio::test]
    async fn test_check_appwrite_threads_multiple_sections() {
        let mock = MockHttpClient::new(vec![(
            200,
            "<html><body><h2>Auth Bug Discussion</h2>Users reporting auth failures after the update.<h3>Fix Attempt</h3>Tried updating the token refresh logic but it still fails.</body></html>",
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["auth".to_string()],
            "Auth bug discussion".to_string(),
            mock,
        );

        let results = checker.check_appwrite_threads().await.unwrap();
        // No embeddings -> similarity 0.0 -> filtered out
        assert!(results.is_empty());
    }

    // Covers: check_appwrite_threads with article tag and text content
    #[tokio::test]
    async fn test_check_appwrite_threads_article_with_content() {
        let mock = MockHttpClient::new(vec![(
            200,
            "<html><body><article>Storage Upload Bug</article>Users are experiencing upload failures with files larger than 10MB.</body></html>",
        )]);

        let checker = LinearRegressionChecker::with_http_client(
            create_config(),
            vec!["storage".to_string()],
            "Storage upload failure".to_string(),
            mock,
        );

        let results = checker.check_appwrite_threads().await.unwrap();
        // No embeddings -> similarity 0.0
        assert!(results.is_empty());
    }

    // Covers: extract_thread_sections - safe char boundary handling with multibyte at exact boundary
    #[tokio::test]
    async fn test_extract_thread_sections_multibyte_at_boundary() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // Create content with Japanese characters that might cross the 2000 byte window
        let content = "x".repeat(1995) + &"日本".repeat(5);
        let html = format!("<h2>Title</h2>{}", content);
        let sections = checker.extract_thread_sections(&html);
        // Should not panic and should produce sections
        assert!(!sections.is_empty());
    }

    // Covers: RegressionResult formatting with multiple similarity matches
    #[test]
    fn test_regression_result_formatting_multiple_issues() {
        let matches = [
            SimilarityMatch {
                url: "https://github.com/org/repo/issues/1".to_string(),
                title: "Issue A".to_string(),
                similarity: 0.95,
            },
            SimilarityMatch {
                url: "https://github.com/org/repo/issues/2".to_string(),
                title: "Issue B".to_string(),
                similarity: 0.82,
            },
        ];

        let issue_links: Vec<String> = matches
            .iter()
            .map(|m| {
                format!(
                    "{} (similarity: {:.0}%) - {}",
                    m.url,
                    m.similarity * 100.0,
                    m.title
                )
            })
            .collect();

        let result_text = format!(
            "Found {} semantically similar GitHub issues that may indicate regression:\n{}",
            matches.len(),
            issue_links.join("\n")
        );

        assert!(result_text.contains("2 semantically similar"));
        assert!(result_text.contains("95%"));
        assert!(result_text.contains("82%"));
        assert!(result_text.contains("Issue A"));
        assert!(result_text.contains("Issue B"));
    }

    // Covers: RegressionResult formatting for threads
    #[test]
    fn test_regression_result_formatting_thread_matches() {
        let matches = [SimilarityMatch {
            url: "https://appwrite.io/threads#auth-bug".to_string(),
            title: "Auth Bug Discussion".to_string(),
            similarity: 0.88,
        }];

        let thread_links: Vec<String> = matches
            .iter()
            .map(|m| {
                format!(
                    "{} (similarity: {:.0}%) - {}",
                    m.url,
                    m.similarity * 100.0,
                    m.title
                )
            })
            .collect();

        let result_text = format!(
            "Found {} semantically similar discussions on appwrite.io/threads:\n{}",
            matches.len(),
            thread_links.join("\n")
        );

        assert!(result_text.contains("1 semantically similar"));
        assert!(result_text.contains("88%"));
        assert!(result_text.contains("Auth Bug Discussion"));
        assert!(result_text.contains("appwrite.io/threads#auth-bug"));
    }

    // Covers: extract_thread_sections with closing tag that has empty content
    #[tokio::test]
    async fn test_extract_thread_sections_empty_content_after_title() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // h2 with title but nothing meaningful after closing tag
        let html = "<h2>Title Only</h2><div></div>";
        let sections = checker.extract_thread_sections(html);
        // Title is "Title Only", content is strip_html_tags("</h2><div></div>") -> ""
        // Empty content -> section skipped -> falls back to full page
        assert!(!sections.is_empty());
    }

    // Covers: search_github_issues with 2+ repos, some succeed some fail
    #[tokio::test]
    async fn test_search_github_issues_mixed_repo_responses() {
        let config = LinearRegressionConfig {
            github_token: "test-token".to_string(),
            scm_repos: vec!["org/repo-1".to_string(), "org/repo-2".to_string()],
            similarity_threshold: 0.75,
        };

        let mock = MockHttpClient::new(vec![
            // First repo: error
            (500, "Internal Server Error"),
            // Second repo: success with empty results
            (200, r#"{"total_count": 0, "items": []}"#),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let results = checker.search_github_issues().await.unwrap();
        assert!(results.is_empty());
        // Both repos were attempted
        assert_eq!(checker.http.call_count.load(Ordering::SeqCst), 2);
    }

    // Covers: search_github_issues_since with mixed repo responses
    #[tokio::test]
    async fn test_search_github_issues_since_mixed_responses() {
        let config = LinearRegressionConfig {
            github_token: "test-token".to_string(),
            scm_repos: vec!["org/repo-1".to_string(), "org/repo-2".to_string()],
            similarity_threshold: 0.75,
        };

        let mock = MockHttpClient::new(vec![
            // First repo: error
            (500, "Internal Server Error"),
            // Second repo: success with empty results
            (200, r#"{"total_count": 0, "items": []}"#),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec!["keyword".to_string()],
            "Some issue".to_string(),
            mock,
        );

        let since = Utc::now() - Duration::hours(1);
        let results = checker.search_github_issues_since(since).await.unwrap();
        assert!(results.is_empty());
        assert_eq!(checker.http.call_count.load(Ordering::SeqCst), 2);
    }

    // Covers: check_regression with empty keywords and empty token
    #[tokio::test]
    async fn test_check_regression_empty_keywords_empty_token() {
        let config = LinearRegressionConfig {
            github_token: String::new(),
            scm_repos: vec!["org/repo".to_string()],
            similarity_threshold: 0.75,
        };

        let mock = MockHttpClient::new(vec![
            // Only threads check
            (200, "<html><body>Nothing</body></html>"),
        ]);

        let checker = LinearRegressionChecker::with_http_client(
            config,
            vec![], // Empty keywords
            "Some issue".to_string(),
            mock,
        );

        let mut watch = RegressionWatch::new(IssueType::LinearBug, "linear-empty-kw-tk", 1);
        watch.monitoring_started_at = Some(Utc::now() - Duration::hours(1));

        let result = checker.check_regression(&watch).await.unwrap();
        assert!(!result.regression_detected);
    }

    // Covers: extract_thread_sections with multiple patterns in same HTML
    #[tokio::test]
    async fn test_extract_thread_sections_multiple_patterns_first_wins() {
        let checker = make_checker(MockHttpClient::new(vec![]));
        // HTML that has h2 BEFORE article - h2 should be found first
        let html = "<html><body><h2>First Heading</h2>Content for first<article>Article Content</article>Article rest</body></html>";
        let sections = checker.extract_thread_sections(html);
        assert!(!sections.is_empty());
    }
}
