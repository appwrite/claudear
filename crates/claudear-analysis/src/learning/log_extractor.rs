//! System 1: Extract structured learnings from Claude execution logs.

use claudear_core::types::ExtractedLearnings;
use std::path::Path;

pub struct LogExtractor;

impl LogExtractor {
    /// Read stdout log file and extract structured learnings via pattern matching.
    pub fn extract_learnings_from_log(
        log_path: &Path,
    ) -> claudear_core::error::Result<ExtractedLearnings> {
        let content = std::fs::read_to_string(log_path).map_err(|e| {
            claudear_core::error::Error::Other(format!(
                "Failed to read log file '{}': {}",
                log_path.display(),
                e
            ))
        })?;

        Ok(Self::extract_from_text(&content))
    }

    /// Extract learnings from log text content.
    pub fn extract_from_text(content: &str) -> ExtractedLearnings {
        let lesson = abnegate_learn::Extractor::extract(content);
        ExtractedLearnings {
            root_cause: lesson.root_cause,
            files_modified: lesson.files,
            strategy_used: lesson.approach,
            tests_added: lesson.tests,
            key_decisions: lesson.decisions,
        }
    }

    /// Extract learnings with LLM if available, falling back to heuristics.
    pub fn extract_with_llm(
        log_path: &Path,
        llm: Option<&dyn crate::llm::LlmAnalyzer>,
    ) -> claudear_core::error::Result<ExtractedLearnings> {
        let content = std::fs::read_to_string(log_path).map_err(|e| {
            claudear_core::error::Error::Other(format!(
                "Failed to read log file '{}': {}",
                log_path.display(),
                e
            ))
        })?;
        if let Some(analyzer) = llm {
            if let Some(analysis) = analyzer.extract_learnings(&content) {
                return Ok(analysis.learnings);
            }
        }
        Ok(Self::extract_from_text(&content))
    }

