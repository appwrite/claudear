//! Minimal embedded migration runner for SQLite.
//!
//! SQL files are included at compile time from the `migrations/` directory.
//! Each file must be named `V<N>__<description>.sql` (e.g. `V1__initial_schema.sql`).
//! Migrations are applied in version order and tracked in a `schema_migrations` table.

use rusqlite::Connection;

/// A single compiled-in migration.
struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

/// All migrations, embedded at compile time.
/// Add new entries here when adding migration files.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_schema",
        sql: include_str!("../../../migrations/V1__initial_schema.sql"),
    },
    Migration {
        version: 2,
        name: "add_session_last_active",
        sql: include_str!("../../../migrations/V2__add_session_last_active.sql"),
    },
    Migration {
        version: 3,
        name: "action_runs",
        sql: include_str!("../../../migrations/V3__action_runs.sql"),
    },
    Migration {
        version: 4,
        name: "support_reply_ratings",
        sql: include_str!("../../../migrations/V4__support_reply_ratings.sql"),
    },
    Migration {
        version: 5,
        name: "issue_recurrence",
        sql: include_str!("../../../migrations/V5__issue_recurrence.sql"),
    },
    Migration {
        version: 6,
        name: "support_discord_knowledgebase",
        sql: include_str!("../../../migrations/V6__support_discord_knowledgebase.sql"),
    },
    Migration {
        version: 7,
        name: "retrieval_usage",
        sql: include_str!("../../../migrations/V7__retrieval_usage.sql"),
    },
    Migration {
        version: 8,
        name: "answer_message_ids",
        sql: include_str!("../../../migrations/V8__answer_message_ids.sql"),
    },
    Migration {
        version: 9,
        name: "pr_review_states_issue_comments",
        sql: include_str!("../../../migrations/V9__pr_review_states_issue_comments.sql"),
    },
    Migration {
        version: 10,
        name: "agent_instructions",
        sql: include_str!("../../../migrations/V10__agent_instructions.sql"),
    },
    Migration {
        version: 11,
        name: "fix_attempt_routing_intent",
        sql: include_str!("../../../migrations/V11__fix_attempt_routing_intent.sql"),
    },
    Migration {
        version: 12,
        name: "deploy_qa_tips",
        sql: include_str!("../../../migrations/V12__deploy_qa_tips.sql"),
    },
    Migration {
        version: 13,
        name: "api_tokens",
        sql: include_str!("../../../migrations/V13__api_tokens.sql"),
    },
    Migration {
        version: 14,
        name: "fix_attempt_heartbeat",
        sql: include_str!("../../../migrations/V14__fix_attempt_heartbeat.sql"),
    },
    Migration {
        version: 18,
        name: "security_inbox",
        sql: include_str!("../../../migrations/V18__security_inbox.sql"),
    },
];

