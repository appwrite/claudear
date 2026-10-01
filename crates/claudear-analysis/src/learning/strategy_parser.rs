//! System 6: Parse Claude execution logs into strategy fingerprints.

use chrono::Utc;
use claudear_core::types::StrategyFingerprint;
use std::path::Path;

pub struct StrategyParser;

impl StrategyParser {
    /// Parse execution log into a strategy fingerprint.
    pub fn parse_from_log(
        log_path: &Path,
        attempt_id: i64,
    ) -> claudear_core::error::Result<StrategyFingerprint> {
        let content = std::fs::read_to_string(log_path).map_err(|e| {
            claudear_core::error::Error::Other(format!(
                "Failed to read log file '{}': {}",
                log_path.display(),
                e
            ))
        })?;

        Ok(Self::parse_from_text(&content, attempt_id))
    }

    /// Parse strategy from text content.
    pub fn parse_from_text(content: &str, attempt_id: i64) -> StrategyFingerprint {
        let fingerprint = abnegate_learn::Fingerprint::parse(
            content,
            &["Read", "Edit", "Write", "Bash", "Grep", "Glob"],
        );

        StrategyFingerprint {
            id: 0,
            attempt_id,
            files_explored: fingerprint.files,
            tests_run: fingerprint.tests,
            tools_used: fingerprint.actions.into_iter().collect(),
            fix_approach: fingerprint.approach,
            strategy_summary: fingerprint.summary,
            fix_quality_score: None,
            created_at: Utc::now(),
        }
    }

    /// Parse strategy with LLM if available, falling back to heuristics.
    pub fn parse_with_llm(
        log_path: &Path,
        attempt_id: i64,
        llm: Option<&dyn crate::llm::LlmAnalyzer>,
    ) -> claudear_core::error::Result<StrategyFingerprint> {
        let content = std::fs::read_to_string(log_path).map_err(|e| {
            claudear_core::error::Error::Other(format!(
                "Failed to read log file '{}': {}",
                log_path.display(),
                e
            ))
        })?;
        if let Some(analyzer) = llm {
            if let Some(analysis) = analyzer.extract_learnings(&content) {
                let mut fp = Self::parse_from_text(&content, attempt_id);
                fp.fix_approach = analysis.fix_approach;
                fp.strategy_summary = analysis.strategy_summary;
                return Ok(fp);
            }
        }
        Ok(Self::parse_from_text(&content, attempt_id))
    }

