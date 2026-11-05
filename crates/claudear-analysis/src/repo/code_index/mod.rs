//! Tree-sitter powered code indexing pipeline.
//!
//! Parses source files into ASTs, extracts symbols (functions, classes, structs, traits),
//! creates AST-aware chunks at semantic boundaries, embeds those chunks for vector similarity
//! search, and persists everything in SQLite. Incremental re-indexing via file hashing.

pub mod analyzer;
mod chunker;
pub mod complexity;
mod languages;
mod parser;
pub mod types;

pub use types::{CodeChunk, CodeIndexStats, CodeSearchResult, CodeSymbol, Language, SymbolKind};

use crate::feedback::EmbeddingClient;
use claudear_core::error::Result;
use claudear_storage::FixAttemptTracker;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use walkdir::WalkDir;

/// Maximum file size to index (default: 1 MB).
const DEFAULT_MAX_FILE_SIZE: u64 = abnegate_index::LARGEST_FILE;
/// Default embedding batch size.
const DEFAULT_BATCH_SIZE: usize = abnegate_index::EMBED_BATCH;

/// Bump this when the indexing pipeline changes (parser, chunker, analyzer,
/// embedding context format, etc.) to force a full re-index on the next run.
pub const CODE_INDEX_VERSION: &str = "1";

/// Write-side: indexes a repository's source code.
pub struct CodeIndexer {
    tracker: Arc<dyn FixAttemptTracker>,
    embedding_client: Arc<EmbeddingClient>,
    max_file_size: u64,
    batch_size: usize,
    force_reindex: bool,
}

impl CodeIndexer {
    pub fn new(
        tracker: Arc<dyn FixAttemptTracker>,
        embedding_client: Arc<EmbeddingClient>,
    ) -> Self {
        Self {
            tracker,
            embedding_client,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            batch_size: DEFAULT_BATCH_SIZE,
            force_reindex: false,
        }
    }

    pub fn with_config(
        tracker: Arc<dyn FixAttemptTracker>,
        embedding_client: Arc<EmbeddingClient>,
        max_file_size_kb: u64,
        batch_size: usize,
    ) -> Self {
        Self {
            tracker,
            embedding_client,
            max_file_size: max_file_size_kb * 1024,
            batch_size: if batch_size == 0 {
                DEFAULT_BATCH_SIZE
            } else {
                batch_size
            },
            force_reindex: false,
        }
    }

    /// When set, deletes all existing code data for the repo before indexing,
    /// bypassing file-hash based incremental detection.
    pub fn with_force_reindex(mut self, force: bool) -> Self {
        self.force_reindex = force;
        self
    }

    /// Index (or incrementally re-index) a repository.
    pub async fn index_repo(&self, repo_name: &str, repo_path: &Path) -> Result<CodeIndexStats> {
        let repo_id = self.tracker.get_or_create_repo_id(repo_name)?;
        let mut stats = CodeIndexStats::default();

        let mut needs_full_reindex = self.force_reindex;

        // Check if the indexer version changed since last run.
        if !needs_full_reindex {
            if let Ok(Some(stored_ver)) = self.tracker.get_code_index_meta(repo_id, "index_version")
            {
                if stored_ver != CODE_INDEX_VERSION {
                    tracing::info!(
                        repo = %repo_name,
                        old_version = %stored_ver,
                        new_version = %CODE_INDEX_VERSION,
                        "Code index version changed — forcing full re-index"
                    );
                    needs_full_reindex = true;
                }
            }
        }

        // Check if the embedding model changed.
        if !needs_full_reindex {
            if let Ok(Some(stored_model)) = self.tracker.get_code_embedding_model(repo_id) {
                let current_model = self.embedding_client.model();
                if stored_model != current_model {
                    tracing::warn!(
                        repo = %repo_name,
                        old_model = %stored_model,
                        new_model = %current_model,
                        "Embedding model changed — forcing full re-index"
                    );
                    needs_full_reindex = true;
                }
            }
        }

        if needs_full_reindex {
            tracing::info!(repo = %repo_name, "Deleting all code data for full re-index");
            self.tracker.delete_all_code_data_for_repo(repo_id)?;
        }

        // Collect all source files.
        let files = self.collect_source_files(repo_path);
        tracing::info!(
            repo = %repo_name,
            source_files = files.len(),
            "Starting code indexing"
        );

        let parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let repo_path_owned = repo_path.to_path_buf();
        let mut seen_paths: Vec<String> = Vec::with_capacity(files.len());
        let mut pending_chunks: Vec<types::CodeChunk> = Vec::new();
        let mut pending_symbols: Vec<types::CodeSymbol> = Vec::new();

        for file_group in files.chunks(parallelism) {
            let tracker = Arc::clone(&self.tracker);
            let rpo = &repo_path_owned;
            let file_results: Vec<FileProcessResult> = file_group
                .par_iter()
                .map(|(path, language)| {
                    let rel_path = abnegate_index::relative_to(rpo, path);

                    // Read file content.
                    let content = match std::fs::read_to_string(path) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::debug!(file = %rel_path, error = %e, "Failed to read file");
                            return FileProcessResult::Failed(());
                        }
                    };

                    // Compute hash for incremental detection.
                    let file_hash = sha256_hex(&content);

                    // Check if file is already indexed with this hash.
                    match tracker.code_chunk_hash_matches(repo_id, &rel_path, &file_hash) {
                        Ok(true) => return FileProcessResult::Skipped(rel_path),
                        Err(e) => {
                            tracing::debug!(file = %rel_path, error = %e, "Hash check failed");
                            return FileProcessResult::Failed(());
                        }
                        Ok(false) => {}
                    }

                    // Delete old data for this file before re-indexing.
                    if let Err(e) = tracker.delete_code_data_for_file(repo_id, &rel_path) {
                        tracing::debug!(file = %rel_path, error = %e, "Failed to delete old data");
                        return FileProcessResult::Failed(());
                    }

                    // Parse and chunk.
                    match chunker::chunk_file(&content, *language, repo_id, &rel_path, &file_hash) {
                        Ok((symbols, chunks)) => FileProcessResult::Parsed {
                            rel_path,
                            symbols,
                            chunks,
                        },
                        Err(e) => {
                            tracing::debug!(file = %rel_path, error = %e, "Failed to parse file");
                            FileProcessResult::Failed(())
                        }
                    }
                })
                .collect();

