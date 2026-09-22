-- Admin-supplied quality ratings for support replies (QA channels + HelpScout).
-- One rating per reply, keyed by the originating action_runs row (action_kind='reply').
-- Ratings are surfaced on the /analytics dashboard; they do not feed the fix pipeline.
CREATE TABLE IF NOT EXISTS support_reply_ratings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    action_run_id INTEGER NOT NULL UNIQUE REFERENCES action_runs(id),
    -- 1..5 stars
    rating INTEGER NOT NULL,
    note TEXT,
    -- email of the admin who rated
    rated_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_reply_ratings_run ON support_reply_ratings(action_run_id);
