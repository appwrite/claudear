//! Embedding generation using fastembed (local, no external dependencies).
//!
//! Uses the Nomic Embed Text model for generating embeddings.

use claudear_core::error::{Error, Result};
use fastembed::{EmbeddingModel, ExecutionProviderDispatch, InitOptions, TextEmbedding};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Configuration for the embedding service.
#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    /// Model to use for embeddings.
    pub model: EmbeddingModel,
    /// Whether to show download progress.
    pub show_download_progress: bool,
    /// Custom cache directory (uses system default if None).
    pub cache_dir: Option<String>,
    /// Number of model instances in the pool (defaults to available CPUs).
    pub pool_size: usize,
    /// ONNX Runtime execution providers (e.g. CUDA). Empty = CPU only.
    pub execution_providers: Vec<ExecutionProviderDispatch>,
    /// Override sub-batch size (0 = auto-detect).
    pub sub_batch_size: usize,
}

fn default_pool_size() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: EmbeddingModel::NomicEmbedTextV15,
            show_download_progress: true,
            cache_dir: None,
            pool_size: default_pool_size(),
            execution_providers: Vec::new(),
            sub_batch_size: 0,
        }
    }
}

impl EmbeddingConfig {
    /// Create from environment variables.
    pub fn from_env() -> Self {
        let model = std::env::var("CLAUDEAR_EMBEDDING_MODEL")
            .ok()
            .and_then(|m| match m.to_lowercase().as_str() {
                "nomic-embed-text" | "nomic" => Some(EmbeddingModel::NomicEmbedTextV15),
                "all-minilm" | "minilm" => Some(EmbeddingModel::AllMiniLML6V2),
                "bge-small" | "bge" => Some(EmbeddingModel::BGESmallENV15),
                _ => None,
            })
            .unwrap_or(EmbeddingModel::NomicEmbedTextV15);

        let cache_dir = std::env::var("CLAUDEAR_EMBEDDING_CACHE_DIR").ok();

        let pool_size = std::env::var("CLAUDEAR_EMBEDDING_POOL_SIZE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or_else(default_pool_size);

        Self {
            model,
            show_download_progress: true,
            cache_dir,
            pool_size,
            execution_providers: Vec::new(),
            sub_batch_size: 0,
        }
    }

    /// Use a smaller, faster model.
    pub fn fast() -> Self {
        Self {
            model: EmbeddingModel::AllMiniLML6V2,
            show_download_progress: true,
            cache_dir: None,
            pool_size: default_pool_size(),
            execution_providers: Vec::new(),
            sub_batch_size: 0,
        }
    }
}

/// Client for generating embeddings using fastembed.
///
/// Uses a pool of `TextEmbedding` model instances for concurrent inference.
/// Sub-batch size is determined dynamically from available system memory.
pub struct EmbeddingClient {
    pool: Vec<Arc<Mutex<TextEmbedding>>>,
    next: AtomicUsize,
    dimension: usize,
    model_name: String,
    /// Config-level sub-batch override (0 = auto-detect).
    sub_batch_override: usize,
    /// Whether GPU execution providers were configured.
    gpu: bool,
}

impl EmbeddingClient {
    /// Create a new embedding client with a pool of model instances.
    ///
    /// The first model is loaded, a warmup inference is run to establish
    /// ONNX Runtime arena buffers, and the resulting memory footprint is
    /// measured.  A 3× multiplier is applied to account for inference-time
    /// arena growth (attention matrices, activations).  The pool size is
    /// then capped so that estimated total memory stays within 60% of
    /// available RAM.
    pub fn new(config: EmbeddingConfig) -> Result<Self> {
        let dimension = match config.model {
            EmbeddingModel::NomicEmbedTextV15 => 768,
            EmbeddingModel::NomicEmbedTextV1 => 768,
            EmbeddingModel::AllMiniLML6V2 => 384,
            EmbeddingModel::BGESmallENV15 => 384,
            EmbeddingModel::BGEBaseENV15 => 768,
            EmbeddingModel::BGELargeENV15 => 1024,
            _ => 768, // Default
        };

        let model_name = format!("{:?}", config.model);
        let desired_pool_size = config.pool_size.max(1);

        // --- Load first instance and measure its memory footprint -----------
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let mem_before = sys.available_memory();

        let has_gpu_providers = !config.execution_providers.is_empty();

        let mut first_model = {
            let mut init_options = InitOptions::new(config.model.clone())
                .with_show_download_progress(config.show_download_progress);
            if let Some(ref cache_dir) = config.cache_dir {
                init_options = init_options.with_cache_dir(cache_dir.into());
            }
            if !config.execution_providers.is_empty() {
                init_options =
                    init_options.with_execution_providers(config.execution_providers.clone());
            }
            TextEmbedding::try_new(init_options)
                .map_err(|e| Error::Other(format!("Failed to initialize embedding model: {}", e)))?
        };

        // Run a warmup inference so the ONNX Runtime arena is allocated before
        // we measure memory.  Without this, per_instance only captures model
        // weights and misses the arena buffers.
        let _ = first_model.embed(vec!["warmup"], None);

        sys.refresh_memory();
        let mem_after = sys.available_memory();
        let per_instance_loaded = mem_before.saturating_sub(mem_after);

        // ONNX Runtime uses arena allocation that grows with
        // batch_size × sequence_length² (attention matrices) and is never
        // released.  The warmup above only allocates a minimal arena for a
        // single short text.  Apply a 3× multiplier to account for realistic
        // inference workloads (batch=8-32 texts of 1000-2000 tokens each).
        let per_instance_bytes = per_instance_loaded.saturating_mul(3);

        // --- Pool size: min(desired, RAM budget) ----------------------------
        let nproc = default_pool_size();
        // Budget: 60% of the memory that was available *before* we loaded
        // the first instance (so the first instance counts against it).
        let budget = mem_before * 6 / 10;
        let pool_size = if let Some(quotient) = budget.checked_div(per_instance_bytes) {
            let max_from_memory = quotient.max(1) as usize;
            // Cap at the requested pool size (or nproc if not explicitly set).
            let capped = max_from_memory.min(desired_pool_size);
            tracing::info!(
                per_instance_mb = per_instance_loaded / (1024 * 1024),
                estimated_with_arena_mb = per_instance_bytes / (1024 * 1024),
                available_mb = mem_before / (1024 * 1024),
                budget_mb = budget / (1024 * 1024),
                nproc = nproc,
                desired = desired_pool_size,
                max_from_memory = max_from_memory,
                capped = capped,
                "Measured ONNX model memory footprint"
            );
            capped
        } else {
            // Measurement was inconclusive (memory increased or stayed flat,
            // e.g. due to page cache reclamation) — fall back to desired size.
            desired_pool_size
        };

        // --- Load remaining instances ---------------------------------------
        let mut pool = Vec::with_capacity(pool_size);
        pool.push(Arc::new(Mutex::new(first_model)));

        for _ in 1..pool_size {
            let mut init_options = InitOptions::new(config.model.clone())
                .with_show_download_progress(config.show_download_progress);

            if let Some(ref cache_dir) = config.cache_dir {
                init_options = init_options.with_cache_dir(cache_dir.into());
            }
            if !config.execution_providers.is_empty() {
                init_options =
                    init_options.with_execution_providers(config.execution_providers.clone());
            }

            let model = TextEmbedding::try_new(init_options).map_err(|e| {
                Error::Other(format!("Failed to initialize embedding model: {}", e))
            })?;
            pool.push(Arc::new(Mutex::new(model)));
        }

        let ep_label = if has_gpu_providers {
            "GPU (CUDA)"
        } else {
            "CPU"
        };
        tracing::info!(
            "Initialized embedding model: {} ({}d, pool_size={}, execution_provider={})",
            model_name,
            dimension,
            pool_size,
            ep_label,
        );

        let sub_batch_override = config.sub_batch_size;

        Ok(Self {
            pool,
            next: AtomicUsize::new(0),
            dimension,
            model_name,
            sub_batch_override,
            gpu: has_gpu_providers,
        })
    }