            for result in file_results {
                match result {
                    FileProcessResult::Parsed {
                        rel_path,
                        symbols,
                        chunks,
                    } => {
                        stats.files_processed += 1;
                        stats.symbols_extracted += symbols.len();
                        stats.chunks_created += chunks.len();
                        pending_symbols.extend(symbols);
                        pending_chunks.extend(chunks);
                        seen_paths.push(rel_path);
                    }
                    FileProcessResult::Skipped(rel_path) => {
                        stats.files_skipped += 1;
                        seen_paths.push(rel_path);
                    }
                    FileProcessResult::Failed(_) => {
                        stats.files_failed += 1;
                    }
                }

                // Flush batch if large enough.
                if pending_chunks.len() >= self.batch_size {
                    self.flush_batch(&mut pending_symbols, &mut pending_chunks, &mut stats)
                        .await?;
                }
            }
        }

        // Flush remaining.
        if !pending_chunks.is_empty() || !pending_symbols.is_empty() {
            self.flush_batch(&mut pending_symbols, &mut pending_chunks, &mut stats)
                .await?;
        }

        // Clean up entries for deleted files.
        self.tracker.cleanup_stale_code_data(repo_id, &seen_paths)?;

        // Stamp the current index version so future runs can detect changes.
        let _ = self
            .tracker
            .set_code_index_meta(repo_id, "index_version", CODE_INDEX_VERSION);

        tracing::info!(repo = %repo_name, %stats, "Code indexing complete");
        Ok(stats)
    }

    /// Flush a batch of symbols and chunks: store symbols, store chunks, embed, store embeddings.
    ///
    /// Content-hash deduplication (Issue #11): chunks with identical `chunk_text`
    /// share a single embedding computation. Each unique text is embedded once
    /// and the resulting vector is reused for all chunks with that content hash.
    async fn flush_batch(
        &self,
        symbols: &mut Vec<types::CodeSymbol>,
        chunks: &mut Vec<types::CodeChunk>,
        stats: &mut CodeIndexStats,
    ) -> Result<()> {
        // Store symbols.
        if !symbols.is_empty() {
            self.tracker.save_code_symbols(symbols)?;
            symbols.clear();
        }

        if chunks.is_empty() {
            return Ok(());
        }

        // Compute content hashes for deduplication.
        for chunk in chunks.iter_mut() {
            chunk.content_hash = Some(sha256_hex(&chunk.chunk_text));
        }

        // Store chunks and get their IDs.
        let chunk_ids = self.tracker.save_code_chunks(chunks)?;

        // --- Content-hash deduplication (Issue #11) ---
        // Group chunks by content_hash: embed each unique text only once.
        // Build full context (prefix + code) for embedding (Issue #6).
        let mut unique_texts: Vec<String> = Vec::new();
        let mut hash_to_embed_idx: HashMap<String, usize> = HashMap::new();
        let mut chunk_to_embed_idx: Vec<usize> = Vec::with_capacity(chunks.len());

        for chunk in chunks.iter() {
            let content_hash = chunk.content_hash.as_deref().unwrap_or("");
            if let Some(&idx) = hash_to_embed_idx.get(content_hash) {
                chunk_to_embed_idx.push(idx);
            } else {
                let idx = unique_texts.len();
                // Build full embedding text from the stored context prefix
                // (includes file path, language, symbol, signature, imports)
                // combined with the code body.
                let full_context =
                    chunker::build_embedding_text(&chunk.context_text, &chunk.chunk_text);
                unique_texts.push(full_context);
                hash_to_embed_idx.insert(content_hash.to_string(), idx);
                chunk_to_embed_idx.push(idx);
            }
        }

        // Embed all unique texts in a single batch (Issue #3: no double-batching).
        let unique_refs: Vec<&str> = unique_texts.iter().map(|s| s.as_str()).collect();
        match self.embedding_client.embed_batch(&unique_refs).await {
            Ok(unique_embeddings) => {
                // Map each chunk to its embedding via the dedup index.
                let pairs: Vec<(i64, &[f32])> = chunk_ids
                    .iter()
                    .zip(chunk_to_embed_idx.iter())
                    .filter_map(|(&id, &embed_idx)| {
                        unique_embeddings
                            .get(embed_idx)
                            .map(|emb| (id, emb.as_slice()))
                    })
                    .collect();

                let model_name = self.embedding_client.model();
                if let Err(error) = self.tracker.save_code_chunk_embeddings(&pairs, model_name) {
                    self.discard_chunks(&chunk_ids);
                    chunks.clear();
                    return Err(error);
                }
                stats.embeddings_generated += pairs.len();
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to generate code chunk embeddings");
                self.discard_chunks(&chunk_ids);
            }
        }

        chunks.clear();
        Ok(())
    }

    fn discard_chunks(&self, chunk_ids: &[i64]) {
        if let Err(delete_error) = self.tracker.delete_code_chunks_by_ids(chunk_ids) {
            tracing::warn!(%delete_error, "Failed to delete code chunks without embeddings");
        }
    }

    /// Walk the repo and collect source files with their detected language.
    fn collect_source_files(&self, repo_path: &Path) -> Vec<(std::path::PathBuf, Language)> {
        let mut files = Vec::new();

        for entry in WalkDir::new(repo_path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                // Always include the root directory (depth 0) so that
                // repositories whose directory name starts with '.' or
                // matches a skip-name are still walked.
                if e.depth() == 0 {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                !name.starts_with('.')
                    && name != "node_modules"
                    && name != "vendor"
                    && name != "target"
                    && name != "build"
                    && name != "dist"
                    && name != "__pycache__"
            })
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }

            // Check file size.
            if let Ok(meta) = entry.metadata() {
                if meta.len() > self.max_file_size {
                    continue;
                }
            }

            // Check language.
            let path = entry.path();
            let lang = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(Language::from_extension)
                .or_else(|| {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .and_then(Language::from_filename)
                });
            if let Some(lang) = lang {
                files.push((path.to_path_buf(), lang));
            }
        }

        files
    }
}

/// Result from processing a single file in the parallel phase.
enum FileProcessResult {
    Parsed {
        rel_path: String,
        symbols: Vec<types::CodeSymbol>,
        chunks: Vec<types::CodeChunk>,
    },
    Skipped(String),
    Failed(()),
}

/// Read-side: search indexed code.
pub struct CodeSearchService {
    tracker: Arc<dyn FixAttemptTracker>,
    embedding_client: Arc<EmbeddingClient>,
}

impl CodeSearchService {
    pub fn new(
        tracker: Arc<dyn FixAttemptTracker>,
        embedding_client: Arc<EmbeddingClient>,
    ) -> Self {
        Self {
            tracker,
            embedding_client,
        }
    }

