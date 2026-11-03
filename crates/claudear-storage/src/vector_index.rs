//! Vectorlite HNSW tables as a per-connection cache derived from the SQLite embedding tables.

pub(crate) mod access;
pub(crate) mod catalog;
pub(crate) mod declaration;
pub(crate) mod definition;
pub(crate) mod lookup;
pub(crate) mod progress;
pub(crate) mod session;
pub(crate) mod state;

use access::Access;
use claudear_core::error::{Error, Result};
use declaration::Declaration;
use definition::Definition;
use lookup::Lookup;
use progress::Progress;
use rusqlite::{params, Connection, OptionalExtension};
use session::Session;
use state::State;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub(crate) const REBUILD_BATCH_SIZE: usize = 256;

/// Upper bound on an embedding dimension, so malformed data cannot generate unbounded DDL.
/// Covers all common embedding models (up to 8192 dimensions).
const MAX_EMBEDDING_DIMENSION: usize = 16384;

const BYTES_PER_COMPONENT: usize = size_of::<f32>();

pub(crate) fn validate_dimension(dimension: usize) -> Result<()> {
    if dimension == 0 || dimension > MAX_EMBEDDING_DIMENSION {
        return Err(Error::storage(format!(
            "Embedding dimension {} is out of valid range (1..={})",
            dimension, MAX_EMBEDDING_DIMENSION
        )));
    }
    Ok(())
}

pub(crate) struct VectorIndex {
    definition: Definition,
    session: Arc<Session>,
    // Lock order: only ever taken while the tracker's connection guard is held, and the
    // connection lock is never requested while this one is held.
    state: Mutex<State>,
}

#[allow(dead_code)]
impl VectorIndex {
    pub(crate) fn new(definition: Definition, session: Arc<Session>) -> Self {
        Self {
            definition,
            session,
            state: Mutex::new(State::Unbuilt),
        }
    }