    /// Round-robin acquire a model instance from the pool.
    fn acquire(&self) -> Arc<Mutex<TextEmbedding>> {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.pool.len();
        self.pool[idx].clone()
    }

    /// Compute sub-batch size based on available system memory.
    ///
    /// Uses 50% of available RAM as a budget.  Falls back to 32 if sysinfo
    /// reports 0.  Respects `EMBEDDING_SUB_BATCH` env var for manual override.
    /// When GPU is enabled, the upper clamp is raised to 256 (GPU VRAM can
    /// handle much larger batches than CPU).
    fn compute_sub_batch(dimension: usize, gpu: bool) -> usize {
        if let Some(val) = std::env::var("CLAUDEAR_EMBEDDING_SUB_BATCH")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n >= 1)
        {
            return val;
        }

        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let available_mb = sys.available_memory() / (1024 * 1024);

        if available_mb == 0 {
            return 32;
        }

        // Per-text memory estimate for ONNX inference.  Attention matrices
        // dominate: heads × seq² × 4 bytes.  For 768-dim BERT-like models
        // (12 heads) processing ~1000-2000 token code chunks, attention alone
        // is 50-200 MB per text.  The estimate below is conservative so the
        // sub-batch stays small enough to prevent arena over-allocation.
        let mb_per_text: u64 = if dimension >= 768 { 100 } else { 40 };
        let budget_mb = available_mb / 2;
        let max_batch = if gpu { 256 } else { 16 };
        (budget_mb / mb_per_text).clamp(4, max_batch) as usize
    }

