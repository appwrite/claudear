//! Daemon lifecycle management for E2E tests.
//!
//! Supports both native binary and Docker container modes.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Handle to a running daemon instance.
pub enum DaemonHandle {
    Process {
        child: Child,
        log_path: PathBuf,
    },
    Docker {
        container_id: String,
        volume_name: String,
        log_path: PathBuf,
    },
}

impl DaemonHandle {
    pub fn log_path(&self) -> &Path {
        match self {
            DaemonHandle::Process { log_path, .. } => log_path,
            DaemonHandle::Docker { log_path, .. } => log_path,
        }
    }

    pub fn volume_name(&self) -> Option<&str> {
        match self {
            DaemonHandle::Docker { volume_name, .. } => Some(volume_name),
            _ => None,
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        stop(self);
    }
}

/// Start a native daemon process.
pub fn start_process(
    binary: &str,
    config_path: &Path,
    port: u16,
    log_dir: &Path,
    label: &str,
) -> Result<DaemonHandle> {
    std::fs::create_dir_all(log_dir)?;
    let log_path = log_dir.join(format!("{}.log", label));
    let log_file = std::fs::File::create(&log_path)?;
    let log_stderr = log_file.try_clone()?;

    let child = Command::new(binary)
        .args([
            "--config",
            config_path.to_str().unwrap_or(""),
            "--verbose",
            "start",
            "--foreground",
            "--poll",
            "--poll-interval",
            "5000",
            "--port",
            &port.to_string(),
            "--no-webhooks",
        ])
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_stderr))
        .spawn()
        .context("spawn claudear daemon")?;

    tracing::info!(pid = child.id(), port, label, "Started daemon process");
    Ok(DaemonHandle::Process { child, log_path })
}

