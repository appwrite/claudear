use super::statements::{statements, Statements};

/// Every statement is a literal chosen by variant, so no SQL text is ever assembled from a
/// value reachable through a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Definition {
    Qa,
    Issue,
    Outcome,
    CodeChunk,
    DiscordMessageChunk,
    #[cfg(test)]
    Test,
    #[cfg(test)]
    Larger,
    #[cfg(test)]
    Sibling,
}

const QA: Statements = statements!(
    "qa_question_embeddings",
    "qa_knowledge",
    "question_embedding",
    10_000
);
const ISSUE: Statements = statements!("issue_embedding_vectors", "issues", "embedding", 10_000);
const OUTCOME: Statements = statements!(
    "outcome_embedding_vectors",
    "feedback_outcomes",
    "embedding",
    10_000
);
const CODE_CHUNK: Statements = statements!(
    "code_chunk_vectors",
    "code_chunk_embeddings",
    "embedding",
    100_000
);
const DISCORD_MESSAGE_CHUNK: Statements = statements!(
    "discord_message_chunk_vectors",
    "discord_message_chunk_embeddings",
    "embedding",
    100_000
);
#[cfg(test)]
const TEST: Statements = statements!("test_vectors", "source", "embedding", 4);
#[cfg(test)]
const LARGER: Statements = statements!("larger_vectors", "larger_source", "embedding", 4);
#[cfg(test)]
const SIBLING: Statements = statements!("sibling_vectors", "sibling_source", "embedding", 4);

impl Definition {
    pub(crate) fn statements(self) -> &'static Statements {
        match self {
            Self::Qa => &QA,
            Self::Issue => &ISSUE,
            Self::Outcome => &OUTCOME,
            Self::CodeChunk => &CODE_CHUNK,
            Self::DiscordMessageChunk => &DISCORD_MESSAGE_CHUNK,
            #[cfg(test)]
            Self::Test => &TEST,
            #[cfg(test)]
            Self::Larger => &LARGER,
            #[cfg(test)]
            Self::Sibling => &SIBLING,
        }
    }
}
