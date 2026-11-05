//! SQL predicates that keep an [`AttemptScope`](crate::AttemptScope)'s excluded
//! work out of a query. Each reads the excluded source from the positional
//! parameter `?{index}`, bound to `AttemptScope::excluded_source`, which is NULL
//! when the scope excludes nothing.

/// Rows attributed to a source by `column`; rows with no source are kept.
pub(super) fn source(column: &str, index: usize) -> String {
    format!("(?{index} IS NULL OR {column} IS NULL OR {column} <> ?{index} COLLATE NOCASE)")
}

/// Metric rows attributed to a source by `column`. A row with no source
/// aggregates every source's work, so it is kept only when nothing is excluded.
pub(super) fn metric_source(column: &str, index: usize) -> String {
    format!("(?{index} IS NULL OR ({column} IS NOT NULL AND {column} <> ?{index} COLLATE NOCASE))")
}

/// Rows linked to a fix attempt by `column`; rows linked to none are kept.
pub(super) fn attempt(column: &str, index: usize) -> String {
    format!(
        "(?{index} IS NULL OR {column} IS NULL OR NOT EXISTS (\
         SELECT 1 FROM fix_attempts scoped_attempt \
         WHERE scoped_attempt.id = {column} AND scoped_attempt.source = ?{index} COLLATE NOCASE))"
    )
}

/// PR rows aliased `alias`, by the source of their issue and of their attempt.
pub(super) fn pull_request(alias: &str, index: usize) -> String {
    format!(
        "({} AND {})",
        source(&format!("{alias}.issue_source"), index),
        attempt(&format!("{alias}.attempt_id"), index)
    )
}

/// Rows attributed to a JSON array of sources in `column`, kept only when no
/// source in it is excluded.
pub(super) fn sources(column: &str, index: usize) -> String {
    format!(
        "(?{index} IS NULL OR NOT EXISTS (\
         SELECT 1 FROM json_each(CASE WHEN json_valid({column}) THEN {column} ELSE '[]' END) scoped_source \
         WHERE scoped_source.value = ?{index} COLLATE NOCASE))"
    )
}