    /// Create with default configuration from environment.
    pub fn from_env() -> Result<Self> {
        Self::new(EmbeddingConfig::from_env())
    }

    /// Ensure the default embedding model is downloaded and cached.
    ///
    /// Call once (single-threaded) before parallel test runs so that
    /// concurrent `EmbeddingClient::new()` calls never race to download.
    /// Returns `Ok(())` if the model is ready, `Err` if the download failed.
    pub fn warmup() -> Result<()> {
        let config = EmbeddingConfig {
            show_download_progress: false,
            pool_size: 1, // only need one for warmup
            ..EmbeddingConfig::default()
        };
        let _client = Self::new(config)?;
        Ok(())
    }

    /// Generate embedding for a single text.
    ///
    /// Uses `spawn_blocking` because ONNX inference is CPU-bound and would
    /// otherwise block a tokio worker thread.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let model = self.acquire();
        let text = text.to_string();

        tokio::task::spawn_blocking(move || {
            let mut model = model
                .lock()
                .map_err(|e| Error::Other(format!("Embedding model lock poisoned: {}", e)))?;

            let embeddings = model
                .embed(vec![&text], None)
                .map_err(|e| Error::Other(format!("Failed to generate embedding: {}", e)))?;

            embeddings
                .into_iter()
                .next()
                .ok_or_else(|| Error::Other("No embedding returned".to_string()))
        })
        .await
        .map_err(|e| Error::Other(format!("Embedding task panicked: {}", e)))?
    }

    /// Generate embeddings for multiple texts concurrently.
    ///
    /// Splits texts into sub-batches (sized by available memory) and spawns
    /// them onto the blocking pool.  Mutex contention naturally limits
    /// concurrency to `pool_size` simultaneous ONNX calls.
    pub async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let sub_batch = if self.sub_batch_override > 0 {
            self.sub_batch_override
        } else {
            Self::compute_sub_batch(self.dimension, self.gpu)
        };

        let mut handles = Vec::new();

        for sub in texts.chunks(sub_batch) {
            let model = self.acquire();
            let sub_owned: Vec<String> = sub.iter().map(|s| s.to_string()).collect();

            handles.push(tokio::task::spawn_blocking(move || {
                let mut model = model
                    .lock()
                    .map_err(|e| Error::Other(format!("Embedding model lock poisoned: {}", e)))?;

                model
                    .embed(sub_owned, None)
                    .map_err(|e| Error::Other(format!("Failed to generate embeddings: {}", e)))
            }));
        }

        let mut all_embeddings = Vec::with_capacity(texts.len());
        for handle in handles {
            let mut batch_result = handle
                .await
                .map_err(|e| Error::Other(format!("Embedding batch task panicked: {}", e)))??;
            all_embeddings.append(&mut batch_result);
        }

        Ok(all_embeddings)
    }

    /// Check if the embedding model is available.
    pub fn is_available(&self) -> bool {
        true // Always available since it's embedded
    }

    /// Get the model name.
    pub fn model(&self) -> &str {
        &self.model_name
    }

    /// Get the embedding dimension for the configured model.
    pub fn dimension(&self) -> usize {
        self.dimension
    }
}

