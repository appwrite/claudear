use super::SqliteTracker;
use crate::vector_index::signal::Signal;
use crate::vector_index::REBUILD_INTERVAL;
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

const THREAD_NAME: &str = "vector-index-rebuilder";

/// Keeps a tracker's vector indexes rebuilt on a dedicated thread: a pass runs whenever a write,
/// search or reset leaves an index needing a rebuild, and at least every [`REBUILD_INTERVAL`].
///
/// Dropping or stopping it halts background rebuilding for that tracker for good, returning once
/// the batch in flight finishes; searches still advance rebuilds afterwards. The thread holds the
/// tracker only during a pass, so it also ends once every other handle on the tracker is dropped.
#[must_use = "dropping the rebuilder stops it"]
pub struct VectorIndexRebuilder {
    signal: Arc<Signal>,
    thread: Option<JoinHandle<()>>,
}

impl SqliteTracker {
    pub fn spawn_vector_index_rebuilder(self: &Arc<Self>) -> std::io::Result<VectorIndexRebuilder> {
        VectorIndexRebuilder::spawn(self, REBUILD_INTERVAL)
    }
}

impl VectorIndexRebuilder {
    fn spawn(tracker: &Arc<SqliteTracker>, interval: Duration) -> std::io::Result<Self> {
        let signal = tracker.vector_indexes.signal();
        let waiting = Arc::clone(&signal);
        let tracker = Arc::downgrade(tracker);
        let thread = std::thread::Builder::new()
            .name(THREAD_NAME.to_string())
            .spawn(move || Self::run(&tracker, &waiting, interval))?;
        Ok(Self {
            signal,
            thread: Some(thread),
        })
    }

    pub fn stop(self) {
        drop(self);
    }

    fn run(tracker: &Weak<SqliteTracker>, signal: &Signal, interval: Duration) {
        while !signal.is_stopped() {
            let Some(tracker) = tracker.upgrade() else {
                return;
            };
            tracker.rebuild_vector_indexes();
            drop(tracker);
            signal.wait(interval);
        }
    }
}

impl Drop for VectorIndexRebuilder {
    fn drop(&mut self) {
        self.signal.stop();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::error!("The vector index rebuilder panicked; vector indexes now rebuild only as searches advance them");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector_index::REBUILD_BATCH_SIZE;
    use crate::vectorlite::require_vectorlite_for_tests;
    use claudear_core::types::{CodeChunk, Language};
    use rusqlite::{params, Connection, OptionalExtension};
    use std::sync::mpsc;
    use std::time::Instant;
    use tempfile::TempDir;

    const MODEL: &str = "test-model";
    const CODE_CHUNK_VECTORS: &str = "code_chunk_vectors";
    const SHORT_INTERVAL: Duration = Duration::from_millis(50);
    const TIMEOUT: Duration = Duration::from_secs(10);
    const POLL: Duration = Duration::from_millis(20);
    const SETTLE: Duration = Duration::from_millis(200);
    const CHUNKS: usize = 3 * REBUILD_BATCH_SIZE + 1;

    fn code_chunk(repository_id: i64, index: usize) -> CodeChunk {
        CodeChunk {
            id: None,
            repo_id: repository_id,
            file_path: format!("src/file_{index}.rs"),
            chunk_type: "function".to_string(),
            symbol_name: Some(format!("function_{index}")),
            language: Language::Rust,
            start_line: index + 1,
            end_line: index + 2,
            chunk_text: format!("fn function_{index}() {{}}"),
            context_text: String::new(),
            file_hash: format!("hash-{index}"),
            content_hash: None,
        }
    }

    fn embedding(index: usize) -> Vec<f32> {
        vec![1.0, index as f32, 0.5, 0.25]
    }

    fn save(tracker: &SqliteTracker, indexes: std::ops::Range<usize>) {
        let repository_id = tracker.get_or_create_repo_id("org/repo").unwrap();
        let chunks: Vec<CodeChunk> = indexes
            .clone()
            .map(|index| code_chunk(repository_id, index))
            .collect();
        let chunk_ids = tracker.save_code_chunks(&chunks).unwrap();
        let embeddings: Vec<Vec<f32>> = indexes.map(embedding).collect();
        let pairs: Vec<(i64, &[f32])> = chunk_ids
            .iter()
            .copied()
            .zip(embeddings.iter().map(Vec::as_slice))
            .collect();
        tracker.save_code_chunk_embeddings(&pairs, MODEL).unwrap();
    }

    fn built_tracker(directory: &TempDir) -> Option<Arc<SqliteTracker>> {
        let tracker = Arc::new(SqliteTracker::new(directory.path().join("claudear.db")).unwrap());
        if !require_vectorlite_for_tests(&tracker.acquire_lock().unwrap()) {
            return None;
        }
        save(&tracker, 0..CHUNKS);
        tracker.rebuild_vector_indexes();
        assert!(
            missing(&tracker).is_empty(),
            "the single pass must build the index before the scenario starts"
        );
        Some(tracker)
    }

    fn foreign(directory: &TempDir) -> Connection {
        let connection = Connection::open(directory.path().join("claudear.db")).unwrap();
        assert!(require_vectorlite_for_tests(&connection));
        connection
    }

    fn missing(tracker: &SqliteTracker) -> Vec<i64> {
        let connection = tracker.acquire_lock().unwrap();
        let embedding_ids: Vec<i64> = connection
            .prepare("SELECT id FROM code_chunk_embeddings ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let present = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                params![CODE_CHUNK_VECTORS],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            == 1;
        if !present {
            return embedding_ids;
        }
        let mut lookup = connection
            .prepare(&format!(
                "SELECT rowid FROM {CODE_CHUNK_VECTORS} WHERE rowid = ?1"
            ))
            .unwrap();
        embedding_ids
            .into_iter()
            .filter(|id| {
                lookup
                    .query_row(params![id], |row| row.get::<_, i64>(0))
                    .optional()
                    .unwrap()
                    .is_none()
            })
            .collect()
    }

    fn await_restored(tracker: &Arc<SqliteTracker>, scenario: &str) {
        let (sender, receiver) = mpsc::channel();
        let watched = Arc::downgrade(tracker);
        std::thread::spawn(move || loop {
            let Some(tracker) = watched.upgrade() else {
                return;
            };
            let missing = missing(&tracker);
            drop(tracker);
            if sender.send(missing.clone()).is_err() || missing.is_empty() {
                return;
            }
            std::thread::sleep(POLL);
        });
        let deadline = Instant::now() + TIMEOUT;
        let mut last = Vec::new();
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match receiver.recv_timeout(remaining) {
                Ok(missing) if missing.is_empty() => return,
                Ok(missing) => last = missing,
                Err(_) => break,
            }
        }
        panic!(
            "{scenario}: the rebuilder left {} embeddings out of {CODE_CHUNK_VECTORS} after {TIMEOUT:?}, starting with {:?}",
            last.len(),
            last.iter().take(5).collect::<Vec<_>>()
        );
    }

    fn await_finished(rebuilder: &VectorIndexRebuilder) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        let thread = rebuilder.thread.as_ref().unwrap();
        while Instant::now() < deadline {
            if thread.is_finished() {
                return true;
            }
            std::thread::sleep(POLL);
        }
        false
    }

