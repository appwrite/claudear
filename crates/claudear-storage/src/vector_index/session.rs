use super::signal::Signal;
use crate::vectorlite::{is_vectorlite_available, try_load_vectorlite};
use claudear_core::error::Result;
use rusqlite::Connection;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

pub(crate) struct Session {
    extension: OnceLock<bool>,
    schema_version: AtomicI64,
    generation: AtomicU64,
    signal: Arc<Signal>,
}

impl Session {
    pub(crate) fn new() -> Self {
        Self {
            extension: OnceLock::new(),
            schema_version: AtomicI64::new(i64::MIN),
            generation: AtomicU64::new(0),
            signal: Arc::default(),
        }
    }

    pub(crate) fn load_extension(&self, connection: &Connection) -> bool {
        *self
            .extension
            .get_or_init(|| Self::attempt_load(connection))
    }

    pub(crate) fn signal(&self) -> &Arc<Signal> {
        &self.signal
    }

    pub(crate) fn generation(&self, connection: &Connection) -> Result<u64> {
        let version = Self::schema_version(connection)?;
        if self.schema_version.swap(version, Ordering::AcqRel) != version {
            return Ok(self.generation.fetch_add(1, Ordering::AcqRel) + 1);
        }
        Ok(self.generation.load(Ordering::Acquire))
    }

    pub(crate) fn acknowledge(&self, connection: &Connection) -> Result<()> {
        let version = Self::schema_version(connection)?;
        self.schema_version.store(version, Ordering::Release);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn extension(&self) -> Option<bool> {
        self.extension.get().copied()
    }

    fn attempt_load(connection: &Connection) -> bool {
        if is_vectorlite_available(connection) {
            return true;
        }
        match try_load_vectorlite(connection) {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::warn!(%error, "Failed to load vectorlite; vector search is disabled on this connection");
                false
            }
        }
    }

    fn schema_version(connection: &Connection) -> Result<i64> {
        Ok(connection.query_row("PRAGMA schema_version", [], |row| row.get(0))?)
    }
}