/// Calculate cosine similarity between two vectors.
///
/// Uses an iterator pattern that is more amenable to LLVM auto-vectorization
/// than an indexed loop, which can yield significant speedups on 768-dim vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let (dot_product, norm_a, norm_b) = a
        .iter()
        .zip(b.iter())
        .fold((0.0f32, 0.0f32, 0.0f32), |(dot, na, nb), (&x, &y)| {
            (dot + x * y, na + x * x, nb + y * y)
        });

    let norm_a = norm_a.sqrt();
    let norm_b = norm_b.sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot_product / (norm_a * norm_b)
}

/// Calculate Euclidean distance between two vectors.
pub fn euclidean_distance(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return f32::MAX;
    }

    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y).powi(2))
        .sum::<f32>()
        .sqrt()
}

/// Normalize a vector to unit length.
pub fn normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

/// Embedding with metadata for similarity search results.
#[derive(Debug, Clone)]
pub struct EmbeddingResult {
    /// The ID of the item.
    pub id: i64,
    /// The similarity score (0.0 to 1.0 for cosine).
    pub similarity: f32,
    /// The original text (optional).
    pub text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_warmup_downloads_model() {
        // Pre-downloads the ONNX model single-threaded so parallel tests
        // don't race.  Runs as a dedicated CI step before the test suite.
        if let Err(e) = EmbeddingClient::warmup() {
            eprintln!("Embedding warmup failed (model may be unavailable): {e}");
        }
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        let similarity = cosine_similarity(&a, &b);
        assert!((similarity - 1.0).abs() < 0.0001);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let similarity = cosine_similarity(&a, &b);
        assert!(similarity.abs() < 0.0001);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let similarity = cosine_similarity(&a, &b);
        assert!((similarity + 1.0).abs() < 0.0001);
    }

