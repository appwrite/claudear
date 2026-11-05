//! Vectorlite integration for HNSW-based similarity search.
//!
//! Uses the vectorlite SQLite extension for efficient approximate nearest neighbor search.
//! The HNSW tables are a per-connection cache of the embedding tables, created and rebuilt
//! by `crate::vector_index` and queried from `sqlite.rs`.

use claudear_core::error::{Error, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

// vectorlite statically links abseil and exports it as weak symbols, which dyld coalesces
// across images: a second copy of the library loaded from another path parks its threads
// on the first copy's semaphores, where its unlocks never wake them. So every load reuses
// the first path that loaded.
static LOADED_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Load the vectorlite extension into a SQLite connection.
///
/// The first path that loads is kept for the life of the process; later calls load that
/// same library whatever path they pass, so the process never maps a second copy.
///
/// # Safety
/// This enables extension loading which can execute arbitrary code.
/// Only load trusted extensions.
pub fn load_vectorlite_extension(connection: &Connection, extension_path: &Path) -> Result<()> {
    let mut loaded = LOADED_PATH.lock().unwrap_or_else(PoisonError::into_inner);
    let path = loaded.clone().unwrap_or_else(|| {
        std::fs::canonicalize(extension_path).unwrap_or_else(|_| extension_path.to_path_buf())
    });

    // SAFETY: We only load the vectorlite extension from trusted paths
    let result = unsafe {
        connection.load_extension_enable()?;
        connection.load_extension(&path, None::<&str>)
    };
    if result.is_ok() {
        loaded.get_or_insert(path);
    }
    connection.load_extension_disable()?;
    result.map_err(|error| Error::Other(format!("Failed to load vectorlite extension: {error}")))
}

fn loaded_path() -> Option<PathBuf> {
    LOADED_PATH
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Try to load vectorlite from common paths.
pub fn try_load_vectorlite(connection: &Connection) -> Result<bool> {
    if let Some(path) = loaded_path() {
        load_vectorlite_extension(connection, &path)?;
        return Ok(true);
    }

    let common_paths = [
        // Docker/Linux paths
        "/usr/local/lib/vectorlite.so",
        "/usr/lib/vectorlite.so",
        "/app/lib/vectorlite.so",
        // macOS paths
        "/usr/local/lib/vectorlite.dylib",
        "/opt/homebrew/lib/vectorlite.dylib",
        // Local development
        "./vectorlite.so",
        "./vectorlite.dylib",
        // Windows paths
        "./vectorlite.dll",
    ];

    for path in common_paths {
        let path = Path::new(path);
        if path.exists() {
            match load_vectorlite_extension(connection, path) {
                Ok(()) => {
                    tracing::info!("Loaded vectorlite from {:?}", path);
                    return Ok(true);
                }
                Err(error) => {
                    tracing::warn!("Failed to load vectorlite from {:?}: {}", path, error);
                }
            }
        }
    }

    if let Ok(path) = std::env::var("CLAUDEAR_VECTORLITE_PATH") {
        let path = Path::new(&path);
        if path.exists() {
            load_vectorlite_extension(connection, path)?;
            tracing::info!(
                "Loaded vectorlite from CLAUDEAR_VECTORLITE_PATH: {:?}",
                path
            );
            return Ok(true);
        }
    }

    tracing::warn!("Vectorlite extension not found, vector search will be disabled");
    Ok(false)
}

/// Check if vectorlite is available in this connection.
pub fn is_vectorlite_available(connection: &Connection) -> bool {
    connection.execute_batch("SELECT vectorlite_info()").is_ok()
}

/// Load vectorlite from `CLAUDEAR_VECTORLITE_PATH` for a test that needs it.
///
/// Returns `false` (and prints a skip line) when the variable is unset, so the
/// test can return early. Panics when the variable is set but the extension
/// cannot be loaded, so a broken setup never passes as a skip.
#[cfg(test)]
pub(crate) fn require_vectorlite_for_tests(connection: &Connection) -> bool {
    const PATH_VARIABLE: &str = "CLAUDEAR_VECTORLITE_PATH";

    let Some(path) = std::env::var(PATH_VARIABLE)
        .ok()
        .filter(|path| !path.is_empty())
    else {
        eprintln!("skipping: {PATH_VARIABLE} is not set, so vectorlite is unavailable");
        return false;
    };
    if is_vectorlite_available(connection) {
        return true;
    }
    if let Err(error) = load_vectorlite_extension(connection, Path::new(&path)) {
        panic!("{PATH_VARIABLE}={path} is set but vectorlite failed to load: {error}");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    const COPIES: usize = 24;
    const THREADS: usize = 8;

    fn create_tables_concurrently(path: &Path) {
        let barrier = Arc::new(Barrier::new(THREADS));
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let path = path.to_path_buf();
                thread::spawn(move || {
                    let connection = Connection::open_in_memory().unwrap();
                    load_vectorlite_extension(&connection, &path).unwrap();
                    barrier.wait();
                    connection
                        .execute_batch(
                            "CREATE VIRTUAL TABLE vectors USING vectorlite(embedding float32[4] cosine, hnsw(max_elements=16))",
                        )
                        .unwrap();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn test_loading_copies_from_other_paths_never_deadlocks() {
        let connection = Connection::open_in_memory().unwrap();
        if !require_vectorlite_for_tests(&connection) {
            return;
        }
        let original = PathBuf::from(std::env::var("CLAUDEAR_VECTORLITE_PATH").unwrap());
        let name = original.file_name().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let copies: Vec<PathBuf> = (0..COPIES)
            .map(|index| {
                let copy = directory.path().join(index.to_string()).join(name);
                std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
                std::fs::copy(&original, &copy).unwrap();
                copy
            })
            .collect();

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for copy in &copies {
                create_tables_concurrently(copy);
            }
            sender.send(()).unwrap();
        });
        match receiver.recv_timeout(Duration::from_secs(60)) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => {
                panic!("creating vectorlite tables from copies of the library deadlocked")
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("a worker failed to create a vectorlite table from a copy of the library")
            }
        }
    }
}
