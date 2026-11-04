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
        version: 15,
        name: "user_directory",
        sql: include_str!("../../../migrations/V15__user_directory.sql"),
    },
];

/// Run all pending migrations against the given connection.
///
/// Creates the `schema_migrations` tracking table if it doesn't exist,
/// then applies any migrations whose version exceeds the current maximum.
pub fn run(connection: &Connection) -> Result<(), String> {
    run_migrations(connection, MIGRATIONS)
}

fn run_migrations(connection: &Connection, migrations: &[Migration]) -> Result<(), String> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )
        .map_err(|error| format!("Failed to create schema_migrations table: {error}"))?;

    let current: u32 = connection
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("Failed to query schema version: {error}"))?;

    migrations
        .iter()
        .filter(|migration| migration.version > current)
        .try_for_each(|migration| apply(connection, migration))
}

/// Apply one migration and record its version atomically, so a failure or crash
/// part-way never leaves a half-applied schema or a version that will re-run.
fn apply(connection: &Connection, migration: &Migration) -> Result<(), String> {
    let transaction = connection.unchecked_transaction().map_err(|error| {
        format!(
            "Failed to begin migration V{}_{}: {error}",
            migration.version, migration.name
        )
    })?;

    transaction.execute_batch(migration.sql).map_err(|error| {
        format!(
            "Migration V{}_{} failed: {error}",
            migration.version, migration.name
        )
    })?;

    transaction
        .execute(
            "INSERT INTO schema_migrations (version, name) VALUES (?1, ?2)",
            rusqlite::params![migration.version, migration.name],
        )
        .map_err(|error| format!("Failed to record migration V{}: {error}", migration.version))?;

    transaction
        .commit()
        .map_err(|error| format!("Failed to commit migration V{}: {error}", migration.version))
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_DIRECTORY_VERSION: u32 = 15;

    fn latest_version() -> u32 {
        MIGRATIONS
            .iter()
            .map(|migration| migration.version)
            .max()
            .unwrap()
    }

    fn applied_version(connection: &Connection) -> u32 {
        connection
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
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

    fn schema_object_exists(connection: &Connection, kind: &str, name: &str) -> bool {
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
                [kind, name],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    }

    fn table_exists(connection: &Connection, table: &str) -> bool {
        schema_object_exists(connection, "table", table)
    }

    fn index_exists(connection: &Connection, index: &str) -> bool {
        schema_object_exists(connection, "index", index)
    }

    fn column_exists(connection: &Connection, table: &str, column: &str) -> bool {
        connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                [table, column],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    }

    fn row_count(connection: &Connection, table: &str) -> i64 {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn migrated_connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        run(&connection).unwrap();
        connection
    }

    fn insert_user(connection: &Connection, email: &str) -> i64 {
        connection
            .execute(
                "INSERT INTO users (email, password_hash, name) VALUES (?1, 'hash', 'Name')",
                [email],
            )
            .unwrap();
        connection.last_insert_rowid()
    }

    fn insert_open_invite(
        connection: &Connection,
        id: &str,
        user_id: i64,
        created_by: Option<i64>,
    ) -> rusqlite::Result<usize> {
        connection.execute(
            "INSERT INTO user_invites (id, user_id, token_hash, created_by, expires_at)
             VALUES (?1, ?2, ?1 || '-hash', ?3, datetime('now', '+7 days'))",
            rusqlite::params![id, user_id, created_by],
        )
    }

    fn user_status(connection: &Connection, user_id: i64) -> String {
        connection
            .query_row("SELECT status FROM users WHERE id = ?1", [user_id], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn test_migrations_apply_to_fresh_db() {
        let connection = Connection::open_in_memory().unwrap();
        run(&connection).unwrap();

        assert_eq!(applied_version(&connection), latest_version());
        assert_eq!(applied_count(&connection), MIGRATIONS.len());

        assert!(table_exists(&connection, "fix_attempts"));
        assert!(column_exists(
            &connection,
            "fix_attempts",
            "answer_message_ids"
        ));
        assert!(column_exists(
            &connection,
            "pr_review_states",
            "last_issue_comment_id"
        ));
        assert!(column_exists(
            &connection,
            "pr_review_comments",
            "handled_at"
        ));
        assert!(column_exists(
            &connection,
            "pr_review_comments",
            "comment_kind"
        ));
        assert!(table_exists(&connection, "agent_instructions"));
        assert!(column_exists(&connection, "fix_attempts", "routing_intent"));
        assert!(table_exists(&connection, "deploy_qa_tips"));
        assert!(table_exists(&connection, "api_tokens"));
        assert!(column_exists(&connection, "fix_attempts", "heartbeat_at"));
        assert!(table_exists(&connection, "user_identities"));
        assert!(index_exists(&connection, "idx_user_identities_slug"));
        assert!(column_exists(&connection, "users", "status"));
        assert!(table_exists(&connection, "user_invites"));
        assert!(index_exists(&connection, "idx_user_invites_open"));
    }

    #[test]
    fn test_migrations_are_idempotent() {
        let connection = Connection::open_in_memory().unwrap();
        run(&connection).unwrap();
        run(&connection).unwrap();

        assert_eq!(applied_version(&connection), latest_version());
        assert_eq!(applied_count(&connection), MIGRATIONS.len());
    }

    #[test]
    fn test_user_directory_marks_existing_users_active() {
        let connection = Connection::open_in_memory().unwrap();
        let (before, _) = MIGRATIONS.split_at(
            MIGRATIONS
                .iter()
                .position(|migration| migration.version == USER_DIRECTORY_VERSION)
                .unwrap(),
        );
        run_migrations(&connection, before).unwrap();
        assert!(!column_exists(&connection, "users", "status"));
        let existing = insert_user(&connection, "existing@example.com");

        run(&connection).unwrap();

        assert_eq!(applied_version(&connection), latest_version());
        assert_eq!(user_status(&connection, existing), "active");
        let created = insert_user(&connection, "created@example.com");
        assert_eq!(user_status(&connection, created), "active");
    }

    #[test]
    fn test_user_status_rejects_unknown_values() {
        let connection = migrated_connection();
        let user = insert_user(&connection, "amy@example.com");

        connection
            .execute("UPDATE users SET status = 'invited' WHERE id = ?1", [user])
            .unwrap();
        assert_eq!(user_status(&connection, user), "invited");

        let update = connection.execute(
            "UPDATE users SET status = 'suspended' WHERE id = ?1",
            [user],
        );
        assert!(update.is_err(), "CHECK must reject an unknown status");

        let insert = connection.execute(
            "INSERT INTO users (email, password_hash, name, status) VALUES ('bob@example.com', 'hash', 'Bob', 'deleted')",
            [],
        );
        assert!(insert.is_err(), "CHECK must reject an unknown status");
        assert_eq!(user_status(&connection, user), "invited");
    }

    #[test]
    fn test_user_identities_slug_is_unique_when_set() {
        let connection = migrated_connection();
        let first = insert_user(&connection, "first@example.com");
        let second = insert_user(&connection, "second@example.com");
        let third = insert_user(&connection, "third@example.com");
        let insert_identity = |user_id: i64, slug: Option<&str>| {
            connection.execute(
                "INSERT INTO user_identities (user_id, slug) VALUES (?1, ?2)",
                rusqlite::params![user_id, slug],
            )
        };

        insert_identity(first, None).unwrap();
        insert_identity(second, None).unwrap();
        insert_identity(third, Some("jake")).unwrap();
        connection
            .execute(
                "UPDATE user_identities SET slug = 'jake' WHERE user_id = ?1",
                [first],
            )
            .expect_err("a slug may belong to only one console user");

        let linear_names: String = connection
            .query_row(
                "SELECT linear_names FROM user_identities WHERE user_id = ?1",
                [first],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(linear_names, "[]");
    }

    #[test]
    fn test_user_invites_allow_one_open_invite_per_user() {
        let connection = migrated_connection();
        let user = insert_user(&connection, "amy@example.com");

        insert_open_invite(&connection, "first", user, None).unwrap();
        insert_open_invite(&connection, "second", user, None)
            .expect_err("a second open invite must violate idx_user_invites_open");

        connection
            .execute(
                "UPDATE user_invites SET revoked_at = datetime('now') WHERE id = 'first'",
                [],
            )
            .unwrap();
        insert_open_invite(&connection, "reissued", user, None).unwrap();

        connection
            .execute(
                "UPDATE user_invites SET accepted_at = datetime('now') WHERE id = 'reissued'",
                [],
            )
            .unwrap();
        insert_open_invite(&connection, "after-accept", user, None).unwrap();
        assert_eq!(row_count(&connection, "user_invites"), 3);
    }

    #[test]
    fn test_deleting_a_user_cascades_to_the_user_directory() {
        let connection = migrated_connection();
        let admin = insert_user(&connection, "admin@example.com");
        let invited = insert_user(&connection, "amy@example.com");
        connection
            .execute(
                "INSERT INTO user_identities (user_id, slug) VALUES (?1, 'amy')",
                [invited],
            )
            .unwrap();
        insert_open_invite(&connection, "invite", invited, Some(admin)).unwrap();

        connection
            .execute("DELETE FROM users WHERE id = ?1", [admin])
            .unwrap();
        let created_by: Option<i64> = connection
            .query_row(
                "SELECT created_by FROM user_invites WHERE id = 'invite'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(created_by, None, "deleting the inviter keeps the invite");

        connection
            .execute("DELETE FROM users WHERE id = ?1", [invited])
            .unwrap();
        assert_eq!(row_count(&connection, "user_identities"), 0);
        assert_eq!(row_count(&connection, "user_invites"), 0);
    }

    #[test]
    fn test_failed_migration_leaves_no_partial_schema_or_version() {
        let connection = Connection::open_in_memory().unwrap();
        let migrations = [
            Migration {
                version: 1,
                name: "applied",
                sql: "CREATE TABLE applied_probe (id INTEGER PRIMARY KEY);",
            },
            Migration {
                version: 2,
                name: "broken",
                sql: "CREATE TABLE partial_probe (id INTEGER PRIMARY KEY);
                      ALTER TABLE applied_probe ADD COLUMN label TEXT;
                      ALTER TABLE missing_table ADD COLUMN label TEXT;",
            },
        ];

        let error = run_migrations(&connection, &migrations).unwrap_err();

        assert!(error.contains("V2_broken"), "unexpected error: {error}");
        assert_eq!(applied_version(&connection), 1);
        assert_eq!(applied_count(&connection), 1);
        assert!(table_exists(&connection, "applied_probe"));
        assert!(!table_exists(&connection, "partial_probe"));
        assert!(!column_exists(&connection, "applied_probe", "label"));
        assert!(connection.is_autocommit());

        let fixed = [
            Migration {
                version: 1,
                name: "applied",
                sql: "CREATE TABLE applied_probe (id INTEGER PRIMARY KEY);",
            },
            Migration {
                version: 2,
                name: "fixed",
                sql: "CREATE TABLE partial_probe (id INTEGER PRIMARY KEY);
                      ALTER TABLE applied_probe ADD COLUMN label TEXT;",
            },
        ];
        run_migrations(&connection, &fixed).unwrap();
        assert_eq!(applied_version(&connection), 2);
        assert!(table_exists(&connection, "partial_probe"));
        assert!(column_exists(&connection, "applied_probe", "label"));
    }

    #[test]
    fn test_all_tables_created() {
        let connection = Connection::open_in_memory().unwrap();
        run(&connection).unwrap();

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
            "user_identities",
            "user_invites",
        ];

        for table in expected_tables {
            assert!(
                table_exists(&connection, table),
                "Table '{table}' should exist"
            );
        }
    }
}