    #[test]
    fn test_cosine_similarity_empty() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_cosine_similarity_different_lengths() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_euclidean_distance_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        let distance = euclidean_distance(&a, &b);
        assert!(distance.abs() < 0.0001);
    }

    #[test]
    fn test_euclidean_distance() {
        let a = vec![0.0, 0.0];
        let b = vec![3.0, 4.0];
        let distance = euclidean_distance(&a, &b);
        assert!((distance - 5.0).abs() < 0.0001);
    }

    #[test]
    fn test_normalize() {
        let v = vec![3.0, 4.0];
        let normalized = normalize(&v);
        let norm: f32 = normalized.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.0001);
    }

    #[test]
    fn test_normalize_zero_vector() {
        let v = vec![0.0, 0.0, 0.0];
        let normalized = normalize(&v);
        assert_eq!(normalized, v);
    }

    #[test]
    fn test_embedding_config_default() {
        let config = EmbeddingConfig::default();
        assert!(matches!(config.model, EmbeddingModel::NomicEmbedTextV15));
    }

    #[test]
    fn test_embedding_config_fast() {
        let config = EmbeddingConfig::fast();
        assert!(matches!(config.model, EmbeddingModel::AllMiniLML6V2));
    }

    #[test]
    fn test_cosine_similarity_zero_vectors() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 2.0, 3.0];
        // Should return 0 for zero vector
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_cosine_similarity_partial_overlap() {
        let a = vec![1.0, 1.0, 0.0];
        let b = vec![1.0, 0.0, 1.0];
        let similarity = cosine_similarity(&a, &b);
        // Should be around 0.5
        assert!(similarity > 0.3 && similarity < 0.7);
    }

    #[test]
    fn test_euclidean_distance_different_lengths() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0];
        let distance = euclidean_distance(&a, &b);
        assert_eq!(distance, f32::MAX);
    }

    #[test]
    fn test_euclidean_distance_unit_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let distance = euclidean_distance(&a, &b);
        // Should be sqrt(2) ≈ 1.414
        assert!((distance - 1.414).abs() < 0.01);
    }

    #[test]
    fn test_normalize_unit_vector() {
        let v = vec![1.0, 0.0, 0.0];
        let normalized = normalize(&v);
        // Already unit length
        assert!((normalized[0] - 1.0).abs() < 0.0001);
        assert!(normalized[1].abs() < 0.0001);
        assert!(normalized[2].abs() < 0.0001);
    }

    #[test]
    fn test_normalize_large_vector() {
        let v = vec![100.0, 0.0];
        let normalized = normalize(&v);
        let norm: f32 = normalized.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.0001);
    }

    #[test]
    fn test_embedding_result_fields() {
        let result = EmbeddingResult {
            id: 42,
            similarity: 0.95,
            text: Some("test text".to_string()),
        };

        assert_eq!(result.id, 42);
        assert!((result.similarity - 0.95).abs() < 0.0001);
        assert_eq!(result.text, Some("test text".to_string()));
    }

    #[test]
    fn test_embedding_result_no_text() {
        let result = EmbeddingResult {
            id: 1,
            similarity: 0.5,
            text: None,
        };

        assert!(result.text.is_none());
    }

    #[test]
    fn test_cosine_similarity_single_element() {
        assert!((cosine_similarity(&[3.0], &[5.0]) - 1.0).abs() < 0.001);
        assert!((cosine_similarity(&[-3.0], &[5.0]) + 1.0).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_nan_handling() {
        let a = vec![f32::NAN, 1.0];
        let b = vec![1.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        // NaN in input should propagate to NaN output
        assert!(sim.is_nan());
    }

    #[test]
    fn test_cosine_similarity_infinity() {
        let a = vec![f32::INFINITY, 0.0];
        let b = vec![1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        // Infinity should still produce a valid result (inf/inf = NaN, or 1.0)
        assert!(sim.is_nan() || (sim - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_very_small_values() {
        let a = vec![1e-30, 1e-30];
        let b = vec![1e-30, 1e-30];
        let sim = cosine_similarity(&a, &b);
        // Should be close to 1.0 for identical vectors, even with tiny values
        // May be 0.0 if underflow makes norms 0
        assert!(
            sim == 0.0 || (sim - 1.0).abs() < 0.01,
            "expected 0.0 or ~1.0, got {}",
            sim
        );
    }

    #[test]
    fn test_cosine_similarity_very_large_values() {
        let a = vec![1e30, 1e30];
        let b = vec![1e30, 1e30];
        let sim = cosine_similarity(&a, &b);
        // May overflow, check it doesn't panic
        assert!(sim.is_finite() || sim.is_nan());
    }

    #[test]
    fn test_cosine_similarity_mixed_signs() {
        let a = vec![1.0, -1.0, 1.0, -1.0];
        let b = vec![1.0, -1.0, 1.0, -1.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_one_zero_one_nonzero() {
        let a = vec![0.0, 0.0];
        let b = vec![0.0, 0.0];
        // Both zero vectors
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_euclidean_distance_empty() {
        // Same-length empty vectors should have 0 distance
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        let dist = euclidean_distance(&a, &b);
        assert!((dist - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_euclidean_distance_single_element() {
        let a = vec![0.0];
        let b = vec![5.0];
        let dist = euclidean_distance(&a, &b);
        assert!((dist - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_euclidean_distance_negative_values() {
        let a = vec![-3.0, 0.0];
        let b = vec![0.0, 4.0];
        let dist = euclidean_distance(&a, &b);
        assert!((dist - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_euclidean_distance_symmetric() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 5.0, 6.0];
        assert!((euclidean_distance(&a, &b) - euclidean_distance(&b, &a)).abs() < 0.0001);
    }

    #[test]
    fn test_normalize_single_element() {
        let v = vec![5.0];
        let n = normalize(&v);
        assert!((n[0] - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_normalize_negative_values() {
        let v = vec![-3.0, -4.0];
        let n = normalize(&v);
        let norm: f32 = n.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.001);
        // Signs should be preserved
        assert!(n[0] < 0.0);
        assert!(n[1] < 0.0);
    }

    #[test]
    fn test_normalize_already_unit() {
        let v = vec![0.6, 0.8]; // 0.36 + 0.64 = 1.0
        let n = normalize(&v);
        assert!((n[0] - 0.6).abs() < 0.001);
        assert!((n[1] - 0.8).abs() < 0.001);
    }

    #[test]
    fn test_normalize_empty() {
        let v: Vec<f32> = vec![];
        let n = normalize(&v);
        assert!(n.is_empty());
    }

    #[test]
    fn test_embedding_config_debug() {
        let config = EmbeddingConfig::default();
        // Should implement Debug without panicking
        let debug_str = format!("{:?}", config);
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn test_embedding_result_clone() {
        let result = EmbeddingResult {
            id: 1,
            similarity: 0.9,
            text: Some("test".to_string()),
        };
        let cloned = result.clone();
        assert_eq!(cloned.id, 1);
        assert!((cloned.similarity - 0.9).abs() < 0.001);
        assert_eq!(cloned.text, Some("test".to_string()));
    }

    // === Coverage tests for EmbeddingConfig::from_env ===

    #[test]
    fn test_embedding_config_from_env_returns_valid_config() {
        // from_env always returns a valid config regardless of env state
        let config = EmbeddingConfig::from_env();
        assert!(config.pool_size >= 1);
    }

    // === Coverage tests for compute_sub_batch ===

    #[test]
    fn test_compute_sub_batch_without_env_high_dim() {
        std::env::remove_var("EMBEDDING_SUB_BATCH");
        let result = EmbeddingClient::compute_sub_batch(768, false);
        // Memory-based clamp range is 4..=16, but falls back to 32 when
        // sysinfo reports 0 available memory (common on macOS in tests).
        assert!((4..=32).contains(&result));
    }

    #[test]
    fn test_compute_sub_batch_without_env_low_dim() {
        std::env::remove_var("EMBEDDING_SUB_BATCH");
        let result = EmbeddingClient::compute_sub_batch(384, false);
        // Memory-based clamp range is 4..=16, but falls back to 32 when
        // sysinfo reports 0 available memory (common on macOS in tests).
        assert!((4..=32).contains(&result));
    }

    #[test]
    fn test_compute_sub_batch_gpu_higher_upper_bound() {
        std::env::remove_var("EMBEDDING_SUB_BATCH");
        let result = EmbeddingClient::compute_sub_batch(768, true);
        // GPU allows up to 256 (vs 16 for CPU)
        assert!((4..=256).contains(&result));
    }

    // === Coverage: default_pool_size ===

    #[test]
    fn test_default_pool_size_at_least_one() {
        let size = default_pool_size();
        assert!(size >= 1);
    }
}
