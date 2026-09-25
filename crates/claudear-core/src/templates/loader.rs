//! Template loading from files and database.

use crate::error::Result;
use crate::types::Issue;
use std::path::{Path, PathBuf};

/// Loads templates from various sources.
pub struct TemplateLoader {
    project_dir: PathBuf,
    agent_md_cache: std::sync::RwLock<Option<Option<String>>>,
}

impl TemplateLoader {
    /// Create a new template loader.
    pub fn new(project_dir: impl Into<PathBuf>) -> Self {
        Self {
            project_dir: project_dir.into(),
            agent_md_cache: std::sync::RwLock::new(None),
        }
    }

    /// Check if the project has an AGENT.md file.
    pub fn has_agent_md(&self) -> bool {
        self.get_agent_md_path().exists()
    }

    /// Get the path to AGENT.md.
    fn get_agent_md_path(&self) -> PathBuf {
        self.project_dir.join("AGENT.md")
    }

    /// Load AGENT.md content if it exists.
    pub fn load_agent_md(&self) -> Option<String> {
        // Check cache first
        {
            let cache = self.agent_md_cache.read().unwrap();
            if let Some(ref cached) = *cache {
                return cached.clone();
            }
        }

        // Load from file
        let result = self.load_agent_md_uncached();

        // Cache the result
        {
            let mut cache = self.agent_md_cache.write().unwrap();
            *cache = Some(result.clone());
        }

        result
    }