    /// Semantic vector search for code chunks matching a query.
    pub async fn search(
        &self,
        query: &str,
        repo_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<CodeSearchResult>> {
        let embedding = self.embedding_client.embed(query).await?;
        self.tracker.search_code_chunks(&embedding, repo_id, limit)
    }

    /// Find symbols by name (exact substring match).
    pub fn find_symbol(
        &self,
        name: &str,
        kind: Option<SymbolKind>,
        repo_id: Option<i64>,
    ) -> Result<Vec<CodeSymbol>> {
        self.tracker.find_code_symbols(name, kind, repo_id)
    }

    /// Find code chunks similar to a given code snippet.
    pub async fn find_similar_to_code(
        &self,
        snippet: &str,
        repo_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<CodeSearchResult>> {
        let embedding = self.embedding_client.embed(snippet).await?;
        self.tracker.search_code_chunks(&embedding, repo_id, limit)
    }
}

/// Build a search query for code search, tailored to the issue source.
///
/// For Sentry issues, includes error type, culprit, filename, and function metadata
/// which are highly relevant for matching stack traces to code. For other sources,
/// uses title + description.
pub fn build_code_search_query(issue: &claudear_core::types::Issue) -> String {
    let mut parts = vec![issue.title.clone()];

    match issue.source.as_str() {
        "sentry" => {
            // Sentry issues carry rich error metadata that maps well to code
            if let Some(ref desc) = issue.description {
                parts.push(desc.clone());
            }
            if let Some(v) = issue.get_metadata::<String>("error_type") {
                parts.push(v);
            }
            if let Some(v) = issue.get_metadata::<String>("error_value") {
                parts.push(v);
            }
            if let Some(v) = issue.get_metadata::<String>("culprit") {
                if !v.is_empty() {
                    parts.push(v);
                }
            }
            if let Some(v) = issue.get_metadata::<String>("filename") {
                parts.push(v);
            }
            if let Some(v) = issue.get_metadata::<String>("function") {
                parts.push(v);
            }
        }
        _ => {
            if let Some(ref desc) = issue.description {
                parts.push(desc.clone());
            }
        }
    }

    parts.join(" ")
}

/// Format code search results as a markdown context section for inclusion in prompts.
pub fn format_code_search_context(results: &[CodeSearchResult]) -> String {
    use std::fmt::Write;

    if results.is_empty() {
        return String::new();
    }

    let mut context = String::from("\n\n## Relevant Code from Repository\n\n");
    context.push_str(
        "The following code snippets were found to be semantically relevant to this issue. ",
    );
    context.push_str("Use them to understand the codebase and inform your approach:\n\n");

    for (i, result) in results.iter().enumerate() {
        let chunk = &result.chunk;
        let _ = writeln!(
            context,
            "### {}. `{}` (Similarity: {:.0}%)",
            i + 1,
            chunk.file_path,
            result.score * 100.0,
        );

        if let Some(ref symbol) = chunk.symbol_name {
            let _ = writeln!(context, "**Symbol:** {} ({})", symbol, chunk.chunk_type);
        }

        let _ = writeln!(
            context,
            "**Lines:** {}-{} | **Language:** {}",
            chunk.start_line,
            chunk.end_line,
            chunk.language.as_str(),
        );

        // Truncate very long snippets to keep context manageable
        let code = if chunk.chunk_text.len() > 2000 {
            format!("{}...\n(truncated)", &chunk.chunk_text[..2000])
        } else {
            chunk.chunk_text.clone()
        };

        let _ = writeln!(
            context,
            "```{}\n{}\n```\n",
            chunk.language.as_str().to_lowercase(),
            code
        );
    }

    context
}

fn sha256_hex(content: &str) -> String {
    abnegate_index::file_hash(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Try to create an EmbeddingClient; returns `None` when the ONNX model
    /// cannot be downloaded (common in CI).  Tests that need an
    /// `Arc<EmbeddingClient>` should early-return when this returns `None`.
    fn try_embedding_client() -> Option<Arc<crate::feedback::EmbeddingClient>> {
        crate::feedback::EmbeddingClient::new(crate::feedback::EmbeddingConfig {
            pool_size: 1, // single instance is sufficient for tests
            ..Default::default()
        })
        .ok()
        .map(Arc::new)
    }

    #[test]
    fn test_sha256_hex() {
        let hash = sha256_hex("hello world");
        assert_eq!(hash.len(), 64); // SHA256 hex is 64 chars
                                    // Deterministic
        assert_eq!(hash, sha256_hex("hello world"));
        // Different input → different hash
        assert_ne!(hash, sha256_hex("hello world!"));
    }

    #[test]
    fn test_sha256_hex_empty_string() {
        let hash = sha256_hex("");
        assert_eq!(hash.len(), 64);
        // SHA256 of empty string is a well-known constant
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_sha256_hex_known_value() {
        // SHA256("abc") is a well-known test vector
        let hash = sha256_hex("abc");
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn test_sha256_hex_deterministic() {
        let input = "fn main() { println!(\"Hello, world!\"); }";
        let hash1 = sha256_hex(input);
        let hash2 = sha256_hex(input);
        let hash3 = sha256_hex(input);
        assert_eq!(hash1, hash2);
        assert_eq!(hash2, hash3);
    }

    #[test]
    fn test_sha256_hex_different_inputs_different_hashes() {
        let inputs = ["a", "b", "c", "ab", "abc", "A", "B"];
        let hashes: Vec<String> = inputs.iter().map(|i| sha256_hex(i)).collect();
        for i in 0..hashes.len() {
            for j in (i + 1)..hashes.len() {
                assert_ne!(
                    hashes[i], hashes[j],
                    "Inputs '{}' and '{}' should produce different hashes",
                    inputs[i], inputs[j]
                );
            }
        }
    }

    #[test]
    fn test_sha256_hex_whitespace_sensitivity() {
        let hash1 = sha256_hex("hello world");
        let hash2 = sha256_hex("hello  world");
        let hash3 = sha256_hex("hello\tworld");
        let hash4 = sha256_hex("hello\nworld");

        assert_ne!(hash1, hash2, "Double space should differ from single");
        assert_ne!(hash1, hash3, "Tab should differ from space");
        assert_ne!(hash1, hash4, "Newline should differ from space");
    }

    #[test]
    fn test_sha256_hex_case_sensitivity() {
        let hash_lower = sha256_hex("hello");
        let hash_upper = sha256_hex("HELLO");
        assert_ne!(hash_lower, hash_upper, "SHA256 is case-sensitive");
    }

    #[test]
    fn test_sha256_hex_unicode() {
        let hash = sha256_hex("\u{1F600}\u{1F601}\u{1F602}");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_sha256_hex_long_content() {
        let content = "x".repeat(1_000_000);
        let hash = sha256_hex(&content);
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_sha256_hex_only_lowercase_hex() {
        // SHA256 hex should use lowercase hex characters
        let hash = sha256_hex("test content");
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "Hash should be lowercase hex only, got: {}",
            hash
        );
    }

    #[test]
    fn test_sha256_hex_newline_at_end_differs() {
        let hash1 = sha256_hex("content");
        let hash2 = sha256_hex("content\n");
        assert_ne!(hash1, hash2, "Trailing newline changes the hash");
    }

    #[test]
    fn test_sha256_hex_null_byte_in_content() {
        let hash1 = sha256_hex("hello\0world");
        let hash2 = sha256_hex("helloworld");
        assert_ne!(hash1, hash2, "Null byte should be included in hash");
    }

    #[test]
    fn test_default_max_file_size_is_1mb() {
        assert_eq!(DEFAULT_MAX_FILE_SIZE, 1024 * 1024);
        assert_eq!(DEFAULT_MAX_FILE_SIZE, 1_048_576);
    }

    #[test]
    fn test_default_batch_size_is_32() {
        assert_eq!(DEFAULT_BATCH_SIZE, 32);
    }

    #[test]
    fn test_collect_source_files_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Need a CodeIndexer to call collect_source_files.
        // We create a minimal one with in-memory tracker and a
        // mock embedding client. Since collect_source_files is sync
        // and doesn't use embedding_client, we just need the struct.
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert!(files.is_empty(), "Empty dir should yield no source files");
    }

    #[test]
    fn test_collect_source_files_finds_rust_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn hello() {}").unwrap();
        std::fs::write(dir.path().join("README.md"), "# Readme").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 2, "Should find 2 .rs files");
        assert!(files.iter().all(|(_, lang)| *lang == Language::Rust));
    }

    #[test]
    fn test_collect_source_files_skips_hidden_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("secret.rs"), "fn secret() {}").unwrap();
        std::fs::write(dir.path().join("visible.rs"), "fn visible() {}").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 1);
        let names: Vec<_> = files
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"visible.rs".to_string()));
        assert!(!names.contains(&"secret.rs".to_string()));
    }

    #[test]
    fn test_collect_source_files_skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let nm = dir.path().join("node_modules");
        std::fs::create_dir(&nm).unwrap();
        std::fs::write(nm.join("index.js"), "module.exports = {}").unwrap();
        std::fs::write(dir.path().join("app.js"), "console.log('hi')").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 1);
        assert!(files[0].0.file_name().unwrap().to_string_lossy() == "app.js");
    }

    #[test]
    fn test_collect_source_files_skips_target_dir() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("compiled.rs"), "fn compiled() {}").unwrap();
        std::fs::write(dir.path().join("src.rs"), "fn src() {}").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 1);
        assert!(files[0].0.file_name().unwrap().to_string_lossy() == "src.rs");
    }

    #[test]
    fn test_collect_source_files_skips_vendor_and_build_and_dist() {
        let dir = tempfile::tempdir().unwrap();
        for dirname in &["vendor", "build", "dist"] {
            let d = dir.path().join(dirname);
            std::fs::create_dir(&d).unwrap();
            std::fs::write(d.join("lib.rs"), "fn lib() {}").unwrap();
        }
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(
            files.len(),
            1,
            "Should only find main.rs, not files in vendor/build/dist"
        );
    }

    #[test]
    fn test_collect_source_files_skips_pycache() {
        let dir = tempfile::tempdir().unwrap();
        let pycache = dir.path().join("__pycache__");
        std::fs::create_dir(&pycache).unwrap();
        std::fs::write(pycache.join("module.py"), "print('cached')").unwrap();
        std::fs::write(dir.path().join("app.py"), "print('app')").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, Language::Python);
    }

    #[test]
    fn test_collect_source_files_multiple_languages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("app.ts"), "const x = 1;").unwrap();
        std::fs::write(dir.path().join("lib.py"), "def f(): pass").unwrap();
        std::fs::write(dir.path().join("main.go"), "package main").unwrap();
        std::fs::write(dir.path().join("App.java"), "class App {}").unwrap();
        std::fs::write(dir.path().join("unknown.xyz"), "???").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(
            files.len(),
            5,
            "Should find 5 supported language files (not .xyz)"
        );

        let languages: std::collections::HashSet<Language> =
            files.iter().map(|(_, l)| *l).collect();
        assert!(languages.contains(&Language::Rust));
        assert!(languages.contains(&Language::TypeScript));
        assert!(languages.contains(&Language::Python));
        assert!(languages.contains(&Language::Go));
        assert!(languages.contains(&Language::Java));
    }

    #[test]
    fn test_collect_source_files_skips_large_files() {
        let dir = tempfile::tempdir().unwrap();
        // Create a file larger than DEFAULT_MAX_FILE_SIZE (1MB)
        let large_content = "x".repeat(DEFAULT_MAX_FILE_SIZE as usize + 1);
        std::fs::write(dir.path().join("large.rs"), &large_content).unwrap();
        std::fs::write(dir.path().join("small.rs"), "fn small() {}").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 1, "Should skip the large file");
        assert!(files[0].0.file_name().unwrap().to_string_lossy() == "small.rs");
    }

    #[test]
    fn test_collect_source_files_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src").join("models");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("user.rs"), "struct User {}").unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 2, "Should find files in nested directories");
    }

    #[test]
    fn test_code_indexer_new_defaults() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        assert_eq!(indexer.max_file_size, DEFAULT_MAX_FILE_SIZE);
        assert_eq!(indexer.batch_size, DEFAULT_BATCH_SIZE);
    }

    #[test]
    fn test_code_indexer_with_config_custom_values() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::with_config(tracker, embedding_client, 512, 64);

        // max_file_size_kb is multiplied by 1024
        assert_eq!(indexer.max_file_size, 512 * 1024);
        assert_eq!(indexer.batch_size, 64);
    }

    #[test]
    fn test_code_indexer_with_config_zero_batch_size_uses_default() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::with_config(tracker, embedding_client, 256, 0);

        assert_eq!(
            indexer.batch_size, DEFAULT_BATCH_SIZE,
            "batch_size=0 should fall back to DEFAULT_BATCH_SIZE"
        );
    }

    #[test]
    fn test_code_indexer_with_config_small_file_size() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::with_config(tracker, embedding_client, 1, 1);

        assert_eq!(indexer.max_file_size, 1024); // 1 KB
        assert_eq!(indexer.batch_size, 1);
    }

    #[test]
    fn test_code_search_service_new() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let _service = CodeSearchService::new(tracker, embedding_client);
        // Constructor should not panic
    }

    #[test]
    fn test_collect_source_files_custom_max_size() {
        let dir = tempfile::tempdir().unwrap();
        // Write a 2KB file
        let content_2kb = "x".repeat(2048);
        std::fs::write(dir.path().join("big.rs"), &content_2kb).unwrap();
        // Write a 500-byte file
        let content_small = "y".repeat(500);
        std::fs::write(dir.path().join("small.rs"), &content_small).unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        // Set max_file_size to 1KB
        let indexer = CodeIndexer::with_config(tracker, embedding_client, 1, 32);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 1, "Only files under 1KB should be collected");
        assert!(files[0].0.file_name().unwrap().to_string_lossy() == "small.rs");
    }

    #[test]
    fn test_collect_source_files_all_supported_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let extensions_and_languages = vec![
            ("test.rs", Language::Rust),
            ("test.ts", Language::TypeScript),
            ("test.tsx", Language::Tsx),
            ("test.js", Language::JavaScript),
            ("test.jsx", Language::JavaScript),
            ("test.mjs", Language::JavaScript),
            ("test.cjs", Language::JavaScript),
            ("test.py", Language::Python),
            ("test.pyi", Language::Python),
            ("test.go", Language::Go),
            ("test.java", Language::Java),
            ("test.c", Language::C),
            ("test.h", Language::C),
            ("test.cpp", Language::Cpp),
            ("test.cc", Language::Cpp),
            ("test.rb", Language::Ruby),
            ("test.php", Language::Php),
            ("test.swift", Language::Swift),
            ("test.kt", Language::Kotlin),
            ("test.kts", Language::Kotlin),
            ("test.yaml", Language::Yaml),
            ("test.yml", Language::Yaml),
            ("test.json", Language::Json),
            ("test.lua", Language::Lua),
            ("Dockerfile", Language::Dockerfile),
        ];

        for (filename, _) in &extensions_and_languages {
            std::fs::write(dir.path().join(filename), "// content").unwrap();
        }
        // Also write unsupported extensions
        std::fs::write(dir.path().join("test.txt"), "text").unwrap();
        std::fs::write(dir.path().join("test.md"), "# markdown").unwrap();
        std::fs::write(dir.path().join("test.xml"), "<root/>").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(
            files.len(),
            extensions_and_languages.len(),
            "Should find all supported extensions and skip unsupported ones"
        );

        // Verify each expected language is present
        for (filename, expected_lang) in &extensions_and_languages {
            let found = files.iter().any(|(path, lang)| {
                path.file_name().unwrap().to_string_lossy() == *filename && *lang == *expected_lang
            });
            assert!(
                found,
                "Expected to find {} with language {:?}",
                filename, expected_lang
            );
        }
    }

    fn make_chunk(
        file_path: &str,
        symbol_name: Option<&str>,
        chunk_type: &str,
        language: Language,
        start_line: usize,
        end_line: usize,
        chunk_text: &str,
    ) -> CodeChunk {
        CodeChunk {
            id: None,
            repo_id: 1,
            file_path: file_path.to_string(),
            chunk_type: chunk_type.to_string(),
            symbol_name: symbol_name.map(|s| s.to_string()),
            language,
            start_line,
            end_line,
            chunk_text: chunk_text.to_string(),
            context_text: String::new(),
            file_hash: String::new(),
            content_hash: None,
        }
    }

    fn make_search_result(chunk: CodeChunk, score: f64) -> CodeSearchResult {
        CodeSearchResult { chunk, score }
    }

    #[test]
    fn test_format_code_search_context_empty() {
        let result = super::format_code_search_context(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_format_code_search_context_single_result() {
        let chunk = make_chunk(
            "src/main.rs",
            Some("main"),
            "function",
            Language::Rust,
            1,
            10,
            "fn main() {\n    println!(\"hello\");\n}",
        );
        let results = vec![make_search_result(chunk, 0.95)];

        let ctx = super::format_code_search_context(&results);

        assert!(ctx.contains("## Relevant Code from Repository"));
        assert!(ctx.contains("### 1. `src/main.rs` (Similarity: 95%)"));
        assert!(ctx.contains("**Symbol:** main (function)"));
        assert!(ctx.contains("**Lines:** 1-10 | **Language:** Rust"));
        assert!(ctx.contains("```rust"));
        assert!(ctx.contains("fn main()"));
    }

    #[test]
    fn test_format_code_search_context_multiple_results() {
        let chunk1 = make_chunk(
            "src/lib.rs",
            Some("process"),
            "function",
            Language::Rust,
            20,
            40,
            "fn process() {}",
        );
        let chunk2 = make_chunk(
            "src/app.ts",
            Some("App"),
            "class",
            Language::TypeScript,
            1,
            50,
            "class App {}",
        );
        let results = vec![
            make_search_result(chunk1, 0.88),
            make_search_result(chunk2, 0.72),
        ];

        let ctx = super::format_code_search_context(&results);

        assert!(ctx.contains("### 1. `src/lib.rs` (Similarity: 88%)"));
        assert!(ctx.contains("### 2. `src/app.ts` (Similarity: 72%)"));
        assert!(ctx.contains("**Symbol:** process (function)"));
        assert!(ctx.contains("**Symbol:** App (class)"));
        assert!(ctx.contains("```rust"));
        assert!(ctx.contains("```typescript"));
    }

    #[test]
    fn test_format_code_search_context_no_symbol() {
        let chunk = make_chunk(
            "src/utils.py",
            None,
            "module",
            Language::Python,
            1,
            5,
            "import os",
        );
        let results = vec![make_search_result(chunk, 0.60)];

        let ctx = super::format_code_search_context(&results);

        assert!(ctx.contains("### 1. `src/utils.py` (Similarity: 60%)"));
        // No symbol line should be present
        assert!(!ctx.contains("**Symbol:**"));
        assert!(ctx.contains("**Lines:** 1-5 | **Language:** Python"));
        assert!(ctx.contains("```python"));
    }

    #[test]
    fn test_format_code_search_context_truncates_long_code() {
        let long_code = "x".repeat(3000);
        let chunk = make_chunk(
            "src/big.rs",
            Some("big_fn"),
            "function",
            Language::Rust,
            1,
            100,
            &long_code,
        );
        let results = vec![make_search_result(chunk, 0.50)];

        let ctx = super::format_code_search_context(&results);

        assert!(ctx.contains("(truncated)"));
        // The truncated code should be 2000 chars + "...\n(truncated)"
        assert!(!ctx.contains(&"x".repeat(3000)));
        assert!(ctx.contains(&"x".repeat(2000)));
    }

    #[test]
    fn test_format_code_search_context_similarity_rounding() {
        let chunk = make_chunk(
            "src/test.go",
            None,
            "function",
            Language::Go,
            1,
            1,
            "func test() {}",
        );
        let results = vec![make_search_result(chunk, 0.8567)];

        let ctx = super::format_code_search_context(&results);
        // 0.8567 * 100 = 85.67, rounded to 86%
        assert!(ctx.contains("Similarity: 86%"));
    }

    #[test]
    fn test_format_code_search_context_all_languages_lowercase_fence() {
        for (lang, expected_fence) in [
            (Language::Rust, "```rust"),
            (Language::TypeScript, "```typescript"),
            (Language::JavaScript, "```javascript"),
            (Language::Python, "```python"),
            (Language::Go, "```go"),
            (Language::Java, "```java"),
            (Language::Cpp, "```c++"),
            (Language::C, "```c"),
        ] {
            let chunk = make_chunk("test.x", None, "function", lang, 1, 1, "code");
            let results = vec![make_search_result(chunk, 0.5)];
            let ctx = super::format_code_search_context(&results);
            assert!(
                ctx.contains(expected_fence),
                "Expected fence '{}' for {:?}, got:\n{}",
                expected_fence,
                lang,
                ctx
            );
        }
    }

    #[test]
    fn test_format_code_search_context_contains_header_text() {
        let chunk = make_chunk("a.rs", None, "function", Language::Rust, 1, 1, "x");
        let results = vec![make_search_result(chunk, 0.9)];
        let ctx = super::format_code_search_context(&results);
        assert!(ctx.contains("semantically relevant to this issue"));
        assert!(ctx.contains("Use them to understand the codebase"));
    }

    #[test]
    fn test_build_code_search_query_default_source() {
        let issue = claudear_core::types::Issue::new("1", "T-1", "Bug title", "url", "jira");
        let query = super::build_code_search_query(&issue);
        assert_eq!(query, "Bug title");
    }

    #[test]
    fn test_build_code_search_query_default_source_with_description() {
        let mut issue = claudear_core::types::Issue::new("1", "T-1", "Bug title", "url", "linear");
        issue.description = Some("The login page crashes".to_string());
        let query = super::build_code_search_query(&issue);
        assert_eq!(query, "Bug title The login page crashes");
    }

    #[test]
    fn test_build_code_search_query_sentry_basic() {
        let issue = claudear_core::types::Issue::new("1", "SENTRY-1", "TypeError", "url", "sentry");
        let query = super::build_code_search_query(&issue);
        assert_eq!(query, "TypeError");
    }

    #[test]
    fn test_build_code_search_query_sentry_with_all_metadata() {
        let mut issue =
            claudear_core::types::Issue::new("1", "SENTRY-1", "TypeError", "url", "sentry");
        issue.description = Some("Cannot read property 'x'".to_string());
        issue.set_metadata("error_type", "TypeError");
        issue.set_metadata("error_value", "Cannot read property 'x' of undefined");
        issue.set_metadata("culprit", "app.js:main");
        issue.set_metadata("filename", "src/app.js");
        issue.set_metadata("function", "handleClick");

        let query = super::build_code_search_query(&issue);

        assert!(query.contains("TypeError"));
        assert!(query.contains("Cannot read property 'x'"));
        assert!(query.contains("Cannot read property 'x' of undefined"));
        assert!(query.contains("app.js:main"));
        assert!(query.contains("src/app.js"));
        assert!(query.contains("handleClick"));
    }

    #[test]
    fn test_build_code_search_query_sentry_partial_metadata() {
        let mut issue = claudear_core::types::Issue::new(
            "1",
            "SENTRY-1",
            "NullPointerException",
            "url",
            "sentry",
        );
        issue.set_metadata("error_type", "NullPointerException");
        issue.set_metadata("filename", "com/app/Service.java");
        // No error_value, culprit, or function

        let query = super::build_code_search_query(&issue);

        assert!(query.contains("NullPointerException"));
        assert!(query.contains("com/app/Service.java"));
        // Should not have extra spaces from missing fields
        assert!(!query.contains("  "));
    }

    #[test]
    fn test_build_code_search_query_sentry_empty_culprit_excluded() {
        let mut issue = claudear_core::types::Issue::new("1", "SENTRY-1", "Error", "url", "sentry");
        issue.set_metadata("culprit", "");

        let query = super::build_code_search_query(&issue);
        // Empty culprit should not be included
        assert_eq!(query, "Error");
    }

    #[test]
    fn test_build_code_search_query_sentry_with_description_no_metadata() {
        let mut issue = claudear_core::types::Issue::new("1", "SENTRY-1", "Error", "url", "sentry");
        issue.description = Some("Something went wrong".to_string());

        let query = super::build_code_search_query(&issue);
        assert_eq!(query, "Error Something went wrong");
    }

    #[test]
    fn test_build_code_search_query_discord_uses_default_path() {
        let mut issue =
            claudear_core::types::Issue::new("1", "D-1", "Help needed", "url", "discord");
        issue.description = Some("My app crashes on startup".to_string());
        let query = super::build_code_search_query(&issue);
        assert_eq!(query, "Help needed My app crashes on startup");
    }

    #[test]
    fn test_build_code_search_query_slack_uses_default_path() {
        let mut issue = claudear_core::types::Issue::new("1", "S-1", "Alert", "url", "slack");
        issue.description = Some("High CPU usage".to_string());
        let query = super::build_code_search_query(&issue);
        assert_eq!(query, "Alert High CPU usage");
    }

    #[test]
    fn test_build_code_search_query_no_description_non_sentry() {
        let issue = claudear_core::types::Issue::new("1", "T-1", "Title only", "url", "github");
        let query = super::build_code_search_query(&issue);
        assert_eq!(query, "Title only");
    }

    #[test]
    fn test_build_code_search_query_sentry_no_description_no_metadata() {
        let issue = claudear_core::types::Issue::new("1", "S-1", "Error", "url", "sentry");
        let query = super::build_code_search_query(&issue);
        assert_eq!(query, "Error");
    }

    #[test]
    fn test_collect_source_files_finds_yaml_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.yaml"), "key: value").unwrap();
        std::fs::write(dir.path().join("other.yml"), "other: true").unwrap();
        std::fs::write(dir.path().join("readme.txt"), "text").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 2, "Should find 2 YAML files");
        assert!(files.iter().all(|(_, lang)| *lang == Language::Yaml));
    }

    #[test]
    fn test_collect_source_files_finds_json_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        std::fs::write(dir.path().join("readme.txt"), "text").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 2, "Should find 2 JSON files");
        assert!(files.iter().all(|(_, lang)| *lang == Language::Json));
    }

    #[test]
    fn test_collect_source_files_finds_lua_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("init.lua"), "print('hello')").unwrap();
        std::fs::write(dir.path().join("config.lua"), "return {}").unwrap();
        std::fs::write(dir.path().join("readme.txt"), "text").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 2, "Should find 2 Lua files");
        assert!(files.iter().all(|(_, lang)| *lang == Language::Lua));
    }

    #[test]
    fn test_collect_source_files_finds_dockerfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM rust:1.75").unwrap();
        std::fs::write(dir.path().join("readme.txt"), "text").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 1, "Should find 1 Dockerfile");
        assert_eq!(files[0].1, Language::Dockerfile);
        assert!(files[0]
            .0
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("Dockerfile"));
    }

    #[test]
    fn test_collect_source_files_dockerfile_in_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("docker");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("Dockerfile"), "FROM node:18").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, Language::Dockerfile);
    }

    #[test]
    fn test_collect_source_files_dockerfile_not_matched_for_similar_names() {
        // Files like "Dockerfile.dev" have extension "dev", which doesn't match.
        // And "dockerfile" (lowercase) isn't matched by from_filename.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM rust:1.75").unwrap();
        std::fs::write(dir.path().join("Dockerfile.dev"), "FROM rust:1.75").unwrap();
        std::fs::write(dir.path().join("dockerfile"), "FROM rust:1.75").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        // Only "Dockerfile" (exact match) should be found
        let dockerfiles: Vec<_> = files
            .iter()
            .filter(|(_, lang)| *lang == Language::Dockerfile)
            .collect();
        assert_eq!(
            dockerfiles.len(),
            1,
            "Only exact 'Dockerfile' should match, got {:?}",
            dockerfiles
                .iter()
                .map(|(p, _)| p.file_name().unwrap().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_collect_source_files_mixed_new_languages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.lua"), "print('hi')").unwrap();
        std::fs::write(dir.path().join("config.yaml"), "key: val").unwrap();
        std::fs::write(dir.path().join("data.json"), "{}").unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM alpine").unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(files.len(), 5, "Should find all 5 files");

        let langs: std::collections::HashSet<_> = files.iter().map(|(_, l)| *l).collect();
        assert!(langs.contains(&Language::Lua));
        assert!(langs.contains(&Language::Yaml));
        assert!(langs.contains(&Language::Json));
        assert!(langs.contains(&Language::Dockerfile));
        assert!(langs.contains(&Language::Rust));
    }

    #[test]
    fn test_format_code_search_context_new_language_fences() {
        for (lang, expected_fence) in [
            (Language::Yaml, "```yaml"),
            (Language::Json, "```json"),
            (Language::Lua, "```lua"),
            (Language::Dockerfile, "```dockerfile"),
        ] {
            let chunk = make_chunk("test.x", None, "top_level", lang, 1, 5, "some code");
            let results = vec![make_search_result(chunk, 0.75)];
            let ctx = super::format_code_search_context(&results);
            assert!(
                ctx.contains(expected_fence),
                "Expected fence '{}' for {:?}, got:\n{}",
                expected_fence,
                lang,
                ctx
            );
        }
    }

    #[test]
    fn test_format_code_search_context_new_language_names() {
        for (lang, expected_name) in [
            (Language::Yaml, "YAML"),
            (Language::Json, "JSON"),
            (Language::Lua, "Lua"),
            (Language::Dockerfile, "Dockerfile"),
        ] {
            let chunk = make_chunk("test.x", Some("sym"), "function", lang, 1, 5, "code");
            let results = vec![make_search_result(chunk, 0.8)];
            let ctx = super::format_code_search_context(&results);
            assert!(
                ctx.contains(&format!("**Language:** {}", expected_name)),
                "Expected language name '{}' for {:?}, got:\n{}",
                expected_name,
                lang,
                ctx
            );
        }
    }

    // --- Tests for CodeIndexer with_force_reindex ---

    #[test]
    fn test_code_indexer_with_force_reindex_true() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client).with_force_reindex(true);
        assert!(indexer.force_reindex);
    }

    #[test]
    fn test_code_indexer_with_force_reindex_false() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client).with_force_reindex(false);
        assert!(!indexer.force_reindex);
    }

    #[test]
    fn test_code_indexer_force_reindex_default_is_false() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);
        assert!(!indexer.force_reindex);
    }

    // --- Tests for build_code_search_query edge cases ---

    #[test]
    fn test_build_code_search_query_sentry_empty_culprit_not_included() {
        let mut issue = claudear_core::types::Issue::new("1", "S-1", "Err", "url", "sentry");
        issue.set_metadata("culprit", "");
        issue.set_metadata("error_type", "ValueError");

        let query = super::build_code_search_query(&issue);
        // Should have title + error_type, but NOT empty culprit
        assert!(query.contains("Err"));
        assert!(query.contains("ValueError"));
        // Check no double spaces
        let parts: Vec<&str> = query.split(' ').filter(|s| !s.is_empty()).collect();
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn test_build_code_search_query_sentry_all_fields_present() {
        let mut issue = claudear_core::types::Issue::new("1", "S-1", "Error", "url", "sentry");
        issue.description = Some("desc".to_string());
        issue.set_metadata("error_type", "TypeError");
        issue.set_metadata("error_value", "null ref");
        issue.set_metadata("culprit", "main.js");
        issue.set_metadata("filename", "src/main.js");
        issue.set_metadata("function", "handleClick");

        let query = super::build_code_search_query(&issue);
        let parts: Vec<&str> = query.split(' ').collect();
        // title, desc, error_type, error_value (2 words), culprit, filename, function
        assert!(parts.len() >= 8);
    }

    #[test]
    fn test_build_code_search_query_github_source() {
        let mut issue =
            claudear_core::types::Issue::new("1", "GH-1", "Bug report", "url", "github");
        issue.description = Some("Steps to reproduce".to_string());
        let query = super::build_code_search_query(&issue);
        assert_eq!(query, "Bug report Steps to reproduce");
    }

    // --- Tests for format_code_search_context edge cases ---

    #[test]
    fn test_format_code_search_context_zero_score() {
        let chunk = make_chunk("a.rs", None, "function", Language::Rust, 1, 1, "x");
        let results = vec![make_search_result(chunk, 0.0)];
        let ctx = super::format_code_search_context(&results);
        assert!(ctx.contains("Similarity: 0%"));
    }

    #[test]
    fn test_format_code_search_context_perfect_score() {
        let chunk = make_chunk("a.rs", None, "function", Language::Rust, 1, 1, "x");
        let results = vec![make_search_result(chunk, 1.0)];
        let ctx = super::format_code_search_context(&results);
        assert!(ctx.contains("Similarity: 100%"));
    }

    #[test]
    fn test_format_code_search_context_many_results() {
        let results: Vec<CodeSearchResult> = (1..=5)
            .map(|i| {
                let chunk = make_chunk(
                    &format!("src/file{}.rs", i),
                    Some(&format!("func_{}", i)),
                    "function",
                    Language::Rust,
                    i * 10,
                    i * 10 + 5,
                    &format!("fn func_{}() {{}}", i),
                );
                make_search_result(chunk, 1.0 - (i as f64 * 0.1))
            })
            .collect();

        let ctx = super::format_code_search_context(&results);

        for i in 1..=5 {
            assert!(
                ctx.contains(&format!("### {}.", i)),
                "Should contain result {}",
                i
            );
            assert!(ctx.contains(&format!("func_{}", i)));
        }
    }

    #[test]
    fn test_format_code_search_context_code_exactly_2000_chars() {
        let code = "x".repeat(2000);
        let chunk = make_chunk("a.rs", None, "function", Language::Rust, 1, 100, &code);
        let results = vec![make_search_result(chunk, 0.5)];
        let ctx = super::format_code_search_context(&results);
        // Exactly 2000 chars should NOT be truncated
        assert!(!ctx.contains("(truncated)"));
        assert!(ctx.contains(&"x".repeat(2000)));
    }

    #[test]
    fn test_format_code_search_context_code_2001_chars() {
        let code = "x".repeat(2001);
        let chunk = make_chunk("a.rs", None, "function", Language::Rust, 1, 100, &code);
        let results = vec![make_search_result(chunk, 0.5)];
        let ctx = super::format_code_search_context(&results);
        // 2001 chars should be truncated
        assert!(ctx.contains("(truncated)"));
    }

    // --- Tests for collect_source_files with symlinks ---

    #[test]
    fn test_collect_source_files_ignores_non_file_entries() {
        let dir = tempfile::tempdir().unwrap();
        // Create a subdirectory (not a file)
        let sub = dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        // Put a .rs file in it
        std::fs::write(sub.join("nested.rs"), "fn nested() {}").unwrap();
        // Put a file at root
        std::fs::write(dir.path().join("root.rs"), "fn root() {}").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let files = indexer.collect_source_files(dir.path());
        assert_eq!(
            files.len(),
            2,
            "Should find files in subdirs but not dirs themselves"
        );
    }

    // --- Tests for index_repo ---

    #[tokio::test]
    async fn test_index_repo_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let stats = indexer.index_repo("test-repo", dir.path()).await.unwrap();
        assert_eq!(stats.files_processed, 0);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(stats.files_failed, 0);
        assert_eq!(stats.symbols_extracted, 0);
        assert_eq!(stats.chunks_created, 0);
    }

    #[tokio::test]
    async fn test_index_repo_with_rust_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            r#"
