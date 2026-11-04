-- V16: The developer responsible for each open Claudear PR.
-- A PR keeps one active assignment until it is merged or closed (status
-- 'closed') or its assignee leaves the roster (status 'released'); ended rows
-- stay as history. The partial unique index keeps two processes sharing the
-- database from both assigning the same PR.
CREATE TABLE IF NOT EXISTS pr_assignments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pr_url TEXT NOT NULL,
    repo TEXT NOT NULL,
    pr_number INTEGER NOT NULL,
    assignee TEXT NOT NULL,
    reason TEXT NOT NULL,
    score REAL NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'active',
    assigned_at TEXT NOT NULL DEFAULT (datetime('now')),
    ended_at TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_pr_assignments_active_pr
    ON pr_assignments (pr_url) WHERE status = 'active';

CREATE INDEX IF NOT EXISTS idx_pr_assignments_status_assignee
    ON pr_assignments (status, assignee);
