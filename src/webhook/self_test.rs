//! Self-test for the webhook server.
//!
//! Sends test payloads with valid HMAC signatures to each configured
//! source endpoint and verifies that the server accepts them.

use claudear_config::config::Config;
use claudear_core::secret::{OptionalSecretExt, SecretValue};
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::Sha256;
use std::fmt;

const TEST_SECRET: &str = "claudear_self_test_secret_do_not_use_in_production";

/// Inject temporary webhook secrets into configured sources that don't already
/// have one. This must be called BEFORE `create_webhook_handlers` so the handlers
/// are built with secrets and can verify signatures on test payloads.
pub fn inject_test_secrets(config: &mut Config) {
    let secret = SecretValue::new(TEST_SECRET);

    if let Some(ref mut sentry) = config.issues.sentry {
        if sentry.client_secret.is_none() {
            sentry.client_secret = Some(secret.clone());
            tracing::info!("Injected test secret for sentry");
        }
    }

    if let Some(ref mut linear) = config.issues.linear {
        if linear.webhook_secret.is_none() {
            linear.webhook_secret = Some(secret.clone());
            tracing::info!("Injected test secret for linear");
        }
    }

    let github = &mut config.scm.github;
    if github
        .webhook_secret
        .as_ref()
        .map(|s: &SecretValue| s.expose().is_empty())
        .unwrap_or(true)
    {
        github.webhook_secret = Some(secret.clone());
        tracing::info!("Injected test secret for github");
    }

    if let Some(ref mut jira) = config.issues.jira {
        // Jira doesn't use HMAC but we still mark it so the test runs
        let _ = jira;
        tracing::info!("Jira configured (no secret needed)");
    }

    if let Some(ref mut gitlab) = config.scm.gitlab {
        if gitlab
            .webhook_secret
            .as_ref()
            .map(|s: &SecretValue| s.expose().is_empty())
            .unwrap_or(true)
        {
            gitlab.webhook_secret = Some(secret.clone());
            tracing::info!("Injected test secret for gitlab");
        }
    }
}

struct TestResult {
    source: String,
    status: TestStatus,
    http_code: Option<u16>,
    detail: String,
}

enum TestStatus {
    Pass,
    Fail,
    Skip,
}

impl fmt::Display for TestStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestStatus::Pass => write!(f, "PASS"),
            TestStatus::Fail => write!(f, "FAIL"),
            TestStatus::Skip => write!(f, "SKIP"),
        }
    }
}