fn main() {
    println!("Hello, world!");
}

fn helper() -> i32 {
    42
}
"#,
        )
        .unwrap();

        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(Arc::clone(&tracker), embedding_client);

        let stats = indexer.index_repo("test-repo", dir.path()).await.unwrap();
        assert!(stats.files_processed >= 1, "Should process at least 1 file");
        assert!(stats.chunks_created >= 1, "Should create at least 1 chunk");
    }

    #[tokio::test]
    async fn test_index_repo_deletes_chunks_when_embeddings_fail_to_save() {
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("tracker.db");
        let repository_path = directory.path().join("repository");
        std::fs::create_dir(&repository_path).unwrap();
        std::fs::write(
            repository_path.join("main.rs"),
            "fn main() {\n    println!(\"Hello, world!\");\n}\n",
        )
        .unwrap();

        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(claudear_storage::SqliteTracker::new(&database_path).unwrap());
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER abort_embeddings BEFORE INSERT ON code_chunk_embeddings \
                 BEGIN SELECT RAISE(ABORT, 'forced embedding failure'); END",
            )
            .unwrap();
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let result = indexer.index_repo("test-repo", &repository_path).await;
        assert!(
            result.is_err(),
            "A failed embedding save must fail the index run"
        );

        let orphans: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM code_chunks c \
                 LEFT JOIN code_chunk_embeddings e ON e.chunk_id = c.id \
                 WHERE e.id IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            orphans, 0,
            "Chunks whose embeddings failed to save must be deleted"
        );
    }

    #[tokio::test]
    async fn test_index_repo_incremental_skips_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }",
        )
        .unwrap();

        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(Arc::clone(&tracker), Arc::clone(&embedding_client));

        // First run
        let stats1 = indexer.index_repo("test-repo", dir.path()).await.unwrap();
        assert!(stats1.files_processed >= 1);

        // Second run with same content
        let indexer2 = CodeIndexer::new(Arc::clone(&tracker), embedding_client);
        let stats2 = indexer2.index_repo("test-repo", dir.path()).await.unwrap();
        assert_eq!(
            stats2.files_processed, 0,
            "Unchanged files should be skipped"
        );
        assert!(stats2.files_skipped >= 1);
    }

    #[tokio::test]
    async fn test_index_repo_force_reindex() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.rs"), "fn app() { let x = 1; }").unwrap();

        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(Arc::clone(&tracker), Arc::clone(&embedding_client));

        // First index
        let stats1 = indexer.index_repo("test-repo", dir.path()).await.unwrap();
        assert!(stats1.files_processed >= 1);

        // Force reindex
        let indexer2 =
            CodeIndexer::new(Arc::clone(&tracker), embedding_client).with_force_reindex(true);
        let stats2 = indexer2.index_repo("test-repo", dir.path()).await.unwrap();
        assert!(
            stats2.files_processed >= 1,
            "Force reindex should re-process files"
        );
    }

    #[tokio::test]
    async fn test_index_repo_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}").unwrap();
        std::fs::write(dir.path().join("c.py"), "def c(): pass").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let stats = indexer.index_repo("multi-repo", dir.path()).await.unwrap();
        assert!(stats.files_processed >= 2, "Should process multiple files");
    }

    // --- Tests for CodeSearchService ---

    #[tokio::test]
    async fn test_code_search_service_search_empty_repo() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let service = CodeSearchService::new(tracker, embedding_client);

        let results = service.search("test query", None, 10).await.unwrap();
        assert!(results.is_empty(), "Empty DB should return no results");
    }

    #[tokio::test]
    async fn test_code_search_service_find_similar_to_code() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let service = CodeSearchService::new(tracker, embedding_client);

        let results = service
            .find_similar_to_code("fn main() {}", None, 5)
            .await
            .unwrap();
        assert!(results.is_empty(), "Empty DB should return no results");
    }

    #[test]
    fn test_code_search_service_find_symbol_empty() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let service = CodeSearchService::new(tracker, embedding_client);

        let results = service.find_symbol("nonexistent", None, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_code_search_service_find_symbol_with_kind() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let service = CodeSearchService::new(tracker, embedding_client);

        let results = service
            .find_symbol("test", Some(SymbolKind::Function), None)
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_code_search_service_find_symbol_with_repo_id() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let service = CodeSearchService::new(tracker, embedding_client);

        let results = service.find_symbol("test", None, Some(1)).unwrap();
        assert!(results.is_empty());
    }

    // --- Tests for sha256_hex with various content types ---

    #[test]
    fn test_sha256_hex_binary_like_content() {
        let content = "\x00\x01\x02\x03\x7f";
        let hash = sha256_hex(content);
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // --- Tests for CODE_INDEX_VERSION constant ---

    #[test]
    fn test_code_index_version_is_not_empty() {
        assert!(!CODE_INDEX_VERSION.is_empty());
    }

    // --- Tests for index_repo with unreadable file (simulate by directory perm) ---

    #[tokio::test]
    async fn test_index_repo_skips_files_in_hidden_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let hidden = dir.path().join(".git");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("config.rs"), "fn git_config() {}").unwrap();
        std::fs::write(dir.path().join("visible.rs"), "fn visible() {}").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        let stats = indexer.index_repo("hidden-test", dir.path()).await.unwrap();
        // Only visible.rs should be processed
        assert!(stats.files_processed <= 1);
    }

    // --- Test with_config large file size ---

    #[test]
    fn test_code_indexer_with_config_large_file_size() {
        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::with_config(tracker, embedding_client, 10240, 128);

        assert_eq!(indexer.max_file_size, 10240 * 1024);
        assert_eq!(indexer.batch_size, 128);
    }

    // --- Test collect_source_files with root dir starting with dot ---

    #[test]
    fn test_collect_source_files_root_dir_with_dot_prefix() {
        let parent_dir = tempfile::tempdir().unwrap();
        let dot_dir = parent_dir.path().join(".my-project");
        std::fs::create_dir(&dot_dir).unwrap();
        std::fs::write(dot_dir.join("main.rs"), "fn main() {}").unwrap();

        let tracker = Arc::new(claudear_storage::SqliteTracker::in_memory().unwrap());
        let Some(embedding_client) = try_embedding_client() else {
            return;
        };
        let indexer = CodeIndexer::new(tracker, embedding_client);

        // The root dir itself starts with '.', but depth 0 always included
        let files = indexer.collect_source_files(&dot_dir);
        assert_eq!(
            files.len(),
            1,
            "Root dir with dot prefix should still be walked"
        );
    }

    // --- Test format_code_search_context with Ruby and PHP ---

    #[test]
    fn test_format_code_search_context_ruby_fence() {
        let chunk = make_chunk(
            "app.rb",
            None,
            "function",
            Language::Ruby,
            1,
            1,
            "puts 'hi'",
        );
        let results = vec![make_search_result(chunk, 0.5)];
        let ctx = super::format_code_search_context(&results);
        assert!(ctx.contains("```ruby"));
    }

    #[test]
    fn test_format_code_search_context_php_fence() {
        let chunk = make_chunk(
            "app.php",
            None,
            "function",
            Language::Php,
            1,
            1,
            "<?php echo 1;",
        );
        let results = vec![make_search_result(chunk, 0.5)];
        let ctx = super::format_code_search_context(&results);
        assert!(ctx.contains("```php"));
    }

    #[test]
    fn test_format_code_search_context_swift_fence() {
        let chunk = make_chunk(
            "app.swift",
            None,
            "function",
            Language::Swift,
            1,
            1,
            "print(1)",
        );
        let results = vec![make_search_result(chunk, 0.5)];
        let ctx = super::format_code_search_context(&results);
        assert!(ctx.contains("```swift"));
    }

    #[test]
    fn test_format_code_search_context_kotlin_fence() {
        let chunk = make_chunk(
            "app.kt",
            None,
            "function",
            Language::Kotlin,
            1,
            1,
            "fun main() {}",
        );
        let results = vec![make_search_result(chunk, 0.5)];
        let ctx = super::format_code_search_context(&results);
        assert!(ctx.contains("```kotlin"));
    }

    #[test]
    fn test_format_code_search_context_tsx_fence() {
        let chunk = make_chunk(
            "App.tsx",
            None,
            "function",
            Language::Tsx,
            1,
            1,
            "const App = () => <div/>;",
        );
        let results = vec![make_search_result(chunk, 0.5)];
        let ctx = super::format_code_search_context(&results);
        assert!(ctx.contains("```tsx"));
    }
}
