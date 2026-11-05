use super::lookup::Lookup;
use claudear_core::error::Result;
use regex_lite::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::LazyLock;

const EF_CONSTRUCTION: usize = 200;
const MAXIMUM_CONNECTIONS: usize = 16;

static DIMENSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)float32\s*\[\s*(\d+)\s*\]").expect("dimension pattern is valid")
});

static CAPACITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)max_elements\s*=\s*(\d+)").expect("capacity pattern is valid")
});

static VECTORLITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)^\s*create\s+virtual\s+table\b.*\busing\s+vectorlite\s*\(")
        .expect("vectorlite pattern is valid")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Declaration {
    pub(crate) dimension: usize,
    pub(crate) capacity: usize,
}

impl Declaration {
    pub(crate) fn parse(sql: &str) -> Option<Self> {
        Some(Self {
            dimension: Self::capture(&DIMENSION, sql)?,
            capacity: Self::capture(&CAPACITY, sql)?,
        })
    }

    pub(crate) fn read(connection: &Connection, table: &str) -> Result<Lookup> {
        let entry: Option<(bool, Option<String>)> = connection
            .query_row(
                "SELECT type = 'table', sql FROM sqlite_master WHERE name = ?1",
                params![table],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((is_table, sql)) = entry else {
            return Ok(Lookup::Absent);
        };
        let Some(sql) = sql.filter(|sql| is_table && VECTORLITE.is_match(sql)) else {
            return Ok(Lookup::Foreign);
        };
        Ok(Self::parse(&sql).map_or(Lookup::Unreadable, Lookup::Declared))
    }

    pub(crate) fn create_statement(&self, table: &str) -> String {
        format!(
            "CREATE VIRTUAL TABLE {table} USING vectorlite(embedding float32[{dimension}] cosine, hnsw(max_elements={capacity}, ef_construction={EF_CONSTRUCTION}, M={MAXIMUM_CONNECTIONS}))",
            dimension = self.dimension,
            capacity = self.capacity,
        )
    }

    fn capture(pattern: &Regex, sql: &str) -> Option<usize> {
        pattern.captures(sql)?.get(1)?.as_str().parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_single_line() {
        let declaration = Declaration {
            dimension: 768,
            capacity: 100_000,
        };
        assert_eq!(
            Declaration::parse(&declaration.create_statement("vectors")),
            Some(declaration)
        );
    }

    #[test]
    fn test_parse_multi_line() {
        let sql = r#"CREATE VIRTUAL TABLE code_chunk_vectors USING vectorlite(
                embedding float32[384] cosine,
                hnsw(max_elements=100000, ef_construction=200, M=16)
            )"#;
        assert_eq!(
            Declaration::parse(sql),
            Some(Declaration {
                dimension: 384,
                capacity: 100_000,
            })
        );
    }

    #[test]
    fn test_parse_tolerates_spacing_and_case() {
        let sql = "create virtual table t using VECTORLITE(embedding FLOAT32 [ 8 ] cosine, hnsw(MAX_ELEMENTS = 42))";
        assert_eq!(
            Declaration::parse(sql),
            Some(Declaration {
                dimension: 8,
                capacity: 42,
            })
        );
    }

    #[test]
    fn test_parse_missing_dimension() {
        assert_eq!(
            Declaration::parse("CREATE VIRTUAL TABLE t USING vectorlite(hnsw(max_elements=10))"),
            None
        );
    }

    #[test]
    fn test_parse_missing_capacity() {
        assert_eq!(
            Declaration::parse(
                "CREATE VIRTUAL TABLE t USING vectorlite(embedding float32[4] cosine)"
            ),
            None
        );
    }

    #[test]
    fn test_read_absent() {
        let connection = Connection::open_in_memory().unwrap();
        assert_eq!(
            Declaration::read(&connection, "vectors").unwrap(),
            Lookup::Absent
        );
    }

    #[test]
    fn test_read_ordinary_table_is_foreign() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE vectors(embedding BLOB)")
            .unwrap();
        assert_eq!(
            Declaration::read(&connection, "vectors").unwrap(),
            Lookup::Foreign
        );
    }

    #[test]
    fn test_read_view_is_foreign() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE VIEW vectors AS SELECT 1 AS embedding")
            .unwrap();
        assert_eq!(
            Declaration::read(&connection, "vectors").unwrap(),
            Lookup::Foreign
        );
    }
}
