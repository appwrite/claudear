CREATE TABLE IF NOT EXISTS discord_message_chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id TEXT,
    channel_id TEXT NOT NULL,
    channel_kind INTEGER NOT NULL DEFAULT 0, -- mirrors Discord channel_type: 0 = channel, 4 = category, 11 = thread

    start_message_id TEXT NOT NULL,
    end_message_id TEXT NOT NULL,
    participant_ids TEXT,

    start_message_time TEXT NOT NULL,
    end_message_time TEXT NOT NULL,

    chunk_text TEXT NOT NULL,
    context_text TEXT NOT NULL,
    content_hash TEXT
);
CREATE INDEX IF NOT EXISTS idx_discord_message_chunks_channel ON discord_message_chunks(channel_id);
CREATE INDEX IF NOT EXISTS idx_discord_message_chunks_content_hash ON discord_message_chunks(content_hash);

CREATE TABLE IF NOT EXISTS discord_message_chunk_embeddings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chunk_id INTEGER NOT NULL UNIQUE REFERENCES discord_message_chunks(id) ON DELETE CASCADE,
    embedding BLOB NOT NULL,
    embedding_model TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS discord_message_chunk_metadata (
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (key)
);

CREATE TABLE IF NOT EXISTS discord_channels (
    channel_id TEXT NOT NULL,
    guild_id TEXT,
    parent_id TEXT,                          -- category id (NULL for categories / uncategorised)
    name TEXT,
    channel_type INTEGER,                    -- raw Discord channel type (0/5 text, 10/11/12 threads, 4 category)
    kind INTEGER NOT NULL DEFAULT 0,         -- classification mirroring Discord channel_type: 0 = channel, 4 = category, 11 = thread
    archived INTEGER NOT NULL DEFAULT 0,     -- 1 when this is an archived thread
    last_indexed_message_id TEXT,            -- forward cursor: newest indexed id (incremental resumes after this)
    last_indexed_at TEXT,
    backfill_complete INTEGER NOT NULL DEFAULT 0, -- 1 once full history (within backfill_days) is scraped
    backfill_cursor TEXT,                    -- backfill progress: oldest indexed id (page *before* this next)
    PRIMARY KEY (channel_id)
);
CREATE INDEX IF NOT EXISTS idx_discord_channels_guild ON discord_channels(guild_id);
CREATE INDEX IF NOT EXISTS idx_discord_channels_parent ON discord_channels(parent_id);
