//! Chunked and resumable GGUF model downloading.

use futures_util::StreamExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;

/// Progress tracker for an active download.
pub struct DownloadProgress {
    pub downloaded_bytes: AtomicU64,
    pub total_bytes: AtomicU64,
    pub completed: AtomicBool,
    pub failed: AtomicBool,
    pub error_message: Mutex<Option<String>>,
}

impl DownloadProgress {
    pub fn new() -> Self {
        Self {
            downloaded_bytes: AtomicU64::new(0),
            total_bytes: AtomicU64::new(0),
            completed: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            error_message: Mutex::new(None),
        }
    }

    /// Get download progress as a percentage (0-100).
    pub fn percent(&self) -> u8 {
        let total = self.total_bytes.load(Ordering::Relaxed);
        if total == 0 {
            return 0;
        }
        let downloaded = self.downloaded_bytes.load(Ordering::Relaxed);
        ((downloaded as f64 / total as f64) * 100.0).min(100.0) as u8
    }
}

impl Default for DownloadProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// Download a GGUF file from a URL to a target path.
///
/// Supports resuming partial downloads via HTTP Range headers.
/// Downloads to a `.part` file first, then renames on success.
pub async fn download_gguf(
    url: &str,
    target: &Path,
    progress: Arc<DownloadProgress>,
) -> Result<(), String> {
    // Create parent directory
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directory: {e}"))?;
    }

    let part_path = target.with_extension(
        target
            .extension()
            .map(|e| format!("{}.part", e.to_string_lossy()))
            .unwrap_or_else(|| "part".to_string()),
    );

    // Check if a partial download exists for resume
    let existing_size = tokio::fs::metadata(&part_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let client = claudear_core::tls::client_builder()
        .timeout(std::time::Duration::from_secs(3600))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    // Build request with optional Range header for resume
    let mut request = client.get(url);
    if existing_size > 0 {
        request = request.header("Range", format!("bytes={}-", existing_size));
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("Download request failed: {e}"))?;

    if !response.status().is_success() && response.status() != reqwest::StatusCode::PARTIAL_CONTENT
    {
        return Err(format!(
            "Download failed with status: {}",
            response.status()
        ));
    }

    let is_resume = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;

    // Determine total size
    let content_length = response.content_length().unwrap_or(0);
    let total_size = if is_resume {
        existing_size + content_length
    } else {
        content_length
    };

    progress.total_bytes.store(total_size, Ordering::Relaxed);

    // Open file for writing
    let file = if is_resume {
        // Append to existing partial file
        progress
            .downloaded_bytes
            .store(existing_size, Ordering::Relaxed);
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part_path)
            .await
            .map_err(|e| format!("Failed to open part file for append: {e}"))?
    } else {
        // Fresh start - truncate any existing partial file
        progress.downloaded_bytes.store(0, Ordering::Relaxed);
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&part_path)
            .await
            .map_err(|e| format!("Failed to create part file: {e}"))?
    };

    let mut writer = tokio::io::BufWriter::new(file);
    let mut stream = response.bytes_stream();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| {
            let msg = format!("Download stream error: {e}");
            progress.failed.store(true, Ordering::Relaxed);
            if let Ok(mut err) = progress.error_message.lock() {
                *err = Some(msg.clone());
            }
            msg
        })?;

        writer.write_all(&chunk).await.map_err(|e| {
            let msg = format!("Failed to write chunk: {e}");
            progress.failed.store(true, Ordering::Relaxed);
            if let Ok(mut err) = progress.error_message.lock() {
                *err = Some(msg.clone());
            }
            msg
        })?;

        progress
            .downloaded_bytes
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }

    // Flush and close
    writer.flush().await.map_err(|e| {
        let msg = format!("Failed to flush download: {e}");
        progress.failed.store(true, Ordering::Relaxed);
        if let Ok(mut err) = progress.error_message.lock() {
            *err = Some(msg.clone());
        }
        msg
    })?;
    drop(writer);

    // Rename .part to final path
    tokio::fs::rename(&part_path, target).await.map_err(|e| {
        let msg = format!("Failed to rename download: {e}");
        progress.failed.store(true, Ordering::Relaxed);
        if let Ok(mut err) = progress.error_message.lock() {
            *err = Some(msg.clone());
        }
        msg
    })?;

    progress.completed.store(true, Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_progress_percent() {
        let progress = DownloadProgress::new();
        assert_eq!(progress.percent(), 0);

        progress.total_bytes.store(1000, Ordering::Relaxed);
        progress.downloaded_bytes.store(500, Ordering::Relaxed);
        assert_eq!(progress.percent(), 50);

        progress.downloaded_bytes.store(1000, Ordering::Relaxed);
        assert_eq!(progress.percent(), 100);
    }

    #[test]
    fn test_download_progress_zero_total() {
        let progress = DownloadProgress::new();
        progress.downloaded_bytes.store(100, Ordering::Relaxed);
        assert_eq!(progress.percent(), 0);
    }

    #[test]
    fn test_download_progress_default() {
        let progress = DownloadProgress::default();
        assert!(!progress.completed.load(Ordering::Relaxed));
        assert!(!progress.failed.load(Ordering::Relaxed));
        assert_eq!(progress.downloaded_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(progress.total_bytes.load(Ordering::Relaxed), 0);
    }
}
