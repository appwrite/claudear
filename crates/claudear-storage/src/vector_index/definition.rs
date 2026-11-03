#[derive(Debug, Clone, Copy)]
pub(crate) struct Definition {
    pub(crate) table: &'static str,
    pub(crate) source_table: &'static str,
    pub(crate) id_column: &'static str,
    pub(crate) embedding_column: &'static str,
    pub(crate) minimum_capacity: usize,
}

impl Definition {
    pub(crate) const QA: Self = Self {
        table: "qa_question_embeddings",
        source_table: "qa_knowledge",
        id_column: "id",
        embedding_column: "question_embedding",
        minimum_capacity: 10_000,
    };

    pub(crate) const ISSUE: Self = Self {
        table: "issue_embedding_vectors",
        source_table: "issues",
        id_column: "id",
        embedding_column: "embedding",
        minimum_capacity: 10_000,
    };

    pub(crate) const OUTCOME: Self = Self {
        table: "outcome_embedding_vectors",
        source_table: "feedback_outcomes",
        id_column: "id",
        embedding_column: "embedding",
        minimum_capacity: 10_000,
    };

    pub(crate) const CODE_CHUNK: Self = Self {
        table: "code_chunk_vectors",
        source_table: "code_chunk_embeddings",
        id_column: "id",
        embedding_column: "embedding",
        minimum_capacity: 100_000,
    };

    pub(crate) const DISCORD_MESSAGE_CHUNK: Self = Self {
        table: "discord_message_chunk_vectors",
        source_table: "discord_message_chunk_embeddings",
        id_column: "id",
        embedding_column: "embedding",
        minimum_capacity: 100_000,
    };
}