fn sign_hmac_sha256(secret: &str, payload: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Build a test payload and headers for each source.
/// Returns `None` when the source is not configured (no webhook secret).
fn build_sentry_test(config: &Config) -> Option<(String, Vec<(String, String)>)> {
    let sentry_cfg = config.sentry_config()?;
    let secret = sentry_cfg.client_secret.as_ref()?;

    let now = now_rfc3339();
    let body = serde_json::json!({
        "action": "created",
        "data": {
            "issue": {
                "id": "SELF-TEST-1",
                "shortId": "TEST-1",
                "title": "Self-test issue",
                "project": { "slug": "test", "name": "test" },
                "status": "unresolved",
                "level": "error",
                "count": "100",
                "userCount": 1,
                "metadata": {},
                "firstSeen": now,
                "lastSeen": now
            }
        }
    });
    let body_str = serde_json::to_string(&body).unwrap();
    let sig = sign_hmac_sha256(secret.expose(), body_str.as_bytes());

    let headers = vec![
        ("sentry-hook-signature".to_string(), sig),
        ("sentry-hook-resource".to_string(), "event".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    Some((body_str, headers))
}

fn build_linear_test(config: &Config) -> Option<(String, Vec<(String, String)>)> {
    let linear_cfg = config.linear()?;
    let secret = linear_cfg.webhook_secret.as_ref()?;

    let now = now_rfc3339();
    let body = serde_json::json!({
        "action": "create",
        "type": "Issue",
        "data": {
            "id": "self-test-1",
            "identifier": "TEST-1",
            "title": "Self-test issue",
            "url": "https://linear.app/test/issue/TEST-1",
            "state": { "name": "Triage" },
            "team": { "id": "test-team", "key": "TEST" },
            "labels": { "nodes": [{ "name": "bug" }] },
            "priority": 2,
            "createdAt": now,
            "updatedAt": now
        }
    });
    let body_str = serde_json::to_string(&body).unwrap();
    let sig = sign_hmac_sha256(secret.expose(), body_str.as_bytes());

    let headers = vec![
        ("linear-signature".to_string(), sig),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    Some((body_str, headers))
}

fn build_github_test(config: &Config) -> Option<(String, Vec<(String, String)>)> {
    let github_cfg = config.github();
    let secret = github_cfg
        .webhook_secret
        .as_ref()
        .filter(|s| !s.expose().is_empty())
        .or_else(|| {
            github_cfg
                .app
                .webhook_secret
                .as_ref()
                .filter(|s| !s.expose().is_empty())
        })?;

    let now = now_rfc3339();
    let body = serde_json::json!({
        "action": "submitted",
        "review": {
            "id": 1,
            "state": "commented",
            "body": "Self-test review",
            "user": { "id": 1, "login": "test-user" },
            "submitted_at": now,
            "html_url": "https://github.com/test/test/pull/1#pullrequestreview-1"
        },
        "pull_request": {
            "html_url": "https://github.com/test/test/pull/1"
        }
    });
    let body_str = serde_json::to_string(&body).unwrap();
    let sig = format!(
        "sha256={}",
        sign_hmac_sha256(secret.expose(), body_str.as_bytes())
    );

    let headers = vec![
        ("x-hub-signature-256".to_string(), sig),
        (
            "x-github-event".to_string(),
            "pull_request_review".to_string(),
        ),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    Some((body_str, headers))
}

fn build_jira_test(config: &Config) -> Option<(String, Vec<(String, String)>)> {
    // Jira webhooks do not use HMAC signing in the current implementation
    // (verify_signature always returns true), so we just need a valid payload.
    let _jira_cfg = config.jira()?;

    let now = now_rfc3339();
    let body = serde_json::json!({
        "webhookEvent": "jira:issue_created",
        "issue": {
            "key": "SELF-TEST-1",
            "fields": {
                "summary": "Self-test issue",
                "status": { "name": "Open" },
                "created": now,
                "updated": now
            }
        }
    });
    let body_str = serde_json::to_string(&body).unwrap();

    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    Some((body_str, headers))
}

fn build_gitlab_test(config: &Config) -> Option<(String, Vec<(String, String)>)> {
    let gitlab_cfg = config.gitlab()?;
    let secret = gitlab_cfg.webhook_secret.expose_as_deref()?;

    let now = now_rfc3339();
    let body = serde_json::json!({
        "object_kind": "issue",
        "event_type": "issue",
        "object_attributes": {
            "iid": 1,
            "title": "Self-test issue",
            "description": "Webhook self-test",
            "state": "opened",
            "action": "open",
            "url": "https://gitlab.com/test/test/-/issues/1",
            "created_at": now,
            "updated_at": now
        },
        "project": {
            "path_with_namespace": "test/test"
        },
        "labels": []
    });
    let body_str = serde_json::to_string(&body).unwrap();

    // GitLab uses a plain token header, not HMAC
    let headers = vec![
        ("x-gitlab-token".to_string(), secret.to_string()),
        ("x-gitlab-event".to_string(), "Issue Hook".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    Some((body_str, headers))
}

/// Wait for the webhook server health endpoint to respond.
async fn wait_for_healthy(client: &Client, base_url: &str) -> anyhow::Result<()> {
    let health_url = format!("{}/health", base_url);
    let mut attempts = 0;
    loop {
        match client.get(&health_url).send().await {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            _ => {
                attempts += 1;
                if attempts > 30 {
                    anyhow::bail!("Webhook server did not become healthy after 30 attempts");
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            }
        }
    }
}

async fn send_test(
    client: &Client,
    base_url: &str,
    source: &str,
    body: &str,
    headers: &[(String, String)],
) -> TestResult {
    let url = format!("{}/webhook/{}", base_url, source);
    let mut req = client.post(&url).body(body.to_string());
    for (key, value) in headers {
        req = req.header(key.as_str(), value.as_str());
    }

    match req.send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            let resp_body = resp.text().await.unwrap_or_default();
            let status = match code {
                200 | 202 => TestStatus::Pass,
                401 => TestStatus::Fail,
                404 => TestStatus::Fail,
                _ => TestStatus::Fail,
            };
            let detail = match code {
                200 | 202 => "accepted".to_string(),
                401 => format!("signature rejected: {}", resp_body),
                404 => format!("not found: {}", resp_body),
                _ => format!("unexpected response: {}", resp_body),
            };
            TestResult {
                source: source.to_string(),
                status,
                http_code: Some(code),
                detail,
            }
        }
        Err(e) => TestResult {
            source: source.to_string(),
            status: TestStatus::Fail,
            http_code: None,
            detail: format!("request error: {}", e),
        },
    }
}

fn print_results(results: &[TestResult]) {
    let source_width = results
        .iter()
        .map(|r| r.source.len())
        .max()
        .unwrap_or(10)
        .max(6);
    let status_width = 4;
    let code_width = 4;

    println!();
    let header = "DETAIL";
    let separator = "-".repeat(30);
    println!(
        "  {:<sw$}  {:<stw$}  {:<cw$}  {header}",
        "SOURCE",
        "RESULT",
        "CODE",
        sw = source_width,
        stw = status_width,
        cw = code_width,
    );
    println!(
        "  {:<sw$}  {:<stw$}  {:<cw$}  {separator}",
        "-".repeat(source_width),
        "-".repeat(status_width),
        "-".repeat(code_width),
        sw = source_width,
        stw = status_width,
        cw = code_width,
    );

    for r in results {
        let code_str = r
            .http_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<sw$}  {:<stw$}  {:<cw$}  {}",
            r.source,
            r.status.to_string(),
            code_str,
            r.detail,
            sw = source_width,
            stw = status_width,
            cw = code_width,
        );
    }
    println!();
}

/// Run the self-test against a running webhook server.
pub async fn run(port: u16, config: &Config) -> anyhow::Result<()> {
    let base_url = format!("http://127.0.0.1:{}", port);
    let client = Client::new();

    tracing::info!("Waiting for webhook server to be healthy...");
    wait_for_healthy(&client, &base_url).await?;
    tracing::info!("Webhook server is healthy, running self-test...");

    type TestPayload = Option<(String, Vec<(String, String)>)>;
    let sources: Vec<(&str, TestPayload)> = vec![
        ("sentry", build_sentry_test(config)),
        ("linear", build_linear_test(config)),
        ("github", build_github_test(config)),
        ("jira", build_jira_test(config)),
        ("gitlab", build_gitlab_test(config)),
    ];

    let mut results = Vec::new();
    let mut any_failed = false;

    for (source, test_data) in &sources {
        match test_data {
            Some((body, headers)) => {
                let result = send_test(&client, &base_url, source, body, headers).await;
                if matches!(result.status, TestStatus::Fail) {
                    any_failed = true;
                }
                results.push(result);
            }
            None => {
                results.push(TestResult {
                    source: source.to_string(),
                    status: TestStatus::Skip,
                    http_code: None,
                    detail: "not configured".to_string(),
                });
            }
        }
    }

    print_results(&results);

    let passed = results
        .iter()
        .filter(|r| matches!(r.status, TestStatus::Pass))
        .count();
    let skipped = results
        .iter()
        .filter(|r| matches!(r.status, TestStatus::Skip))
        .count();
    let failed = results
        .iter()
        .filter(|r| matches!(r.status, TestStatus::Fail))
        .count();

    println!(
        "  Summary: {} passed, {} skipped, {} failed",
        passed, skipped, failed
    );
    println!();

    if any_failed {
        anyhow::bail!("{} test(s) failed", failed);
    }

    Ok(())
}