    /// Format strategy suggestions for prompt injection.
    pub fn format_strategy_suggestions(strategies: &[StrategyFingerprint]) -> String {
        if strategies.is_empty() {
            return String::new();
        }

        let mut output = String::from("# Successful Fix Strategies for This Repo\n\n");

        for (i, strategy) in strategies.iter().take(3).enumerate() {
            output.push_str(&format!(
                "{}. **{}**: {}\n",
                i + 1,
                strategy.fix_approach,
                strategy.strategy_summary
            ));

            if !strategy.files_explored.is_empty() {
                let files: Vec<&str> = strategy
                    .files_explored
                    .iter()
                    .take(5)
                    .map(|s| s.as_str())
                    .collect();
                output.push_str(&format!("   Key files: {}\n", files.join(", ")));
            }

            if let Some(score) = strategy.fix_quality_score {
                output.push_str(&format!("   Quality score: {:.2}\n", score));
            }
        }

        output.push('\n');
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_parse_tdd_strategy() {
        let log = r#"
Read src/main.rs
Edit src/handler.rs
Running cargo test
Edit tests/test_handler.rs
cargo test passed
"#;
        let fp = StrategyParser::parse_from_text(log, 1);
        assert_eq!(fp.fix_approach, "tdd");
        assert!(fp.tests_run >= 1);
        assert!(fp.files_explored.contains(&"src/main.rs".to_string()));
        assert!(fp.files_explored.contains(&"src/handler.rs".to_string()));
    }

    #[test]
    fn test_parse_direct_fix() {
        let log = r#"
Read src/config.rs
Edit src/config.rs
Write src/new_file.rs
"#;
        let fp = StrategyParser::parse_from_text(log, 2);
        assert_eq!(fp.fix_approach, "direct_fix");
        assert_eq!(fp.tests_run, 0);
    }

    #[test]
    fn test_parse_investigation() {
        let log = r#"
Read src/main.rs
Read src/handler.rs
Read src/config.rs
Read src/types.rs
Read src/storage/mod.rs
Read src/storage/sqlite.rs
Edit src/handler.rs
"#;
        let fp = StrategyParser::parse_from_text(log, 3);
        assert_eq!(fp.fix_approach, "investigation");
    }

    #[test]
    fn test_format_strategy_suggestions() {
        let strategies = vec![StrategyFingerprint {
            id: 1,
            attempt_id: 1,
            files_explored: vec!["src/main.rs".to_string()],
            tests_run: 2,
            tools_used: HashMap::new(),
            fix_approach: "tdd".to_string(),
            strategy_summary: "1 files explored, 2 tests run, approach: tdd".to_string(),
            fix_quality_score: Some(0.9),
            created_at: Utc::now(),
        }];
        let output = StrategyParser::format_strategy_suggestions(&strategies);
        assert!(output.contains("tdd"));
        assert!(output.contains("Quality score: 0.90"));
    }

    #[test]
    fn test_format_empty_strategies() {
        assert!(StrategyParser::format_strategy_suggestions(&[]).is_empty());
    }

    #[test]
    fn test_parse_empty_log() {
        let fp = StrategyParser::parse_from_text("", 1);
        assert_eq!(fp.fix_approach, "unknown");
        assert!(fp.files_explored.is_empty());
        assert_eq!(fp.tests_run, 0);
        assert!(fp.tools_used.is_empty());
    }

    #[test]
    fn test_parse_exploration_only() {
        // With only Reads and no Edits, read_count (3) > edit_count (0) * 2
        // so this is categorized as "investigation"
        let log = "Read src/main.rs\nRead src/lib.rs\nRead src/config.rs\n";
        let fp = StrategyParser::parse_from_text(log, 1);
        assert_eq!(fp.fix_approach, "investigation");
        assert_eq!(fp.files_explored.len(), 3);
    }

    #[test]
    fn test_parse_true_exploration() {
        // "Looking at src/main.rs" has no tool keywords (Read/Edit/etc.)
        // but file paths are detected. read_count=0, edit_count=0.
        // Since read_count (0) is not > 0, falls through to "unknown".
        let log = "Looking at src/main.rs for context\n";
        let fp = StrategyParser::parse_from_text(log, 1);
        assert_eq!(fp.fix_approach, "unknown");
        // But the file should still be detected via regex
        assert!(fp.files_explored.contains(&"src/main.rs".to_string()));
    }

    #[test]
    fn test_tool_counting() {
        let log = "Read file\nRead another\nRead more\nEdit something\nBash command\nGrep search\nGlob pattern\n";
        let fp = StrategyParser::parse_from_text(log, 1);
        assert_eq!(*fp.tools_used.get("Read").unwrap_or(&0), 3);
        assert_eq!(*fp.tools_used.get("Edit").unwrap_or(&0), 1);
        assert_eq!(*fp.tools_used.get("Bash").unwrap_or(&0), 1);
        assert_eq!(*fp.tools_used.get("Grep").unwrap_or(&0), 1);
        assert_eq!(*fp.tools_used.get("Glob").unwrap_or(&0), 1);
    }

    #[test]
    fn test_test_run_counting() {
        let log = "cargo test\ncargo test -- --test-threads=1\npytest\n";
        let fp = StrategyParser::parse_from_text(log, 1);
        assert_eq!(fp.tests_run, 3);
    }

    #[test]
    fn test_file_deduplication() {
        let log = "Read src/main.rs\nEdit src/main.rs\nRead src/main.rs again\n";
        let fp = StrategyParser::parse_from_text(log, 1);
        let main_count = fp
            .files_explored
            .iter()
            .filter(|f| f.as_str() == "src/main.rs")
            .count();
        assert_eq!(main_count, 1);
    }

    #[test]
    fn test_format_strategy_suggestions_multiple() {
        let strategies = vec![
            StrategyFingerprint {
                id: 1,
                attempt_id: 1,
                files_explored: vec!["src/a.rs".to_string()],
                tests_run: 0,
                tools_used: HashMap::new(),
                fix_approach: "direct_fix".to_string(),
                strategy_summary: "summary1".to_string(),
                fix_quality_score: None,
                created_at: Utc::now(),
            },
            StrategyFingerprint {
                id: 2,
                attempt_id: 2,
                files_explored: vec!["src/b.rs".to_string(), "src/c.rs".to_string()],
                tests_run: 5,
                tools_used: HashMap::new(),
                fix_approach: "tdd".to_string(),
                strategy_summary: "summary2".to_string(),
                fix_quality_score: Some(0.75),
                created_at: Utc::now(),
            },
        ];
        let output = StrategyParser::format_strategy_suggestions(&strategies);
        assert!(output.contains("1. **direct_fix**"));
        assert!(output.contains("2. **tdd**"));
        assert!(output.contains("Quality score: 0.75"));
        assert!(output.contains("Key files: src/b.rs, src/c.rs"));
    }

    #[test]
    fn test_format_strategy_suggestions_limits_to_3() {
        let strategies: Vec<StrategyFingerprint> = (0..5)
            .map(|i| StrategyFingerprint {
                id: i,
                attempt_id: i,
                files_explored: vec![],
                tests_run: 0,
                tools_used: HashMap::new(),
                fix_approach: format!("approach_{}", i),
                strategy_summary: format!("summary_{}", i),
                fix_quality_score: None,
                created_at: Utc::now(),
            })
            .collect();
        let output = StrategyParser::format_strategy_suggestions(&strategies);
        assert!(output.contains("approach_0"));
        assert!(output.contains("approach_1"));
        assert!(output.contains("approach_2"));
        assert!(!output.contains("approach_3"));
        assert!(!output.contains("approach_4"));
    }

    #[test]
    fn test_parse_various_test_frameworks() {
        let cases = vec!["npm test", "pytest tests/", "make test", "jest --watch"];
        for case in cases {
            let fp = StrategyParser::parse_from_text(case, 1);
            assert!(fp.tests_run > 0, "Should detect test run for: {}", case);
        }
    }

    #[test]
    fn test_parse_file_paths_various_prefixes() {
        let log = "src/main.rs\nlib/utils.py\napp/handler.js\npkg/server.go\ntests/test_api.rs\n";
        let fp = StrategyParser::parse_from_text(log, 1);
        assert!(fp.files_explored.contains(&"src/main.rs".to_string()));
        assert!(fp.files_explored.contains(&"lib/utils.py".to_string()));
        assert!(fp.files_explored.contains(&"app/handler.js".to_string()));
        assert!(fp.files_explored.contains(&"pkg/server.go".to_string()));
        assert!(fp.files_explored.contains(&"tests/test_api.rs".to_string()));
    }

    #[test]
    fn test_summary_format() {
        let log = "Read src/a.rs\nEdit src/b.rs\ncargo test\n";
        let fp = StrategyParser::parse_from_text(log, 1);
        assert!(fp.strategy_summary.contains("files explored"));
        assert!(fp.strategy_summary.contains("tests run"));
        assert!(fp.strategy_summary.contains("approach:"));
    }

    #[test]
    fn test_parse_from_log_file() {
        let dir = std::env::temp_dir();
        let log_path = dir.join("claudear_test_strategy_log.txt");
        std::fs::write(
            &log_path,
            r#"
Read src/main.rs
Read src/config.rs
Edit src/handler.rs
Running cargo test
test passed
Write src/new_helper.rs
"#,
        )
        .unwrap();

        let fp = StrategyParser::parse_from_log(&log_path, 42).unwrap();
        assert_eq!(fp.attempt_id, 42);
        assert_eq!(fp.fix_approach, "tdd");
        assert!(fp.tests_run >= 1);
        assert!(fp.files_explored.contains(&"src/main.rs".to_string()));
        assert!(fp.files_explored.contains(&"src/config.rs".to_string()));
        assert!(fp.files_explored.contains(&"src/handler.rs".to_string()));
        assert!(*fp.tools_used.get("Read").unwrap_or(&0) >= 2);
        assert!(*fp.tools_used.get("Edit").unwrap_or(&0) >= 1);

        let _ = std::fs::remove_file(&log_path);
    }

    #[test]
    fn test_parse_from_log_nonexistent_file() {
        let result = StrategyParser::parse_from_log(
            std::path::Path::new("/tmp/nonexistent_claudear_strategy.txt"),
            1,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_tool_in_different_context_still_counted() {
        // "Read" as part of a sentence still gets counted
        let log = "I Read the file carefully\nThe Edit was applied\n";
        let fp = StrategyParser::parse_from_text(log, 1);
        assert!(*fp.tools_used.get("Read").unwrap_or(&0) >= 1);
        assert!(*fp.tools_used.get("Edit").unwrap_or(&0) >= 1);
    }

    #[test]
    fn test_attempt_id_preserved() {
        let fp = StrategyParser::parse_from_text("", 99);
        assert_eq!(fp.attempt_id, 99);
    }

    #[test]
    fn test_fix_quality_score_starts_none() {
        let fp = StrategyParser::parse_from_text("", 1);
        assert!(fp.fix_quality_score.is_none());
    }

    #[test]
    fn test_format_strategy_no_files_no_quality() {
        let strategies = vec![StrategyFingerprint {
            id: 1,
            attempt_id: 1,
            files_explored: vec![],
            tests_run: 0,
            tools_used: HashMap::new(),
            fix_approach: "unknown".to_string(),
            strategy_summary: "0 files, no tests".to_string(),
            fix_quality_score: None,
            created_at: Utc::now(),
        }];
        let output = StrategyParser::format_strategy_suggestions(&strategies);
        assert!(!output.contains("Key files:"));
        assert!(!output.contains("Quality score:"));
    }

    #[test]
    fn test_format_strategy_files_truncated_to_5() {
        let strategies = vec![StrategyFingerprint {
            id: 1,
            attempt_id: 1,
            files_explored: (0..10).map(|i| format!("src/file{}.rs", i)).collect(),
            tests_run: 0,
            tools_used: HashMap::new(),
            fix_approach: "investigation".to_string(),
            strategy_summary: "summary".to_string(),
            fix_quality_score: None,
            created_at: Utc::now(),
        }];
        let output = StrategyParser::format_strategy_suggestions(&strategies);
        assert!(output.contains("file4"));
        assert!(!output.contains("file5"));
    }

    #[test]
    fn test_multiple_test_frameworks_counted_separately() {
        let log = "cargo test\npytest\nnpm test\njest\nmake test\n";
        let fp = StrategyParser::parse_from_text(log, 1);
        assert_eq!(fp.tests_run, 5);
    }
}
