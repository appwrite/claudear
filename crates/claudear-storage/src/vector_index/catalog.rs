use super::definition::Definition;
use super::session::Session;
use super::signal::Signal;
use super::VectorIndex;
use rusqlite::Connection;
use std::sync::Arc;

pub(crate) struct Catalog {
    session: Arc<Session>,
    pub(crate) qa: VectorIndex,
    pub(crate) issue: VectorIndex,
    pub(crate) outcome: VectorIndex,
    pub(crate) code_chunk: VectorIndex,
    pub(crate) discord_message_chunk: VectorIndex,
}

impl Catalog {
    pub(crate) fn new() -> Self {
        let session = Arc::new(Session::new());
        Self {
            qa: VectorIndex::new(Definition::QA, Arc::clone(&session)),
            issue: VectorIndex::new(Definition::ISSUE, Arc::clone(&session)),
            outcome: VectorIndex::new(Definition::OUTCOME, Arc::clone(&session)),
            code_chunk: VectorIndex::new(Definition::CODE_CHUNK, Arc::clone(&session)),
            discord_message_chunk: VectorIndex::new(
                Definition::DISCORD_MESSAGE_CHUNK,
                Arc::clone(&session),
            ),
            session,
        }
    }

    pub(crate) fn load_extension(&self, connection: &Connection) -> bool {
        self.session.load_extension(connection)
    }

    pub(crate) fn signal(&self) -> Arc<Signal> {
        Arc::clone(self.session.signal())
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.session.signal().is_stopped()
    }

    pub(crate) fn all(&self) -> [&VectorIndex; 5] {
        [
            &self.qa,
            &self.issue,
            &self.outcome,
            &self.code_chunk,
            &self.discord_message_chunk,
        ]
    }

    #[cfg(test)]
    pub(crate) fn session(&self) -> &Arc<Session> {
        &self.session
    }
}

impl Drop for Catalog {
    fn drop(&mut self) {
        self.session.signal().stop();
    }
}
