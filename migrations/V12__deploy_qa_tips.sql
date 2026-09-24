-- Live deploy-QA tips (distinct from regression_watches).
-- One row per (track, tag). Last-seen is the newest row per track.
-- issue_id is `track:repo:tag` (track names never contain ':'), so it is unique per (track, tag).

CREATE TABLE IF NOT EXISTS deploy_qa_tips (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    track TEXT NOT NULL,
    repo TEXT NOT NULL,
    tag TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    published_at TEXT,
    html_url TEXT,
    author_login TEXT,
    status TEXT NOT NULL DEFAULT 'pending',
    attempt_id INTEGER,
    discord_message_id TEXT,
    discord_thread_id TEXT,
    release_body TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (track, tag)
);

CREATE INDEX IF NOT EXISTS idx_deploy_qa_tips_track_status
    ON deploy_qa_tips (track, status);

CREATE UNIQUE INDEX IF NOT EXISTS idx_deploy_qa_tips_issue_id
    ON deploy_qa_tips (issue_id);

CREATE INDEX IF NOT EXISTS idx_deploy_qa_tips_track_created
    ON deploy_qa_tips (track, created_at DESC);