    /// Create a compact summary string for storage.
    pub fn summarize(learnings: &ExtractedLearnings) -> String {
        let mut parts = Vec::new();

        if let Some(rc) = &learnings.root_cause {
            parts.push(format!("Root cause: {}", rc));
        }

        if !learnings.files_modified.is_empty() {
            let count = learnings.files_modified.len();
            let preview: Vec<&str> = learnings
                .files_modified
                .iter()
                .take(5)
                .map(|s| s.as_str())
                .collect();
            parts.push(format!(
                "Files modified ({}): {}",
                count,
                preview.join(", ")
            ));
        }

        if let Some(strategy) = &learnings.strategy_used {
            parts.push(format!("Strategy: {}", strategy));
        }

        if learnings.tests_added {
            parts.push("Tests were added/run".to_string());
        }

        if parts.is_empty() {
            "No learnings extracted".to_string()
        } else {
            parts.join("; ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_from_text_with_diff() {
        let log = r#"
Reading file src/main.rs
--- a/src/handler.rs
+++ b/src/handler.rs
The issue was the missing null check in the handler.
Running cargo test
All tests passed.
"#;
        let learnings = LogExtractor::extract_from_text(log);
        assert!(learnings.root_cause.is_some());
        assert!(learnings
            .root_cause
            .as_ref()
            .unwrap()
            .contains("missing null check"));
        assert!(learnings
            .files_modified
            .contains(&"src/handler.rs".to_string()));
        assert!(learnings
            .files_modified
            .contains(&"src/main.rs".to_string()));
        assert!(learnings.tests_added);
        assert_eq!(learnings.strategy_used.as_deref(), Some("test_driven"));
    }

    #[test]
    fn test_extract_empty_log() {
        let learnings = LogExtractor::extract_from_text("");
        assert!(learnings.root_cause.is_none());
        assert!(learnings.files_modified.is_empty());
        assert!(!learnings.tests_added);
    }

    #[test]
    fn test_summarize() {
        let learnings = ExtractedLearnings {
            root_cause: Some("missing import".to_string()),
            files_modified: vec!["src/lib.rs".to_string()],
            strategy_used: Some("direct_fix".to_string()),
            tests_added: false,
            key_decisions: Vec::new(),
        };
        let summary = LogExtractor::summarize(&learnings);
        assert!(summary.contains("Root cause: missing import"));
        assert!(summary.contains("src/lib.rs"));
    }

    #[test]
    fn test_extract_multiple_root_cause_patterns() {
        // "The bug was" pattern
        let log = "The bug was an off-by-one error in the loop boundary check.";
        let learnings = LogExtractor::extract_from_text(log);
        assert!(learnings.root_cause.is_some());
        assert!(learnings
            .root_cause
            .as_ref()
            .unwrap()
            .contains("off-by-one"));

        // "root cause" pattern
        let log2 =
            "After investigation, root cause: the config file was not being parsed correctly.";
        let learnings2 = LogExtractor::extract_from_text(log2);
        assert!(learnings2.root_cause.is_some());
        assert!(learnings2
            .root_cause
            .as_ref()
            .unwrap()
            .contains("config file"));

        // "fixed by" pattern
        let log3 = "Fixed by adding proper error handling to the database connection.";
        let learnings3 = LogExtractor::extract_from_text(log3);
        assert!(learnings3.root_cause.is_some());
        assert!(learnings3
            .root_cause
            .as_ref()
            .unwrap()
            .contains("error handling"));
    }

    #[test]
    fn test_extract_short_root_cause_ignored() {
        // Root cause text <= 10 chars should be ignored
        let log = "The issue was foo";
        let learnings = LogExtractor::extract_from_text(log);
        assert!(learnings.root_cause.is_none());
    }

    #[test]
    fn test_extract_deduplicates_files() {
        let log = r#"
Read src/main.rs
Edit src/main.rs
Read src/main.rs
Read src/lib.rs
"#;
        let learnings = LogExtractor::extract_from_text(log);
        let main_count = learnings
            .files_modified
            .iter()
            .filter(|f| f.as_str() == "src/main.rs")
            .count();
        assert_eq!(main_count, 1, "src/main.rs should appear exactly once");
    }

    #[test]
    fn test_extract_test_detection_variants() {
        let cases = vec![
            ("Running npm test", true),
            ("pytest tests/test_api.py", true),
            ("make test", true),
            ("jest --coverage", true),
            ("test passed", true),
            ("test failed with 2 errors", true),
            ("just reading some code", false),
        ];
        for (line, expected) in cases {
            let learnings = LogExtractor::extract_from_text(line);
            assert_eq!(learnings.tests_added, expected, "Failed for: {}", line);
        }
    }

    #[test]
    fn test_strategy_direct_fix_with_diff_no_tests() {
        let log = "+++ b/src/handler.rs\n--- a/src/handler.rs";
        let learnings = LogExtractor::extract_from_text(log);
        assert_eq!(learnings.strategy_used.as_deref(), Some("direct_fix"));
    }

    #[test]
    fn test_strategy_investigation_no_diff_with_files() {
        let log = "Read src/main.rs\nRead src/config.rs";
        let learnings = LogExtractor::extract_from_text(log);
        assert_eq!(
            learnings.strategy_used.as_deref(),
            Some("investigation_then_fix")
        );
    }

    #[test]
    fn test_strategy_unknown_no_content() {
        let log = "Hello world, no files here.";
        let learnings = LogExtractor::extract_from_text(log);
        assert_eq!(learnings.strategy_used.as_deref(), Some("unknown"));
    }

    #[test]
    fn test_summarize_empty_learnings() {
        let learnings = ExtractedLearnings {
            root_cause: None,
            files_modified: vec![],
            strategy_used: None,
            tests_added: false,
            key_decisions: vec![],
        };
        let summary = LogExtractor::summarize(&learnings);
        assert_eq!(summary, "No learnings extracted");
    }

    #[test]
    fn test_summarize_tests_added_flag() {
        let learnings = ExtractedLearnings {
            root_cause: None,
            files_modified: vec![],
            strategy_used: None,
            tests_added: true,
            key_decisions: vec![],
        };
        let summary = LogExtractor::summarize(&learnings);
        assert!(summary.contains("Tests were added/run"));
    }

    #[test]
    fn test_summarize_truncates_files_to_5() {
        let learnings = ExtractedLearnings {
            root_cause: None,
            files_modified: (0..10).map(|i| format!("src/file{}.rs", i)).collect(),
            strategy_used: None,
            tests_added: false,
            key_decisions: vec![],
        };
        let summary = LogExtractor::summarize(&learnings);
        assert!(summary.contains("Files modified (10)"));
        // Should show "file0" through "file4" but not "file5"
        assert!(summary.contains("file4"));
    }

    #[test]
    fn test_extract_diff_files_from_markers() {
        let log = "+++ b/src/api.rs\n--- a/src/api.rs\n+++ b/src/models.rs";
        let learnings = LogExtractor::extract_from_text(log);
        assert!(learnings.files_modified.contains(&"src/api.rs".to_string()));
        assert!(learnings
            .files_modified
            .contains(&"src/models.rs".to_string()));
    }

    #[test]
    fn test_extract_file_paths_various_prefixes() {
        let log = r#"
Reading lib/utils.rb
Editing app/controllers/user_controller.rb
Found pkg/server/handler.go
Checking internal/auth/middleware.go
Editing cmd/main.go
"#;
        let learnings = LogExtractor::extract_from_text(log);
        assert!(learnings
            .files_modified
            .contains(&"lib/utils.rb".to_string()));
        assert!(learnings
            .files_modified
            .contains(&"app/controllers/user_controller.rb".to_string()));
        assert!(learnings
            .files_modified
            .contains(&"pkg/server/handler.go".to_string()));
        assert!(learnings
            .files_modified
            .contains(&"internal/auth/middleware.go".to_string()));
        assert!(learnings
            .files_modified
            .contains(&"cmd/main.go".to_string()));
    }

    #[test]
    fn test_extract_learnings_from_log_file() {
        // Write a temp file and exercise the file-based method
        let dir = std::env::temp_dir();
        let log_path = dir.join("claudear_test_log.txt");
        std::fs::write(
            &log_path,
            r#"
Reading src/main.rs
The issue was a missing null check in the handler function.
Edit src/handler.rs
Running cargo test
All tests passed.
+++ b/src/handler.rs
--- a/src/handler.rs
Created pull request #42
"#,
        )
        .unwrap();

        let learnings = LogExtractor::extract_learnings_from_log(&log_path).unwrap();
        assert!(learnings.root_cause.is_some());
        assert!(learnings
            .root_cause
            .as_ref()
            .unwrap()
            .contains("missing null check"));
        assert!(learnings
            .files_modified
            .contains(&"src/main.rs".to_string()));
        assert!(learnings
            .files_modified
            .contains(&"src/handler.rs".to_string()));
        assert!(learnings.tests_added);
        assert_eq!(learnings.strategy_used.as_deref(), Some("test_driven"));

        // Cleanup
        let _ = std::fs::remove_file(&log_path);
    }

    #[test]
    fn test_extract_learnings_from_log_nonexistent_file() {
        let result = LogExtractor::extract_learnings_from_log(Path::new(
            "/tmp/nonexistent_claudear_test.txt",
        ));
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_learnings_from_log_empty_file() {
        let dir = std::env::temp_dir();
        let log_path = dir.join("claudear_test_empty_log.txt");
        std::fs::write(&log_path, "").unwrap();

        let learnings = LogExtractor::extract_learnings_from_log(&log_path).unwrap();
        assert!(learnings.root_cause.is_none());
        assert!(learnings.files_modified.is_empty());
        assert!(!learnings.tests_added);
        assert_eq!(learnings.strategy_used.as_deref(), Some("unknown"));

        let _ = std::fs::remove_file(&log_path);
    }

    #[test]
    fn test_root_cause_too_long_ignored() {
        // Root cause text >= 500 chars should be ignored
        let long_text = "a".repeat(500);
        let log = format!("The issue was {}", long_text);
        let learnings = LogExtractor::extract_from_text(&log);
        assert!(learnings.root_cause.is_none());
    }

    #[test]
    fn test_root_cause_exactly_11_chars() {
        let log = "The issue was 12345678901";
        let learnings = LogExtractor::extract_from_text(log);
        // 11 chars > 10, should be accepted
        assert!(learnings.root_cause.is_some());
    }

    #[test]
    fn test_root_cause_exactly_10_chars() {
        let log = "The issue was 1234567890";
        let learnings = LogExtractor::extract_from_text(log);
        // 10 chars == 10, not > 10, should be rejected
        assert!(learnings.root_cause.is_none());
    }

    #[test]
    fn test_multiple_root_causes_first_wins() {
        let log =
            "The issue was the missing import statement.\nThe bug was a typo in the variable name.";
        let learnings = LogExtractor::extract_from_text(log);
        assert!(learnings
            .root_cause
            .as_ref()
            .unwrap()
            .contains("missing import"));
    }

    #[test]
    fn test_diff_marker_with_empty_path() {
        let log = "+++ b/\n--- a/";
        let learnings = LogExtractor::extract_from_text(log);
        // Empty path after trimming should not be added
        assert!(learnings.files_modified.is_empty());
    }

    #[test]
    fn test_case_insensitive_test_detection() {
        let log = "CARGO TEST passed\nNPM TEST completed\nPYTEST --verbose";
        let learnings = LogExtractor::extract_from_text(log);
        assert!(learnings.tests_added);
    }

    #[test]
    fn test_summarize_all_sections() {
        let learnings = ExtractedLearnings {
            root_cause: Some("missing import statement".to_string()),
            files_modified: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
            strategy_used: Some("test_driven".to_string()),
            tests_added: true,
            key_decisions: vec![],
        };
        let summary = LogExtractor::summarize(&learnings);
        assert!(summary.contains("Root cause:"));
        assert!(summary.contains("Files modified (2)"));
        assert!(summary.contains("Strategy: test_driven"));
        assert!(summary.contains("Tests were added/run"));
        // Parts should be joined by "; "
        assert!(summary.contains("; "));
    }

    #[test]
    fn test_extract_only_diff_markers_no_regex_files() {
        // Only diff markers, no regex-matching file paths
        let log = "+++ b/README.md\n--- a/README.md";
        let learnings = LogExtractor::extract_from_text(log);
        assert!(learnings.files_modified.contains(&"README.md".to_string()));
        assert_eq!(learnings.strategy_used.as_deref(), Some("direct_fix"));
    }
}
