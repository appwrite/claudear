-- V19: When each scheduled job (the PR digest, the repetitive-issues digest)
-- last ran, so a restart neither repeats a run nor skips a slot it missed.
CREATE TABLE IF NOT EXISTS scheduled_runs (
    name TEXT PRIMARY KEY,
    ran_at TEXT NOT NULL
);
