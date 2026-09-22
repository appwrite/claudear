-- Observed recurrence signal for issues, captured at processing time.
--
-- Sentry's recurrence fields (event_count, is_escalating) live only in the
-- transient Issue.metadata during polling and are not otherwise persisted.
-- This table records the most-recent observation per issue so the weekly
-- repetitive-issues digest can be built entirely from what the agent has
-- actually seen and tried — no live API calls, nothing unseen/untried.
CREATE TABLE IF NOT EXISTS issue_recurrence (
    source TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    event_count INTEGER NOT NULL DEFAULT 0,
    is_escalating INTEGER NOT NULL DEFAULT 0,
    observed_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (source, issue_id)
);

CREATE INDEX IF NOT EXISTS idx_issue_recurrence_event_count
    ON issue_recurrence(event_count DESC);