    fn load_agent_md_uncached(&self) -> Option<String> {
        let path = self.get_agent_md_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    tracing::debug!(component = "templates", path = ?path, "Loaded AGENT.md");
                    Some(content)
                }
                Err(e) => {
                    tracing::warn!(component = "templates", error = %e, "Failed to read AGENT.md");
                    None
                }
            }
        } else {
            tracing::debug!(component = "templates", path = ?path, "No AGENT.md found");
            None
        }
    }

    /// Clear the AGENT.md cache (useful for testing or hot-reload).
    pub fn clear_cache(&self) {
        let mut cache = self.agent_md_cache.write().unwrap();
        *cache = None;
    }

    /// Get the appropriate template for an issue.
    /// Priority:
    /// 1. AGENT.md content (prepended to default template)
    /// 2. Database template (future)
    /// 3. Default template based on source
    pub fn get_template(&self, issue: &Issue) -> Result<String> {
        let agent_md = self.load_agent_md();
        let base_template = self.get_default_template(&issue.source);

        // Templates that render `{{agent_md}}` themselves would get it twice
        let renders_agent_md = base_template.contains("{{#if has_agent_md}}");
        Ok(
            if let Some(md) = agent_md.as_ref().filter(|_| !renders_agent_md) {
                format!("{}\n\n---\n\n{}", md.trim(), base_template)
            } else {
                base_template.to_string()
            },
        )
    }

    /// Get the default template for a source type.
    fn get_default_template(&self, source: &str) -> &'static str {
        match source.to_lowercase().as_str() {
            "linear" => super::DEFAULT_LINEAR_TEMPLATE,
            "sentry" => super::DEFAULT_SENTRY_TEMPLATE,
            _ => super::DEFAULT_FIX_TEMPLATE,
        }
    }

    /// Check if a custom template file exists at a path.
    pub fn template_exists_at(&self, path: impl AsRef<Path>) -> bool {
        path.as_ref().exists()
    }

    /// Load a template from a specific file path.
    pub fn load_from_file(&self, path: impl AsRef<Path>) -> Result<String> {
        let content = std::fs::read_to_string(path.as_ref())
            .map_err(|e| crate::error::Error::config(format!("Failed to load template: {}", e)))?;
        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_loader() -> (TemplateLoader, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let loader = TemplateLoader::new(temp_dir.path());
        (loader, temp_dir)
    }

    #[test]
    fn test_has_agent_md_false() {
        let (loader, _temp) = create_test_loader();
        assert!(!loader.has_agent_md());
    }

    #[test]
    fn test_has_agent_md_true() {
        let (loader, temp) = create_test_loader();
        std::fs::write(temp.path().join("AGENT.md"), "# Agent Instructions").unwrap();
        assert!(loader.has_agent_md());
    }

    #[test]
    fn test_load_agent_md() {
        let (loader, temp) = create_test_loader();
        std::fs::write(temp.path().join("AGENT.md"), "Custom instructions").unwrap();

        let content = loader.load_agent_md();
        assert_eq!(content, Some("Custom instructions".to_string()));
    }

    #[test]
    fn test_load_agent_md_cached() {
        let (loader, temp) = create_test_loader();
        std::fs::write(temp.path().join("AGENT.md"), "Original").unwrap();

        // First load
        let content1 = loader.load_agent_md();
        assert_eq!(content1, Some("Original".to_string()));

        // Modify file
        std::fs::write(temp.path().join("AGENT.md"), "Modified").unwrap();

        // Should still return cached
        let content2 = loader.load_agent_md();
        assert_eq!(content2, Some("Original".to_string()));

        // Clear cache and reload
        loader.clear_cache();
        let content3 = loader.load_agent_md();
        assert_eq!(content3, Some("Modified".to_string()));
    }

    #[test]
    fn test_get_template_with_agent_md() {
        let (loader, temp) = create_test_loader();
        std::fs::write(temp.path().join("AGENT.md"), "# Custom Agent Rules").unwrap();

        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "github",
        );
        let template = loader.get_template(&issue).unwrap();

        assert!(template.contains("# Custom Agent Rules"));
        assert!(template.contains("---")); // Separator
    }

    #[test]
    fn test_rendered_prompt_includes_agent_md_exactly_once() {
        use crate::templates::{TemplateContext, TemplateRenderer};
        let (loader, temp) = create_test_loader();
        std::fs::write(temp.path().join("AGENT.md"), "# Custom Agent Rules").unwrap();

        for source in ["sentry", "linear", "github"] {
            let issue = Issue::new("123", "PROJ-123", "Fix bug", "https://example.com", source);
            let template = loader.get_template(&issue).unwrap();
            let context = TemplateContext::new(issue, "ctx".to_string())
                .with_agent_md(loader.load_agent_md());
            let prompt = TemplateRenderer::new().render(&template, &context);
            assert_eq!(
                prompt.matches("# Custom Agent Rules").count(),
                1,
                "{source} prompt should carry AGENT.md once"
            );
        }
    }

    #[test]
    fn test_get_template_without_agent_md() {
        let (loader, _temp) = create_test_loader();

        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix bug",
            "https://example.com",
            "linear",
        );
        let template = loader.get_template(&issue).unwrap();

        // Should use default template
        assert!(template.contains("Linear issue"));
    }

    #[test]
    fn test_get_default_template_linear() {
        let (loader, _temp) = create_test_loader();
        let template = loader.get_default_template("linear");
        assert!(template.contains("Linear"));
    }

    #[test]
    fn test_get_default_template_sentry() {
        let (loader, _temp) = create_test_loader();
        let template = loader.get_default_template("sentry");
        assert!(template.contains("Sentry"));
    }

    #[test]
    fn test_get_default_template_github() {
        let (loader, _temp) = create_test_loader();
        let template = loader.get_default_template("github");
        // GitHub might use a generic template
        assert!(!template.is_empty());
    }

    #[test]
    fn test_get_default_template_unknown() {
        let (loader, _temp) = create_test_loader();
        let template = loader.get_default_template("unknown");
        // Should return generic template
        assert!(!template.is_empty());
    }

    #[test]
    fn test_load_agent_md_empty_file() {
        let (loader, temp) = create_test_loader();
        std::fs::write(temp.path().join("AGENT.md"), "").unwrap();

        let content = loader.load_agent_md();
        assert_eq!(content, Some("".to_string()));
    }

    #[test]
    fn test_clear_cache() {
        let (loader, _temp) = create_test_loader();

        // Access something to populate cache (internally)
        let _ = loader.load_agent_md();

        // Clear should not panic
        loader.clear_cache();
    }

    #[test]
    fn test_get_template_for_sentry_issue() {
        let (loader, _temp) = create_test_loader();

        let issue = Issue::new(
            "456",
            "SENTRY-456",
            "TypeError",
            "https://sentry.io/123",
            "sentry",
        );
        let template = loader.get_template(&issue).unwrap();

        assert!(template.contains("Sentry"));
    }

    #[test]
    fn test_get_template_for_github_issue() {
        let (loader, _temp) = create_test_loader();

        let issue = Issue::new(
            "789",
            "#789",
            "Bug fix",
            "https://github.com/org/repo/issues/789",
            "github",
        );
        let template = loader.get_template(&issue).unwrap();

        // GitHub uses a generic template
        assert!(!template.is_empty());
    }

    #[test]
    fn test_custom_template_directory_creation() {
        let (loader, temp) = create_test_loader();
        let template_dir = temp.path().join(".claudear");
        std::fs::create_dir_all(&template_dir).unwrap();

        // Verify directory was created
        assert!(template_dir.exists());

        let issue = Issue::new("123", "LIN-123", "Test", "https://linear.app", "linear");
        // Template should still load (falls back to default)
        let template = loader.get_template(&issue).unwrap();
        assert!(!template.is_empty());
    }

    #[test]
    fn test_template_context_provided() {
        let (loader, _temp) = create_test_loader();

        let issue = Issue::new(
            "123",
            "PROJ-123",
            "Fix something",
            "https://example.com",
            "linear",
        );
        let template = loader.get_template(&issue).unwrap();

        // Default template should mention the issue
        assert!(
            template.contains("{{") || template.contains("PROJ-123") || template.contains("Linear")
        );
    }
}