    pub(crate) fn table(&self) -> &'static str {
        self.definition.table
    }

    pub(crate) fn prepare_search(&self, connection: &Connection, dimension: usize) -> Result<bool> {
        let mut state = self.lock();
        self.refresh(&mut state, connection)?;
        self.ensure(
            &mut state,
            connection,
            dimension,
            Access::Read,
            REBUILD_BATCH_SIZE,
        )
    }

    pub(crate) fn upsert(&self, connection: &Connection, rowid: i64, embedding: &[f32]) {
        let mut state = self.lock();
        if let Err(error) = self.refresh(&mut state, connection) {
            tracing::warn!(table = self.table(), rowid, %error, "Failed to check the vector index generation; rebuilding on next use");
            Self::invalidate(&mut state);
            return;
        }
        match self.ensure(
            &mut state,
            connection,
            embedding.len(),
            Access::Write,
            REBUILD_BATCH_SIZE,
        ) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(table = self.table(), rowid, %error, "Failed to prepare the vector index; rebuilding on next use");
                Self::invalidate(&mut state);
                return;
            }
        }
        if let Err(error) = self.write(connection, rowid, &Self::blob(embedding)) {
            tracing::warn!(table = self.table(), rowid, %error, "Failed to write vector; rebuilding the index on next use");
            *state = State::Stale {
                dimension: embedding.len(),
            };
        }
    }

    pub(crate) fn remove(&self, connection: &Connection, rowids: &[i64]) {
        if rowids.is_empty() {
            return;
        }
        let mut state = self.lock();
        if let Err(error) = self.refresh(&mut state, connection) {
            tracing::warn!(table = self.table(), %error, "Failed to check the vector index generation; rebuilding on next use");
            Self::invalidate(&mut state);
            return;
        }
        let dimension = match *state {
            State::Unbuilt | State::Unavailable => return,
            State::Building { dimension, .. }
            | State::Built { dimension, .. }
            | State::Stale { dimension } => dimension,
        };
        if let Err(error) = self.delete(connection, rowids) {
            tracing::warn!(table = self.table(), %error, "Failed to remove vectors; rebuilding the index on next use");
            *state = State::Stale { dimension };
        }
    }

    pub(crate) fn rebuild_step(
        &self,
        connection: &Connection,
        batch_size: usize,
    ) -> Result<Progress> {
        let mut state = self.lock();
        self.refresh(&mut state, connection)?;
        match *state {
            State::Unavailable | State::Built { .. } => Ok(Progress::Complete),
            State::Building { .. } => {
                self.advance(&mut state, connection, batch_size)?;
                Ok(Self::progress(&state))
            }
            State::Unbuilt | State::Stale { .. } => {
                let Some(length) = self.newest_length(connection)? else {
                    return Ok(Progress::Complete);
                };
                let Some(dimension) = Self::dimension_of(length) else {
                    tracing::warn!(
                        table = self.table(),
                        length,
                        "Newest embedding has a length that is not a whole number of float32 components; skipping rebuild"
                    );
                    return Ok(Progress::Complete);
                };
                if self.ensure(&mut state, connection, dimension, Access::Write, batch_size)? {
                    Ok(Self::progress(&state))
                } else {
                    Ok(Progress::Complete)
                }
            }
        }
    }

    pub(crate) fn reset(&self, connection: &Connection) {
        let mut state = self.lock();
        if let Err(error) = self.refresh(&mut state, connection) {
            tracing::warn!(table = self.table(), %error, "Failed to check the vector index generation before reset");
            Self::invalidate(&mut state);
        }
        if matches!(*state, State::Unbuilt | State::Unavailable) {
            return;
        }
        if let Err(error) = self.drop_table(&mut state, connection) {
            tracing::warn!(table = self.table(), %error, "Failed to record the vector index reset");
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn refresh(&self, state: &mut State, connection: &Connection) -> Result<()> {
        if let State::Building { generation, .. } | State::Built { generation, .. } = *state {
            if self.session.generation(connection)? != generation {
                *state = State::Unbuilt;
            }
        }
        Ok(())
    }

    fn invalidate(state: &mut State) {
        if let State::Building { dimension, .. } | State::Built { dimension, .. } = *state {
            *state = State::Stale { dimension };
        }
    }

    fn ensure(
        &self,
        state: &mut State,
        connection: &Connection,
        dimension: usize,
        access: Access,
        batch_size: usize,
    ) -> Result<bool> {
        if dimension == 0 {
            return Ok(false);
        }
        validate_dimension(dimension)?;
        if !self.session.load_extension(connection) {
            *state = State::Unavailable;
            return Ok(false);
        }
        match *state {
            State::Unavailable => return Ok(false),
            State::Built {
                dimension: built, ..
            } if built == dimension => return Ok(true),
            State::Building {
                dimension: building,
                ..
            } if building == dimension => {
                self.advance(state, connection, batch_size)?;
                return Ok(true);
            }
            _ => {}
        }

        let lookup = Declaration::read(connection, self.table())?;
        if lookup == Lookup::Foreign {
            tracing::warn!(
                table = self.table(),
                "Table name is taken by something other than a vectorlite table; vector search is disabled for it"
            );
            *state = State::Unavailable;
            return Ok(false);
        }
        if access == Access::Read
            && self.authoritative_dimension(state, lookup, connection)? != Some(dimension)
        {
            return Ok(false);
        }

        let required = self
            .definition
            .minimum_capacity
            .max(self.count(connection, dimension)?.saturating_mul(2));
        self.session.generation(connection)?;

        let stale = matches!(*state, State::Stale { .. });
        let capacity = match lookup {
            Lookup::Declared(existing)
                if !stale && existing.dimension == dimension && existing.capacity >= required =>
            {
                existing.capacity
            }
            Lookup::Absent => {
                if !self.create_table(state, connection, dimension, required)? {
                    return Ok(false);
                }
                required
            }
            _ => {
                if !self.drop_table(state, connection)?
                    || !self.create_table(state, connection, dimension, required)?
                {
                    return Ok(false);
                }
                required
            }
        };

        *state = State::Building {
            dimension,
            capacity,
            cursor: 0,
            generation: self.session.generation(connection)?,
        };
        self.advance(state, connection, batch_size)?;
        Ok(true)
    }

    fn authoritative_dimension(
        &self,
        state: &State,
        lookup: Lookup,
        connection: &Connection,
    ) -> Result<Option<usize>> {
        match (*state, lookup) {
            (
                State::Building { dimension, .. }
                | State::Built { dimension, .. }
                | State::Stale { dimension },
                _,
            ) => Ok(Some(dimension)),
            (State::Unbuilt | State::Unavailable, Lookup::Declared(declaration)) => {
                Ok(Some(declaration.dimension))
            }
            (State::Unbuilt | State::Unavailable, _) => {
                Ok(self.newest_length(connection)?.and_then(Self::dimension_of))
            }
        }
    }

    fn drop_table(&self, state: &mut State, connection: &Connection) -> Result<bool> {
        if let Err(error) =
            connection.execute_batch(&format!("DROP TABLE IF EXISTS {}", self.table()))
        {
            tracing::warn!(table = self.table(), %error, "Failed to drop vector index; retrying on next use");
            Self::invalidate(state);
            return Ok(false);
        }
        *state = State::Unbuilt;
        self.session.acknowledge(connection)?;
        Ok(true)
    }

    fn create_table(
        &self,
        state: &mut State,
        connection: &Connection,
        dimension: usize,
        capacity: usize,
    ) -> Result<bool> {
        let declaration = Declaration {
            dimension,
            capacity,
        };
        if let Err(error) = connection.execute_batch(&declaration.create_statement(self.table())) {
            tracing::warn!(table = self.table(), dimension, capacity, %error, "Failed to create vector index; retrying on next use");
            Self::invalidate(state);
            return Ok(false);
        }
        self.session.acknowledge(connection)?;
        Ok(true)
    }

    fn advance(&self, state: &mut State, connection: &Connection, batch_size: usize) -> Result<()> {
        let State::Building {
            dimension,
            capacity,
            cursor,
            generation,
        } = *state
        else {
            return Ok(());
        };
        let limit = batch_size.max(1);
        let Definition {
            source_table,
            id_column,
            embedding_column,
            ..
        } = self.definition;
        let rows: Vec<(i64, Vec<u8>)> = connection
            .prepare_cached(&format!(
                "SELECT {id_column}, {embedding_column} FROM {source_table} WHERE {id_column} > ?1 AND {embedding_column} IS NOT NULL AND length({embedding_column}) = ?2 ORDER BY {id_column} LIMIT ?3"
            ))?
            .query_map(
                params![cursor, Self::byte_length(dimension), limit as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<rusqlite::Result<_>>()?;

        let mut failures: usize = 0;
        let mut first_failure: Option<rusqlite::Error> = None;
        for (rowid, blob) in &rows {
            if let Err(error) = self.write(connection, *rowid, blob) {
                failures += 1;
                first_failure.get_or_insert(error);
            }
        }
        if let Some(error) = first_failure {
            tracing::warn!(table = self.table(), failures, %error, "Skipped rows while rebuilding vector index");
        }

        let cursor = rows.last().map_or(cursor, |(rowid, _)| *rowid);
        *state = if rows.len() < limit {
            State::Built {
                dimension,
                capacity,
                generation,
            }
        } else {
            State::Building {
                dimension,
                capacity,
                cursor,
                generation,
            }
        };
        Ok(())
    }

    fn write(&self, connection: &Connection, rowid: i64, blob: &[u8]) -> rusqlite::Result<()> {
        connection
            .prepare_cached(&format!("DELETE FROM {} WHERE rowid = ?1", self.table()))?
            .execute(params![rowid])?;
        connection
            .prepare_cached(&format!(
                "INSERT INTO {}(rowid, embedding) VALUES (?1, ?2)",
                self.table()
            ))?
            .execute(params![rowid, blob])?;
        Ok(())
    }

    fn delete(&self, connection: &Connection, rowids: &[i64]) -> rusqlite::Result<()> {
        let mut statement =
            connection.prepare_cached(&format!("DELETE FROM {} WHERE rowid = ?1", self.table()))?;
        for rowid in rowids {
            statement.execute(params![rowid])?;
        }
        Ok(())
    }

    fn count(&self, connection: &Connection, dimension: usize) -> Result<usize> {
        let Definition {
            source_table,
            embedding_column,
            ..
        } = self.definition;
        let count: i64 = connection.query_row(
            &format!(
                "SELECT count(*) FROM {source_table} WHERE {embedding_column} IS NOT NULL AND length({embedding_column}) = ?1"
            ),
            params![Self::byte_length(dimension)],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    fn newest_length(&self, connection: &Connection) -> Result<Option<i64>> {
        let Definition {
            source_table,
            id_column,
            embedding_column,
            ..
        } = self.definition;
        Ok(connection
            .query_row(
                &format!(
                    "SELECT length({embedding_column}) FROM {source_table} WHERE {embedding_column} IS NOT NULL ORDER BY {id_column} DESC LIMIT 1"
                ),
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    fn dimension_of(length: i64) -> Option<usize> {
        let length = usize::try_from(length).ok()?;
        (length > 0 && length % BYTES_PER_COMPONENT == 0).then_some(length / BYTES_PER_COMPONENT)
    }

    fn byte_length(dimension: usize) -> i64 {
        i64::try_from(dimension * BYTES_PER_COMPONENT).unwrap_or(i64::MAX)
    }

    fn blob(embedding: &[f32]) -> Vec<u8> {
        embedding
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    fn progress(state: &State) -> Progress {
        match state {
            State::Building { .. } => Progress::Pending,
            _ => Progress::Complete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vectorlite::load_vectorlite_extension;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    const TABLE: &str = "test_vectors";
    const SOURCE: &str = "source";

    fn definition(table: &'static str, source_table: &'static str) -> Definition {
        Definition {
            table,
            source_table,
            id_column: "id",
            embedding_column: "embedding",
            minimum_capacity: 4,
        }
    }

    fn index() -> VectorIndex {
        VectorIndex::new(definition(TABLE, SOURCE), Arc::new(Session::new()))
    }

    fn load(connection: &Connection) -> bool {
        let Ok(path) = std::env::var("CLAUDEAR_VECTORLITE_PATH") else {
            eprintln!("skipping: CLAUDEAR_VECTORLITE_PATH is not set");
            return false;
        };
        load_vectorlite_extension(connection, Path::new(&path))
            .unwrap_or_else(|error| panic!("failed to load vectorlite from {path}: {error}"));
        true
    }

    fn database() -> (TempDir, PathBuf) {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("vectors.db");
        (directory, path)
    }

    fn open(path: &Path) -> Connection {
        Connection::open(path).unwrap()
    }

    fn create_source(connection: &Connection, source: &str) {
        connection
            .execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS {source}(id INTEGER PRIMARY KEY, embedding BLOB)"
            ))
            .unwrap();
    }

    fn embedding(seed: i64, dimension: usize) -> Vec<f32> {
        (0..dimension)
            .map(|component| {
                if component == 0 {
                    1.0
                } else {
                    seed as f32 * (component as f32 + 1.0)
                }
            })
            .collect()
    }

    fn insert_source(connection: &Connection, source: &str, id: i64, embedding: &[f32]) {
        connection
            .execute(
                &format!("INSERT INTO {source}(id, embedding) VALUES (?1, ?2)"),
                params![id, VectorIndex::blob(embedding)],
            )
            .unwrap();
    }

    fn insert_rows(
        connection: &Connection,
        source: &str,
        ids: std::ops::RangeInclusive<i64>,
        dimension: usize,
    ) {
        create_source(connection, source);
        for id in ids {
            insert_source(connection, source, id, &embedding(id, dimension));
        }
    }

    fn vector_exists(connection: &Connection, table: &str, rowid: i64) -> bool {
        connection
            .query_row(
                &format!("SELECT rowid FROM {table} WHERE rowid = ?1"),
                params![rowid],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .unwrap()
            .is_some()
    }

    fn master_sql(connection: &Connection, table: &str) -> Option<String> {
        connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = ?1",
                params![table],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    fn declared(connection: &Connection, table: &str) -> Declaration {
        match Declaration::read(connection, table).unwrap() {
            Lookup::Declared(declaration) => declaration,
            other => panic!("expected a vectorlite declaration for {table}, found {other:?}"),
        }
    }

    fn state(index: &VectorIndex) -> State {
        *index.lock()
    }

    fn rebuild(index: &VectorIndex, connection: &Connection) {
        while index.rebuild_step(connection, REBUILD_BATCH_SIZE).unwrap() == Progress::Pending {}
    }

    fn create_raw(connection: &Connection, table: &str, dimension: usize, capacity: usize) {
        connection
            .execute_batch(
                &Declaration {
                    dimension,
                    capacity,
                }
                .create_statement(table),
            )
            .unwrap();
    }

    #[test]
    fn test_vector_index_validate_dimension_zero() {
        assert!(validate_dimension(0).is_err());
    }

    #[test]
    fn test_vector_index_validate_dimension_too_large() {
        assert!(validate_dimension(MAX_EMBEDDING_DIMENSION + 1).is_err());
    }

    #[test]
    fn test_vector_index_validate_dimension_valid() {
        assert!(validate_dimension(1).is_ok());
        assert!(validate_dimension(128).is_ok());
        assert!(validate_dimension(MAX_EMBEDDING_DIMENSION).is_ok());
    }

    #[test]
    fn test_vector_index_catalog_shares_one_session() {
        let catalog = catalog::Catalog::new();
        let indexes = catalog.all();
        let tables: Vec<&str> = indexes.iter().map(|index| index.table()).collect();
        assert_eq!(
            tables,
            [
                "qa_question_embeddings",
                "issue_embedding_vectors",
                "outcome_embedding_vectors",
                "code_chunk_vectors",
                "discord_message_chunk_vectors",
            ]
        );
        assert!(indexes
            .iter()
            .all(|index| Arc::ptr_eq(&index.session, &catalog.qa.session)));
    }

    #[test]
    fn test_vector_index_foreign_table_is_terminally_unavailable() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(&format!("CREATE TABLE {TABLE}(x)"))
            .unwrap();
        insert_rows(&connection, SOURCE, 1..=1, 4);
        let index = index();

        assert_eq!(
            index.rebuild_step(&connection, 2).unwrap(),
            Progress::Complete
        );
        assert_eq!(state(&index), State::Unavailable);
        let attempted = index.session.extension();
        assert!(attempted.is_some());

        assert_eq!(
            index.rebuild_step(&connection, 2).unwrap(),
            Progress::Complete
        );
        assert_eq!(state(&index), State::Unavailable);
        assert_eq!(index.session.extension(), attempted);
        assert!(!index.prepare_search(&connection, 4).unwrap());
        assert_eq!(
            master_sql(&connection, TABLE).as_deref(),
            Some("CREATE TABLE test_vectors(x)")
        );
    }

    #[test]
    fn test_vector_index_creates_with_capacity_from_row_count() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        insert_rows(&connection, SOURCE, 1..=1, 4);
        let small = index();
        rebuild(&small, &connection);
        assert_eq!(
            declared(&connection, TABLE),
            Declaration {
                dimension: 4,
                capacity: 4,
            }
        );

        insert_rows(&connection, "larger_source", 1..=3, 4);
        let larger = VectorIndex::new(
            definition("larger_vectors", "larger_source"),
            Arc::new(Session::new()),
        );
        rebuild(&larger, &connection);
        assert_eq!(
            declared(&connection, "larger_vectors"),
            Declaration {
                dimension: 4,
                capacity: 6,
            }
        );
    }

    #[test]
    fn test_vector_index_rebuild_step_is_bounded() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        insert_rows(&connection, SOURCE, 1..=5, 4);
        let index = index();

        for (expected, indexed) in [
            (Progress::Pending, 2),
            (Progress::Pending, 4),
            (Progress::Complete, 5),
        ] {
            assert_eq!(index.rebuild_step(&connection, 2).unwrap(), expected);
            for rowid in 1..=5 {
                assert_eq!(
                    vector_exists(&connection, TABLE, rowid),
                    rowid <= indexed,
                    "rowid {rowid} after indexing through {indexed}"
                );
            }
        }
        assert!(matches!(state(&index), State::Built { dimension: 4, .. }));
        assert_eq!(
            index.rebuild_step(&connection, 2).unwrap(),
            Progress::Complete
        );
    }

    #[test]
    fn test_vector_index_second_connection_rebuilds() {
        let (_directory, path) = database();
        let first = open(&path);
        if !load(&first) {
            return;
        }
        insert_rows(&first, SOURCE, 1..=5, 4);
        let first_index = index();
        rebuild(&first_index, &first);
        assert!((1..=5).all(|rowid| vector_exists(&first, TABLE, rowid)));

        let second = open(&path);
        load(&second);
        let second_index = index();
        assert_eq!(state(&second_index), State::Unbuilt);
        assert!(!vector_exists(&second, TABLE, 1));

        rebuild(&second_index, &second);
        assert!((1..=5).all(|rowid| vector_exists(&second, TABLE, rowid)));
    }

    #[test]
    fn test_vector_index_foreign_ddl_triggers_rebuild() {
        let (_directory, path) = database();
        let first = open(&path);
        if !load(&first) {
            return;
        }
        insert_rows(&first, SOURCE, 1..=3, 4);
        let index = index();
        assert!(index.prepare_search(&first, 4).unwrap());
        assert!(matches!(state(&index), State::Built { .. }));

        let second = open(&path);
        second.execute_batch("CREATE TABLE unrelated(x)").unwrap();
        assert!(!vector_exists(&first, TABLE, 1));

        assert!(index.prepare_search(&first, 4).unwrap());
        assert!((1..=3).all(|rowid| vector_exists(&first, TABLE, rowid)));
    }

    #[test]
    fn test_vector_index_own_ddl_keeps_sibling_built() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        let session = Arc::new(Session::new());
        let sibling = VectorIndex::new(
            definition("sibling_vectors", "sibling_source"),
            Arc::clone(&session),
        );
        let index = VectorIndex::new(definition(TABLE, SOURCE), session);
        insert_rows(&connection, "sibling_source", 1..=2, 4);
        create_source(&connection, SOURCE);
        assert!(sibling.prepare_search(&connection, 4).unwrap());
        let built = state(&sibling);
        assert!(matches!(built, State::Built { .. }));

        insert_source(&connection, SOURCE, 1, &embedding(1, 4));
        index.upsert(&connection, 1, &embedding(1, 4));
        assert!(vector_exists(&connection, TABLE, 1));
        assert!(sibling.prepare_search(&connection, 4).unwrap());
        assert_eq!(state(&sibling), built);

        index.reset(&connection);
        assert!(master_sql(&connection, TABLE).is_none());
        assert!(sibling.prepare_search(&connection, 4).unwrap());
        assert_eq!(state(&sibling), built);
        assert!((1..=2).all(|rowid| vector_exists(&connection, "sibling_vectors", rowid)));
    }

    #[test]
    fn test_vector_index_recreates_undersized_table() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        create_raw(&connection, TABLE, 4, 2);
        insert_rows(&connection, SOURCE, 1..=10, 4);
        let index = index();

        assert!(index.prepare_search(&connection, 4).unwrap());
        assert_eq!(
            declared(&connection, TABLE),
            Declaration {
                dimension: 4,
                capacity: 20,
            }
        );
        assert!((1..=10).all(|rowid| vector_exists(&connection, TABLE, rowid)));
    }

    #[test]
    fn test_vector_index_upsert_replaces_existing_rowid() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        create_source(&connection, SOURCE);
        let index = index();
        insert_source(&connection, SOURCE, 1, &embedding(1, 4));
        index.upsert(&connection, 1, &embedding(1, 4));
        index.upsert(&connection, 1, &embedding(2, 4));
        assert!(vector_exists(&connection, TABLE, 1));
        assert!(matches!(state(&index), State::Built { .. }));

        connection
            .execute(
                &format!("INSERT INTO {TABLE}(rowid, embedding) VALUES (?1, ?2)"),
                params![2, VectorIndex::blob(&embedding(2, 4))],
            )
            .unwrap();
        insert_source(&connection, SOURCE, 2, &embedding(2, 4));
        index.upsert(&connection, 2, &embedding(2, 4));
        assert!(vector_exists(&connection, TABLE, 2));
        assert!(matches!(state(&index), State::Built { .. }));
    }

    #[test]
    fn test_vector_index_overflow_marks_stale_and_recreates() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        create_source(&connection, SOURCE);
        let index = index();
        for rowid in 1..=5 {
            insert_source(&connection, SOURCE, rowid, &embedding(rowid, 4));
            index.upsert(&connection, rowid, &embedding(rowid, 4));
        }
        assert_eq!(declared(&connection, TABLE).capacity, 4);
        assert_eq!(state(&index), State::Stale { dimension: 4 });

        assert!(index.prepare_search(&connection, 4).unwrap());
        assert!(declared(&connection, TABLE).capacity >= 10);
        assert!((1..=5).all(|rowid| vector_exists(&connection, TABLE, rowid)));
    }

    #[test]
    fn test_vector_index_remove() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        insert_rows(&connection, SOURCE, 1..=3, 4);

        let unbuilt = index();
        unbuilt.remove(&connection, &[1]);
        assert_eq!(state(&unbuilt), State::Unbuilt);
        assert!(master_sql(&connection, TABLE).is_none());

        let index = index();
        assert!(index.prepare_search(&connection, 4).unwrap());
        index.remove(&connection, &[1, 99]);
        assert!(!vector_exists(&connection, TABLE, 1));
        assert!(vector_exists(&connection, TABLE, 2));
        assert!(vector_exists(&connection, TABLE, 3));
        assert!(matches!(state(&index), State::Built { .. }));
    }

    #[test]
    fn test_vector_index_writer_switches_dimension() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        insert_rows(&connection, SOURCE, 1..=2, 4);
        let index = index();
        assert!(index.prepare_search(&connection, 4).unwrap());

        let before = master_sql(&connection, TABLE);
        assert!(!index.prepare_search(&connection, 8).unwrap());
        assert_eq!(master_sql(&connection, TABLE), before);
        assert!(matches!(state(&index), State::Built { dimension: 4, .. }));

        insert_source(&connection, SOURCE, 3, &embedding(3, 8));
        index.upsert(&connection, 3, &embedding(3, 8));
        assert_eq!(declared(&connection, TABLE).dimension, 8);
        assert!(matches!(state(&index), State::Built { dimension: 8, .. }));
        assert!(vector_exists(&connection, TABLE, 3));
        assert!(!vector_exists(&connection, TABLE, 1));
    }

    #[test]
    fn test_vector_index_reader_never_switches_unbuilt_dimension() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        create_raw(&connection, TABLE, 4, 10);
        insert_rows(&connection, SOURCE, 1..=1, 8);
        let index = index();
        let before = master_sql(&connection, TABLE);

        assert!(!index.prepare_search(&connection, 8).unwrap());
        assert_eq!(master_sql(&connection, TABLE), before);
        assert_eq!(state(&index), State::Unbuilt);
    }

    #[test]
    fn test_vector_index_reader_without_rows_runs_no_ddl() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        create_source(&connection, SOURCE);
        let index = index();

        assert!(!index.prepare_search(&connection, 4).unwrap());
        assert!(master_sql(&connection, TABLE).is_none());
    }

    #[test]
    fn test_vector_index_excludes_wrong_dimension_rows() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        create_source(&connection, SOURCE);
        insert_source(&connection, SOURCE, 1, &embedding(1, 8));
        insert_source(&connection, SOURCE, 2, &embedding(2, 8));
        connection
            .execute(
                &format!("INSERT INTO {SOURCE}(id, embedding) VALUES (3, NULL)"),
                [],
            )
            .unwrap();
        for rowid in 4..=6 {
            insert_source(&connection, SOURCE, rowid, &embedding(rowid, 4));
        }
        let index = index();

        rebuild(&index, &connection);
        assert_eq!(
            declared(&connection, TABLE),
            Declaration {
                dimension: 4,
                capacity: 6,
            }
        );
        assert!((1..=3).all(|rowid| !vector_exists(&connection, TABLE, rowid)));
        assert!((4..=6).all(|rowid| vector_exists(&connection, TABLE, rowid)));
    }

    #[test]
    fn test_vector_index_rebuild_skips_partial_component_length() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        create_source(&connection, SOURCE);
        insert_source(&connection, SOURCE, 1, &embedding(1, 4));
        connection
            .execute(
                &format!("INSERT INTO {SOURCE}(id, embedding) VALUES (2, ?1)"),
                params![vec![0u8; 6]],
            )
            .unwrap();
        let index = index();

        assert_eq!(
            index.rebuild_step(&connection, 2).unwrap(),
            Progress::Complete
        );
        assert_eq!(state(&index), State::Unbuilt);
        assert!(master_sql(&connection, TABLE).is_none());
    }

    #[test]
    fn test_vector_index_reset_drops_built_table() {
        let connection = Connection::open_in_memory().unwrap();
        if !load(&connection) {
            return;
        }
        insert_rows(&connection, SOURCE, 1..=2, 4);
        let index = index();
        assert!(index.prepare_search(&connection, 4).unwrap());

        index.reset(&connection);
        assert_eq!(state(&index), State::Unbuilt);
        assert!(master_sql(&connection, TABLE).is_none());
    }

    #[test]
    fn test_vector_index_reset_while_unbuilt_leaves_table() {
        let (_directory, path) = database();
        let loader = open(&path);
        if !load(&loader) {
            return;
        }
        create_raw(&loader, TABLE, 4, 10);
        drop(loader);

        let connection = open(&path);
        let index = index();
        index.reset(&connection);
        assert_eq!(state(&index), State::Unbuilt);
        assert!(index.session.extension().is_none());
        assert!(master_sql(&connection, TABLE).is_some());
    }

    #[test]
    fn test_vector_index_failed_ddl_is_retried() {
        let (_directory, path) = database();
        let connection = open(&path);
        if !load(&connection) {
            return;
        }
        insert_rows(&connection, SOURCE, 1..=2, 4);
        connection.busy_timeout(std::time::Duration::ZERO).unwrap();
        let blocker = open(&path);
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let index = index();

        assert!(!index.prepare_search(&connection, 4).unwrap());
        assert_eq!(state(&index), State::Unbuilt);

        blocker.execute_batch("COMMIT").unwrap();
        assert!(index.prepare_search(&connection, 4).unwrap());
        assert!((1..=2).all(|rowid| vector_exists(&connection, TABLE, rowid)));
    }
}