/// Run all pending migrations against the given connection.
///
/// Creates the `schema_migrations` tracking table if it doesn't exist,
/// then applies any migrations whose version exceeds the current maximum.
pub fn run(conn: &Connection) -> Result<(), String> {
    // Create tracking table
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        );",
    )
    .map_err(|e| format!("Failed to create schema_migrations table: {e}"))?;

    // Get current version
    let current: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to query schema version: {e}"))?;

    // Apply pending migrations in order
    for m in MIGRATIONS {
        if m.version <= current {
            continue;
        }

        conn.execute_batch(m.sql)
            .map_err(|e| format!("Migration V{}_{} failed: {e}", m.version, m.name))?;

        conn.execute(
            "INSERT INTO schema_migrations (version, name) VALUES (?1, ?2)",
            rusqlite::params![m.version, m.name],
        )
        .map_err(|e| format!("Failed to record migration V{}: {e}", m.version))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latest_version() -> u32 {
        MIGRATIONS
            .iter()
            .map(|migration| migration.version)
            .max()
            .unwrap()
    }

    fn applied_version(connection: &Connection) -> u32 {
        connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn applied_count(connection: &Connection) -> usize {
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        usize::try_from(count).unwrap()
    }

    #[test]
    fn test_migrations_apply_to_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();
        run(&conn).unwrap();

        assert_eq!(applied_version(&conn), latest_version());
        assert_eq!(applied_count(&conn), MIGRATIONS.len());

        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='fix_attempts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Verify the V8 column exists on fix_attempts.
        let has_col: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('fix_attempts') WHERE name = 'answer_message_ids'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_col, 1);

        // Verify the V9 columns exist: the issue-comment cursor on pr_review_states
        // and the handled ledger on pr_review_comments.
        let has_issue_comment_col: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pr_review_states') WHERE name = 'last_issue_comment_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_issue_comment_col, 1);

        let has_handled_col: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pr_review_comments') WHERE name = 'handled_at'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_handled_col, 1);

        let has_kind_col: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pr_review_comments') WHERE name = 'comment_kind'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_kind_col, 1);

        // Verify the V10 table exists.
        let has_instructions: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_instructions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_instructions, 1);

        // Verify the V11 column exists on fix_attempts.
        let has_routing_intent: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('fix_attempts') WHERE name = 'routing_intent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_routing_intent, 1);

        let has_deploy_qa: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='deploy_qa_tips'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_deploy_qa, 1);
    }

    #[test]
    fn test_migrations_are_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        run(&conn).unwrap();
        run(&conn).unwrap();

        assert_eq!(applied_version(&conn), latest_version());
        assert_eq!(applied_count(&conn), MIGRATIONS.len());
    }

    fn unique_constraint_columns(connection: &Connection, table: &str) -> Vec<Vec<String>> {
        let mut statement = connection
            .prepare(
                "SELECT name FROM pragma_index_list(?1) WHERE \"unique\" = 1 AND origin = 'u' ORDER BY name",
            )
            .unwrap();
        let indexes: Vec<String> = statement
            .query_map([table], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        indexes
            .iter()
            .map(|index| {
                let mut statement = connection
                    .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
                    .unwrap();
                statement
                    .query_map([index], |row| row.get(0))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap()
            })
            .collect()
    }

    fn foreign_keys(connection: &Connection, table: &str) -> Vec<(String, String)> {
        let mut statement = connection
            .prepare(
                "SELECT \"from\", \"table\" FROM pragma_foreign_key_list(?1) ORDER BY \"from\"",
            )
            .unwrap();
        statement
            .query_map([table], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn test_security_inbox_foreign_keys() {
        let connection = Connection::open_in_memory().unwrap();
        run(&connection).unwrap();
        let pairs = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(column, table)| (column.to_string(), table.to_string()))
                .collect()
        };
        assert_eq!(
            foreign_keys(&connection, "security_vulnerabilities"),
            pairs(&[
                ("first_report_id", "security_reports"),
                ("fix_attempt_id", "fix_attempts")
            ])
        );
        assert_eq!(
            foreign_keys(&connection, "security_reports"),
            pairs(&[("vulnerability_id", "security_vulnerabilities")])
        );
        assert_eq!(
            foreign_keys(&connection, "security_replies"),
            pairs(&[("report_id", "security_reports")])
        );
        assert_eq!(
            foreign_keys(&connection, "security_awards"),
            pairs(&[
                ("report_id", "security_reports"),
                ("vulnerability_id", "security_vulnerabilities")
            ])
        );
    }

    #[test]
    fn test_security_replies_allow_one_fix_live_reply_per_report() {
        let connection = Connection::open_in_memory().unwrap();
        run(&connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO security_reports (source, conversation_id, short_id, mailbox_id, url, subject, reported_at, state, created_at, updated_at)
                 VALUES ('helpscout', '1', 'HS-1', '249891', '', '', '2026-11-04T00:00:00Z', 'tracking', '2026-11-04T00:00:00Z', '2026-11-04T00:00:00Z');
                 INSERT INTO security_replies (report_id, kind, status, claimed_at) VALUES (1, 'confirmed', 'sent', '2026-11-04T00:00:00Z');
                 INSERT INTO security_replies (report_id, kind, status, claimed_at) VALUES (1, 'fix_live', 'sent', '2026-11-04T00:00:00Z');",
            )
            .unwrap();
        assert!(connection
            .execute(
                "INSERT INTO security_replies (report_id, kind, status, claimed_at) VALUES (1, 'fix_live_rewarded', 'sending', '2026-11-04T00:00:00Z')",
                [],
            )
            .is_err());
    }

    #[test]
    fn test_security_inbox_schema() {
        let connection = Connection::open_in_memory().unwrap();
        run(&connection).unwrap();

        for table in [
            "security_vulnerabilities",
            "security_reports",
            "security_events",
            "security_replies",
            "security_awards",
        ] {
            let count: u32 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "Table '{table}' should exist");
        }

        assert_eq!(
            unique_constraint_columns(&connection, "security_reports"),
            vec![vec!["source".to_string(), "conversation_id".to_string()]]
        );
        assert_eq!(
            unique_constraint_columns(&connection, "security_replies"),
            vec![vec!["report_id".to_string(), "kind".to_string()]]
        );
        assert_eq!(
            unique_constraint_columns(&connection, "security_awards"),
            vec![vec!["vulnerability_id".to_string()]]
        );

        let defaulted_timestamps: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master AS tables, pragma_table_info(tables.name) AS columns
                 WHERE tables.type = 'table' AND tables.name LIKE 'security_%'
                   AND columns.name LIKE '%_at' AND columns.dflt_value IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(defaulted_timestamps, 0);
    }

    #[test]
    fn test_all_tables_created() {
        let conn = Connection::open_in_memory().unwrap();
        run(&conn).unwrap();

        let expected_tables = [
            "fix_attempts",
            "feedback_outcomes",
            "pr_review_states",
            "repositories",
            "repository_dependencies",
            "activity_log",
            "claude_executions",
            "pr_reviews",
            "pr_review_comments",
            "issues",
            "error_patterns",
            "processing_metrics",
            "prompt_experiments",
            "similar_issues",
            "qa_knowledge",
            "qa_usage",
            "question_channel_cursor",
            "repo_files",
            "inference_attempts",
            "prs",
            "regression_watches",
            "release_tracking",
            "regression_checks",
            "users",
            "sessions",
            "webhook_deliveries",
            "diff_analyses",
            "promoted_instructions",
            "repo_knowledge",
            "review_patterns",
            "strategy_fingerprints",
            "issue_clusters",
            "issue_cluster_members",
            "content_clusters",
            "severity_scores",
            "suppression_log",
            "code_symbols",
            "code_chunks",
            "code_chunk_embeddings",
            "code_index_metadata",
            "indexing_progress",
            "cross_repo_correlations",
            "code_complexity",
            "eval_snapshots",
            "eval_deltas",
            "chat_sessions",
            "chat_messages",
            "action_runs",
            "support_reply_ratings",
            "issue_recurrence",
            "retrieval_usage",
        ];

        for table in expected_tables {
            let count: u32 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "Table '{}' should exist", table);
        }
    }
}
