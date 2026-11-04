//! RAG orchestration service for code chat.

use super::llm::{self, LlmEngine};
use super::models::download::DownloadProgress;
use super::prompt;
use super::types::*;
use claudear_analysis::repo::code_index::{format_code_search_context, CodeSearchService};
use claudear_config::config::{ChatConfig, LlmModelConfig};
use claudear_core::error::{Error, Result};
use claudear_core::types::ChatViewer;
use claudear_storage::FixAttemptTracker;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// The error a chat request gets for a session it may not read, whether or
/// not the session exists.
pub const CHAT_SESSION_NOT_FOUND: &str = "Chat session not found";

/// Who a chat request acts for.
#[derive(Debug, Clone, Copy)]
pub enum ChatActor<'a> {
    /// A signed-in dashboard user, who owns the sessions they create.
    Viewer(&'a ChatViewer),
    /// The local `claudear chat` operator, whose sessions have no owner and so
    /// are visible in the dashboard only to admins.
    Operator,
}

impl ChatActor<'_> {
    fn owner_user_id(self) -> Option<i64> {
        match self {
            Self::Viewer(viewer) => Some(viewer.user_id),
            Self::Operator => None,
        }
    }

    fn can_continue(self, session: &ChatSession) -> bool {
        match self {
            Self::Viewer(viewer) => viewer.can_read(session),
            Self::Operator => session.owner_user_id.is_none(),
        }
    }
}

/// Service that orchestrates RAG-powered code chat.
///
/// Combines code search (embeddings + vector search), conversation history (SQLite),
/// and local LLM inference (llama-cpp-2) into a streaming chat pipeline.
pub struct ChatService {
    config: ChatConfig,
    llm_config: LlmModelConfig,
    search: CodeSearchService,
    tracker: Arc<dyn FixAttemptTracker>,
    llm: Mutex<Option<Arc<LlmEngine>>>,
    download_progress: Arc<Mutex<Option<Arc<DownloadProgress>>>>,
}

