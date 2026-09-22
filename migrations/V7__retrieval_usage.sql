-- Per-attempt record of every chunk retrieved from each RAG source, so that
-- retrieval quality (which chunks were pulled, their scores/ranks, whether they
-- were injected into the prompt, and a relevance quality score) can be
-- assessed. Generalizes the existing `qa_usage` table to
-- all four retrieval sources (code chunks, similar issues, Discord, Q&A).
CREATE TABLE IF NOT EXISTS retrieval_usage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id INTEGER NOT NULL REFERENCES fix_attempts(id),
    source_kind TEXT NOT NULL,           -- 'code_chunk' | 'similar_issue' | 'discord_chunk' | 'qa'
    chunk_ref TEXT NOT NULL,             -- code_chunks.id / issues.issue_id / discord chunk id / qa_id
    file_path TEXT,                      -- code: file_path; discord: channel_id; issue: url
    rank INTEGER NOT NULL,               -- 0-based retrieval order
    similarity_score REAL NOT NULL,      -- cosine similarity [0,1] as returned
    injected INTEGER NOT NULL DEFAULT 1, -- made it into the final context string
    char_len INTEGER,                    -- rendered length contributed to the prompt
    quality_score REAL,                  -- NULL until relevance judge runs
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(attempt_id, source_kind, chunk_ref)
);
CREATE INDEX IF NOT EXISTS idx_retrieval_usage_attempt ON retrieval_usage(attempt_id);