    #[test]
    fn test_rebuilder_restores_an_index_a_write_recreated_empty_without_any_search() {
        let directory = TempDir::new().unwrap();
        let Some(tracker) = built_tracker(&directory) else {
            return;
        };
        let rebuilder = tracker.spawn_vector_index_rebuilder().unwrap();
        std::thread::sleep(SETTLE);

        foreign(&directory)
            .execute_batch(&format!("DROP TABLE {CODE_CHUNK_VECTORS}"))
            .unwrap();
        save(&tracker, CHUNKS..CHUNKS + 1);

        await_restored(&tracker, "after a write recreated the index empty");
        rebuilder.stop();
    }

    #[test]
    fn test_rebuilder_restores_an_index_wiped_by_foreign_ddl_on_its_interval() {
        let directory = TempDir::new().unwrap();
        let Some(tracker) = built_tracker(&directory) else {
            return;
        };
        let rebuilder = VectorIndexRebuilder::spawn(&tracker, SHORT_INTERVAL).unwrap();

        let connection = foreign(&directory);
        let declaration: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = ?1",
                params![CODE_CHUNK_VECTORS],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute_batch(&format!("DROP TABLE {CODE_CHUNK_VECTORS}; {declaration};"))
            .unwrap();
        drop(connection);

        await_restored(&tracker, "after foreign DDL emptied the index");
        rebuilder.stop();
    }

    #[test]
    fn test_rebuilder_stops_promptly_while_waiting() {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let rebuilder = tracker.spawn_vector_index_rebuilder().unwrap();
        std::thread::sleep(POLL);

        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            rebuilder.stop();
            let _ = sender.send(());
        });

        receiver
            .recv_timeout(TIMEOUT)
            .expect("stopping must wake the rebuilder instead of waiting out its interval");
        assert!(tracker.vector_indexes.is_stopped());
    }

    #[test]
    fn test_rebuilder_stop_ends_a_pass_before_its_next_batch() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("claudear.db");
        let writer = SqliteTracker::new(&path).unwrap();
        if !require_vectorlite_for_tests(&writer.acquire_lock().unwrap()) {
            return;
        }
        save(&writer, 0..CHUNKS);
        writer
            .acquire_lock()
            .unwrap()
            .execute_batch(&format!("DROP TABLE {CODE_CHUNK_VECTORS}"))
            .unwrap();
        drop(writer);

        let tracker = Arc::new(SqliteTracker::new(&path).unwrap());
        assert!(require_vectorlite_for_tests(
            &tracker.acquire_lock().unwrap()
        ));
        let connection = tracker.acquire_lock().unwrap();
        let rebuilder = tracker.spawn_vector_index_rebuilder().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            rebuilder.stop();
            let _ = sender.send(());
        });
        while !tracker.vector_indexes.is_stopped() {
            std::thread::sleep(POLL);
        }
        drop(connection);

        receiver
            .recv_timeout(TIMEOUT)
            .expect("stopping must return once the batch in flight finishes");
        assert_eq!(
            missing(&tracker).len(),
            CHUNKS,
            "a stopped rebuilder must not start another batch"
        );
    }

    #[test]
    fn test_rebuilder_exits_once_the_tracker_is_dropped() {
        let tracker = Arc::new(SqliteTracker::in_memory().unwrap());
        let rebuilder = tracker.spawn_vector_index_rebuilder().unwrap();
        std::thread::sleep(POLL);

        drop(tracker);

        assert!(
            await_finished(&rebuilder),
            "the rebuilder must exit once the tracker is dropped instead of waiting out its interval"
        );
    }
}