/// Extract the access token from a Claude Code credentials JSON string.
///
/// Tries serde_json first, then falls back to regex for malformed JSON
/// (e.g. control characters from macOS keychain).
fn extract_token_from_creds(creds_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(creds_json)
        .ok()
        .and_then(|v| {
            v.get("claudeAiOauth")?
                .get("accessToken")?
                .as_str()
                .map(|s| s.to_string())
        })
        .or_else(|| {
            let re = regex_lite::Regex::new(r#""accessToken"\s*:\s*"([^"]+)""#).ok()?;
            re.captures(creds_json)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
        })
        .filter(|t| !t.is_empty())
}

/// Try to extract Claude Code OAuth token from the macOS keychain.
fn get_keychain_oauth_token() -> Option<String> {
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let creds_json = String::from_utf8(output.stdout).ok()?;
    let creds_json = creds_json.trim();
    if creds_json.is_empty() {
        return None;
    }

    let token = extract_token_from_creds(creds_json)?;

    tracing::info!("Extracted Claude Code OAuth token from macOS keychain");
    Some(token)
}

/// Start a Docker daemon container.
///
/// When `reset_volume` is true the named volume is destroyed and recreated so
/// the daemon starts with a clean database.  Pass `false` when restarting
/// mid-scenario to preserve DB state.
#[expect(clippy::too_many_arguments)]
pub fn start_docker(
    image: &str,
    config_path: &Path,
    port: u16,
    log_dir: &Path,
    label: &str,
    volume_name: Option<&str>,
    repos_dir: Option<&Path>,
    reset_volume: bool,
) -> Result<DaemonHandle> {
    std::fs::create_dir_all(log_dir)?;
    let log_path = log_dir.join(format!("{}.log", label));

    let default_vol = format!("claudear-e2e-db-{}", port);
    let vol = volume_name.unwrap_or(&default_vol);

    // Remove any stale container with the same name from a previous run.
    // Must happen BEFORE volume removal — a running container holds a reference
    // to the volume, causing `docker volume rm` to fail silently.
    let container_name = format!("claudear-e2e-{}", label);
    let _ = Command::new("docker")
        .args(["rm", "-f", &container_name])
        .output();

    if reset_volume {
        // Remove stale volume from previous run (ignore errors if it doesn't exist)
        let _ = Command::new("docker")
            .args(["volume", "rm", "-f", vol])
            .output();
    }

    // Ensure volume exists
    let _ = Command::new("docker")
        .args(["volume", "create", vol])
        .output();

    // Build docker args dynamically
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        format!("claudear-e2e-{}", label),
        "-p".to_string(),
        format!("{}:{}", port, port),
        "-v".to_string(),
        format!("{}:/app/config.toml:ro", config_path.display()),
        "-v".to_string(),
        format!("{}:/app/data", vol),
    ];

    // Mount host repos dir into container
    if let Some(repos) = repos_dir {
        args.extend(["-v".to_string(), format!("{}:/app/repos", repos.display())]);
    }

    // Pass through env vars for Claude auth
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            args.extend(["-e".to_string(), format!("ANTHROPIC_API_KEY={}", key)]);
        }
    }

    // Pass GitHub token for gh CLI usage inside container
    if let Ok(gh_token) = std::env::var("CLAUDEAR_E2E_GITHUB_TOKEN") {
        if !gh_token.is_empty() {
            args.extend(["-e".to_string(), format!("GH_TOKEN={}", gh_token)]);
        }
    }

    // Try CLAUDE_CODE_OAUTH_TOKEN from env, then fall back to macOS keychain
    let oauth_token = std::env::var("CLAUDE_CODE_OAUTH_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .or_else(get_keychain_oauth_token);
    if let Some(token) = oauth_token {
        args.extend([
            "-e".to_string(),
            format!("CLAUDE_CODE_OAUTH_TOKEN={}", token),
        ]);
    }

    args.extend([
        image.to_string(),
        "claudear".to_string(),
        "--config".to_string(),
        "/app/config.toml".to_string(),
        "--verbose".to_string(),
        "start".to_string(),
        "--poll".to_string(),
        "--poll-interval".to_string(),
        "5000".to_string(),
        "--port".to_string(),
        port.to_string(),
        "--no-webhooks".to_string(),
    ]);

    let container_id = {
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = Command::new("docker")
            .args(&arg_refs)
            .output()
            .context("docker run")?;

        if !output.status.success() {
            bail!(
                "Docker run failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    // Stream Docker container logs to the log file so wait_for_log_message works.
    {
        let log_file = std::fs::File::create(&log_path).context("create docker log file")?;
        let log_stderr = log_file.try_clone().context("clone docker log file")?;
        let cid = container_id.clone();
        std::thread::spawn(move || {
            let _ = Command::new("docker")
                .args(["logs", "-f", &cid])
                .stdout(log_file)
                .stderr(log_stderr)
                .status();
        });
    }

    // Verify repos bind mount is accessible inside the container
    if repos_dir.is_some() {
        let check = Command::new("docker")
            .args(["exec", &container_id, "test", "-d", "/app/repos"])
            .output();
        match check {
            Ok(output) if !output.status.success() => {
                tracing::warn!(
                    container = %container_id,
                    repos_dir = ?repos_dir,
                    "Repos bind mount verification failed: /app/repos not accessible inside container"
                );
            }
            Err(e) => {
                tracing::warn!(
                    container = %container_id,
                    error = %e,
                    "Failed to verify repos bind mount"
                );
            }
            _ => {
                tracing::info!(container = %container_id, "Verified /app/repos mount");
            }
        }
    }

    tracing::info!(container = %container_id, port, label, "Started Docker container");
    Ok(DaemonHandle::Docker {
        container_id,
        volume_name: vol.to_string(),
        log_path,
    })
}

/// Stop a daemon instance.
pub fn stop(handle: &mut DaemonHandle) {
    match handle {
        DaemonHandle::Process { child, .. } => {
            let _ = child.kill();
            let _ = child.wait();
        }
        DaemonHandle::Docker { container_id, .. } => {
            let _ = Command::new("docker").args(["stop", container_id]).output();
            let _ = Command::new("docker")
                .args(["rm", "-f", container_id])
                .output();
        }
    }
}

fn tail_text_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn log_tail(path: &Path, max_lines: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let tail = tail_text_lines(&content, max_lines);
            if tail.trim().is_empty() {
                "<log file is empty>".to_string()
            } else {
                tail
            }
        }
        Err(e) => format!("<failed to read log {}: {}>", path.display(), e),
    }
}

fn docker_state(container_id: &str) -> Option<String> {
    let output = Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{.State.Status}} exit={{.State.ExitCode}} oom={{.State.OOMKilled}} error={{.State.Error}}",
            container_id,
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn docker_logs_tail(container_id: &str, max_lines: usize) -> Option<String> {
    let output = Command::new("docker")
        .args(["logs", "--tail", &max_lines.to_string(), container_id])
        .output()
        .ok()?;

    let mut combined = String::new();
    if !output.stdout.is_empty() {
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    let trimmed = combined.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn health_timeout_diagnostics(handle: &DaemonHandle) -> String {
    let mut parts = Vec::new();
    parts.push(format!("log_tail:\n{}", log_tail(handle.log_path(), 80)));

    if let DaemonHandle::Docker { container_id, .. } = handle {
        if let Some(state) = docker_state(container_id) {
            parts.push(format!("docker_state: {}", state));
        }
        if let Some(logs) = docker_logs_tail(container_id, 80) {
            parts.push(format!("docker_logs_tail:\n{}", logs));
        }
    }

    parts.join("\n\n")
}

/// Wait for the daemon's health endpoint to respond.
pub async fn wait_healthy(handle: &DaemonHandle, port: u16, timeout: Duration) -> Result<()> {
    let url = format!("http://127.0.0.1:{}/api/health", port);
    let client = reqwest::Client::new();
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            let diagnostics = health_timeout_diagnostics(handle);
            bail!(
                "Daemon on port {} did not become healthy within {:?}\n\n{}",
                port,
                timeout,
                diagnostics
            );
        }

        match client.get(&url).send().await {
            Ok(_) => return Ok(()), // Any HTTP response means the server is up
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_token_valid_json() {
        let json =
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-test-token-123","refreshToken":"rt-456"}}"#;
        let token = extract_token_from_creds(json);
        assert_eq!(token.as_deref(), Some("sk-ant-test-token-123"));
    }

    #[test]
    fn test_extract_token_valid_json_extra_fields() {
        let json = r#"{"claudeAiOauth":{"accessToken":"sk-abc","refreshToken":"rt","expiresAt":"2026-01-01"},"otherKey":"value"}"#;
        let token = extract_token_from_creds(json);
        assert_eq!(token.as_deref(), Some("sk-abc"));
    }

    #[test]
    fn test_extract_token_malformed_json_with_control_char() {
        // Simulate the real-world case: JSON with a control character that breaks serde
        let json = "{\"claudeAiOauth\":{\"accessToken\":\"sk-ant-real-token\",\"refreshToken\":\"rt\"},\"broken\":\"\x01\"}";
        let token = extract_token_from_creds(json);
        // serde_json may fail, but regex fallback should find the token
        assert_eq!(token.as_deref(), Some("sk-ant-real-token"));
    }

    #[test]
    fn test_extract_token_truncated_json() {
        // JSON cut off mid-string — serde will fail, regex should still find accessToken
        let json = r#"{"claudeAiOauth":{"accessToken":"sk-ant-partial","refresh"#;
        let token = extract_token_from_creds(json);
        assert_eq!(token.as_deref(), Some("sk-ant-partial"));
    }

    #[test]
    fn test_extract_token_empty_access_token() {
        let json = r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"rt"}}"#;
        let token = extract_token_from_creds(json);
        assert!(token.is_none(), "Empty token should return None");
    }

    #[test]
    fn test_extract_token_missing_access_token_field() {
        let json = r#"{"claudeAiOauth":{"refreshToken":"rt-only"}}"#;
        let token = extract_token_from_creds(json);
        assert!(token.is_none());
    }

    #[test]
    fn test_extract_token_missing_claude_ai_oauth() {
        let json = r#"{"someOtherKey":{"accessToken":"sk-wrong-path"}}"#;
        let token = extract_token_from_creds(json);
        // serde path fails (no claudeAiOauth), regex finds "accessToken" anyway
        assert_eq!(token.as_deref(), Some("sk-wrong-path"));
    }

    #[test]
    fn test_extract_token_completely_invalid() {
        let token = extract_token_from_creds("not json at all");
        assert!(token.is_none());
    }

    #[test]
    fn test_extract_token_empty_string() {
        let token = extract_token_from_creds("");
        assert!(token.is_none());
    }

    #[test]
    fn test_extract_token_nested_quotes_in_token() {
        // Token shouldn't contain quotes — regex stops at first "
        let json = r#"{"claudeAiOauth":{"accessToken":"sk-ant-abc123"}}"#;
        let token = extract_token_from_creds(json);
        assert_eq!(token.as_deref(), Some("sk-ant-abc123"));
    }

    #[test]
    fn test_extract_token_whitespace_around_colon() {
        // Regex allows \s* around the colon
        let json = r#"{"claudeAiOauth":{"accessToken" : "sk-ant-spaced"}}"#;
        let token = extract_token_from_creds(json);
        assert_eq!(token.as_deref(), Some("sk-ant-spaced"));
    }

    #[test]
    fn test_extract_token_regex_fallback_with_newlines() {
        let json = "{\n  \"claudeAiOauth\": {\n    \"accessToken\": \"sk-ant-newline\"\n  },\n  \"broken\": \"\x00\"\n}";
        let token = extract_token_from_creds(json);
        assert_eq!(token.as_deref(), Some("sk-ant-newline"));
    }
}
