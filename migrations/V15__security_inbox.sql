-- Security inbox: reports, canonical vulnerabilities, their event timeline,
-- the customer reply ledger and prepared rewards.
-- Every *_at column is written by the application in RFC3339 UTC
-- (YYYY-MM-DDTHH:MM:SSZ) and compared as text, so none has a default.
-- security_vulnerabilities and security_reports reference each other; SQLite
-- resolves a foreign key when a row is written, not when a table is created.

CREATE TABLE IF NOT EXISTS security_vulnerabilities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    summary TEXT NOT NULL,
    embedding BLOB,
    repository TEXT,
    severity TEXT,
    state TEXT NOT NULL,
    first_report_id INTEGER NOT NULL REFERENCES security_reports (id),
    first_reported_at TEXT NOT NULL,
    fix_attempt_id INTEGER REFERENCES fix_attempts (id),
    pull_request_url TEXT,
    merge_commit TEXT,
    merged_at TEXT,
    deployed_version TEXT,
    live_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_security_vulnerabilities_state
    ON security_vulnerabilities (state);

CREATE TABLE IF NOT EXISTS security_reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    short_id TEXT NOT NULL,
    mailbox_id TEXT NOT NULL,
    url TEXT NOT NULL,
    subject TEXT NOT NULL,
    reporter_email TEXT,
    reporter_customer_id TEXT,
    reported_at TEXT NOT NULL,
    state TEXT NOT NULL,
    outcome TEXT,
    public_reason TEXT,
    assessment_summary TEXT,
    affected_repository TEXT,
    fixed_by_commit TEXT,
    fixed_on TEXT,
    spam_confidence REAL,
    assessment_attempts INTEGER NOT NULL DEFAULT 0,
    customer_thread_count INTEGER NOT NULL DEFAULT 0,
    vulnerability_id INTEGER REFERENCES security_vulnerabilities (id),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (source, conversation_id)
);

CREATE INDEX IF NOT EXISTS idx_security_reports_state
    ON security_reports (state);

CREATE INDEX IF NOT EXISTS idx_security_reports_vulnerability
    ON security_reports (vulnerability_id);

CREATE INDEX IF NOT EXISTS idx_security_reports_reported_at
    ON security_reports (reported_at);

CREATE TABLE IF NOT EXISTS security_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    report_id INTEGER REFERENCES security_reports (id),
    vulnerability_id INTEGER REFERENCES security_vulnerabilities (id),
    kind TEXT NOT NULL,
    from_state TEXT,
    to_state TEXT,
    actor TEXT,
    detail TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_security_events_report
    ON security_events (report_id);

CREATE INDEX IF NOT EXISTS idx_security_events_vulnerability_kind
    ON security_events (vulnerability_id, kind, created_at);

CREATE TABLE IF NOT EXISTS security_replies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    report_id INTEGER NOT NULL REFERENCES security_reports (id),
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    error TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    claimed_at TEXT NOT NULL,
    completed_at TEXT,
    UNIQUE (report_id, kind)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_security_replies_one_fix_live
    ON security_replies (report_id)
    WHERE kind IN ('fix_live', 'fix_live_rewarded');

CREATE TABLE IF NOT EXISTS security_awards (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    vulnerability_id INTEGER NOT NULL UNIQUE REFERENCES security_vulnerabilities (id),
    report_id INTEGER NOT NULL REFERENCES security_reports (id),
    recipient_email TEXT,
    campaign TEXT NOT NULL,
    credits INTEGER NOT NULL,
    status TEXT NOT NULL,
    actor TEXT,
    note TEXT,
    prepared_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