impl ChatService {
    /// Create a new chat service.
    pub fn new(
        config: ChatConfig,
        llm_config: LlmModelConfig,
        search: CodeSearchService,
        tracker: Arc<dyn FixAttemptTracker>,
    ) -> Self {
        Self {
            config,
            llm_config,
            search,
            tracker,
            llm: Mutex::new(None),
            download_progress: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a new chat service with a pre-loaded LLM engine.
    pub fn with_engine(
        config: ChatConfig,
        llm_config: LlmModelConfig,
        search: CodeSearchService,
        tracker: Arc<dyn FixAttemptTracker>,
        engine: Arc<LlmEngine>,
    ) -> Self {
        Self {
            config,
            llm_config,
            search,
            tracker,
            llm: Mutex::new(Some(engine)),
            download_progress: Arc::new(Mutex::new(None)),
        }
    }

    /// Lazily load the LLM model on first use.
    fn get_or_load_llm(&self) -> Result<Arc<LlmEngine>> {
        let mut guard = self
            .llm
            .lock()
            .map_err(|e| Error::Other(format!("Lock poisoned: {e}")))?;
        if let Some(ref engine) = *guard {
            return Ok(engine.clone());
        }

        let model_path = expand_tilde(&self.llm_config.model_path);
        llm::validate_model_path(&model_path)?;

        let llm_config = llm::LlmConfig {
            model_path,
            context_length: self.llm_config.context_length,
            gpu_layers: self.llm_config.gpu_layers,
            threads: self.llm_config.threads,
            timeout: if self.llm_config.inference_timeout_secs > 0 {
                Some(std::time::Duration::from_secs(
                    self.llm_config.inference_timeout_secs,
                ))
            } else {
                None
            },
        };

        let engine = Arc::new(LlmEngine::load(&llm_config)?);
        *guard = Some(engine.clone());
        Ok(engine)
    }

    /// Process a user message: retrieve context, build prompt, generate response.
    ///
    /// Returns a list of token strings (for streaming to client) and the source references.
    pub async fn chat(
        &self,
        actor: ChatActor<'_>,
        session_id: &str,
        message: &str,
        repo_id: Option<i64>,
        params_override: Option<&GenerationParamsOverride>,
    ) -> Result<(Vec<String>, Vec<ChatSource>)> {
        let prepared = self
            .prepare_chat(actor, session_id, message, repo_id, params_override)
            .await?;

        let tokens = prepared
            .llm
            .complete_streaming(&prepared.prompt, &prepared.gen_params)?;

        let full_response: String = tokens.join("");
        self.save_assistant_message(&prepared.session_id, &full_response, &prepared.sources)?;

        Ok((tokens, prepared.sources))
    }

    /// Prepare a chat request: create session, save user message, RAG search, build prompt, load LLM.
    ///
    /// A session that does not exist yet is created owned by `actor`; one
    /// `actor` may not continue fails with [`CHAT_SESSION_NOT_FOUND`].
    ///
    /// Returns a `PreparedChat` that the caller can use to drive generation (either
    /// via `complete_streaming` or `complete_streaming_channel`).
    pub async fn prepare_chat(
        &self,
        actor: ChatActor<'_>,
        session_id: &str,
        message: &str,
        repo_id: Option<i64>,
        params_override: Option<&GenerationParamsOverride>,
    ) -> Result<PreparedChat> {
        // 1. Ensure the session exists and belongs to the actor
        match self.tracker.get_chat_session(session_id)? {
            Some(session) if !actor.can_continue(&session) => {
                return Err(Error::Other(CHAT_SESSION_NOT_FOUND.to_string()));
            }
            Some(_) => {}
            None => {
                self.tracker
                    .create_chat_session(session_id, repo_id, actor.owner_user_id())?;
            }
        }

        // 2. Save user message
        self.tracker
            .save_chat_message(session_id, "user", message, None)?;

        // 3. Retrieve relevant code chunks
        let max_chunks = self.config.max_context_chunks;
        let search_results = self.search.search(message, repo_id, max_chunks).await?;

        // 4. Build sources list
        let sources: Vec<ChatSource> = search_results
            .iter()
            .map(|r| ChatSource {
                file_path: r.chunk.file_path.clone(),
                start_line: r.chunk.start_line as u32,
                end_line: r.chunk.end_line as u32,
                symbol_name: r.chunk.symbol_name.clone(),
                similarity: r.score as f32,
            })
            .collect();

        // 5. Format code context
        let code_context = format_code_search_context(&search_results);

        // 6. Load conversation history
        let history_messages = self
            .tracker
            .get_chat_history(session_id, self.config.max_history_messages)?;

        let history: Vec<ChatMessage> = history_messages
            .into_iter()
            .filter(|m| !(m.role == ChatRole::User && m.content == message))
            .collect();

        // 7. Build prompt
        let full_prompt = prompt::build_chat_prompt(&code_context, &history, message);

        // 8. Load LLM + build params
        let llm = self.get_or_load_llm()?;
        let gen_params = self.build_generation_params(params_override);

        Ok(PreparedChat {
            session_id: session_id.to_string(),
            prompt: full_prompt,
            sources,
            gen_params,
            llm,
        })
    }

    /// Save the assistant's (possibly partial) response after streaming ends.
    pub fn save_assistant_message(
        &self,
        session_id: &str,
        content: &str,
        sources: &[ChatSource],
    ) -> Result<()> {
        let sources_json = serde_json::to_string(sources).ok();
        self.tracker.save_chat_message(
            session_id,
            "assistant",
            content,
            sources_json.as_deref(),
        )?;
        Ok(())
    }

    /// Build generation params from config with optional client overrides.
    fn build_generation_params(
        &self,
        overrides: Option<&GenerationParamsOverride>,
    ) -> llm::GenerationParams {
        let mut params = llm::GenerationParams {
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            top_p: self.config.top_p,
            stop_sequences: vec![
                "<|end|>".to_string(),
                "<|user|>".to_string(),
                "<|system|>".to_string(),
            ],
        };

        if let Some(o) = overrides {
            if let Some(t) = o.max_tokens {
                params.max_tokens = t;
            }
            if let Some(t) = o.temperature {
                params.temperature = t;
            }
            if let Some(t) = o.top_p {
                params.top_p = t;
            }
        }

        params
    }

    /// Check if the model is configured and the file exists.
    pub fn is_model_available(&self) -> bool {
        let path = expand_tilde(&self.llm_config.model_path);
        path.exists() && path.is_file()
    }

    /// Check if the model is loaded.
    pub fn is_model_loaded(&self) -> bool {
        self.llm.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Get the model name from the file path.
    pub fn model_name(&self) -> String {
        self.llm_config
            .model_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string()
    }

    /// Get the configured context length.
    pub fn context_length(&self) -> u32 {
        self.llm_config.context_length
    }

    /// Get the configured model URL.
    pub fn model_url(&self) -> &str {
        &self.llm_config.model_url
    }

    /// Get the expanded model path.
    pub fn model_path_expanded(&self) -> PathBuf {
        expand_tilde(&self.llm_config.model_path)
    }

    /// Start downloading a GGUF model file.
    ///
    /// Returns the progress handle, or an error if a download is already in progress.
    pub fn start_download(&self, url: &str) -> Result<Arc<DownloadProgress>> {
        let mut guard = self
            .download_progress
            .lock()
            .map_err(|e| Error::Other(format!("Lock poisoned: {e}")))?;

        // Check if there's already an active download
        if let Some(ref existing) = *guard {
            if !existing.completed.load(Ordering::Relaxed)
                && !existing.failed.load(Ordering::Relaxed)
            {
                return Err(Error::Other("Download already in progress".to_string()));
            }
        }

        let progress = Arc::new(DownloadProgress::new());
        *guard = Some(progress.clone());

        let target = self.model_path_expanded();
        let url = url.to_string();
        let progress_clone = progress.clone();

        tokio::spawn(async move {
            if let Err(e) =
                super::models::download::download_gguf(&url, &target, progress_clone).await
            {
                tracing::error!(error = %e, "Model download failed");
            }
        });

        Ok(progress)
    }

    /// Get current download status: (downloaded_bytes, total_bytes, completed, failed).
    pub fn download_status(&self) -> Option<(u64, u64, bool, bool)> {
        let guard = self.download_progress.lock().ok()?;
        let progress = guard.as_ref()?;
        Some((
            progress.downloaded_bytes.load(Ordering::Relaxed),
            progress.total_bytes.load(Ordering::Relaxed),
            progress.completed.load(Ordering::Relaxed),
            progress.failed.load(Ordering::Relaxed),
        ))
    }
}

/// Expand `~` to the user's home directory.
pub fn expand_tilde(path: &std::path::Path) -> std::path::PathBuf {
    if let Some(s) = path.to_str() {
        if let Some(stripped) = s.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(stripped);
            }
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(owner_user_id: Option<i64>) -> ChatSession {
        ChatSession {
            id: "session".to_string(),
            repo_id: None,
            owner_user_id,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            messages: Vec::new(),
        }
    }

    #[test]
    fn a_viewer_continues_only_its_own_sessions_and_owns_new_ones() {
        let viewer = ChatViewer {
            user_id: 1,
            is_admin: true,
        };
        let actor = ChatActor::Viewer(&viewer);
        assert_eq!(actor.owner_user_id(), Some(1));
        assert!(actor.can_continue(&session(Some(1))));
        assert!(!actor.can_continue(&session(Some(2))));
    }

    #[test]
    fn the_operator_continues_only_ownerless_sessions_and_owns_none() {
        let actor = ChatActor::Operator;
        assert_eq!(actor.owner_user_id(), None);
        assert!(actor.can_continue(&session(None)));
        assert!(!actor.can_continue(&session(Some(1))));
    }

    #[test]
    fn test_expand_tilde() {
        let expanded = expand_tilde(std::path::Path::new("~/test/model.gguf"));
        assert!(!expanded.to_str().unwrap().starts_with("~/"));
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        let path = std::path::Path::new("/absolute/path/model.gguf");
        assert_eq!(expand_tilde(path), path);
    }

    #[test]
    fn test_expand_tilde_relative_path() {
        let path = std::path::Path::new("relative/path/model.gguf");
        assert_eq!(expand_tilde(path), path);
    }

    #[test]
    fn test_expand_tilde_only_tilde_no_slash() {
        // "~somethingelse" should NOT be expanded (only ~/...)
        let path = std::path::Path::new("~somethingelse/model.gguf");
        assert_eq!(expand_tilde(path), path);
    }
}
