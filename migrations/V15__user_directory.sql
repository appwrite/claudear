-- V15: The console user directory.
-- Per-person identity handles owned by the console. A row here replaces the
-- [users.<slug>] config entry with the same person key.
CREATE TABLE IF NOT EXISTS user_identities (
    user_id INTEGER PRIMARY KEY,
    slug TEXT,
    linear_names TEXT NOT NULL DEFAULT '[]',
    github_usernames TEXT NOT NULL DEFAULT '[]',
    sentry_usernames TEXT NOT NULL DEFAULT '[]',
    jira_usernames TEXT NOT NULL DEFAULT '[]',
    gitlab_usernames TEXT NOT NULL DEFAULT '[]',
    discord_id TEXT,
    slack_id TEXT,
    email TEXT,
    push_user_key TEXT,
    sms_number TEXT,
    whatsapp_number TEXT,
    telegram_chat_id TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_user_identities_slug ON user_identities(slug) WHERE slug IS NOT NULL;

-- Invited users keep a non-null (unusable) password hash and are told apart by
-- status, because dropping NOT NULL would need a users table rebuild that
-- cascade-deletes sessions and api_tokens.
ALTER TABLE users ADD COLUMN status TEXT NOT NULL DEFAULT 'active'
    CHECK (status IN ('active', 'invited'));

-- Single-use invite links. Only the SHA-256 hash of the secret is stored, and
-- the partial unique index allows at most one open invite per user.
CREATE TABLE IF NOT EXISTS user_invites (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_by INTEGER,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT NOT NULL,
    accepted_at TEXT,
    revoked_at TEXT,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE SET NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_user_invites_open
    ON user_invites(user_id) WHERE accepted_at IS NULL AND revoked_at IS NULL;
