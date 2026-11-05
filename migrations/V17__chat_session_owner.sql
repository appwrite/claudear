-- V17: The dashboard user a chat session belongs to. Sessions created before
-- this column existed keep a NULL owner; only admins can see those. A user's
-- chat history is removed with the user, like their sessions and API tokens.
ALTER TABLE chat_sessions ADD COLUMN owner_user_id INTEGER REFERENCES users(id) ON DELETE CASCADE;
CREATE INDEX IF NOT EXISTS idx_chat_sessions_owner ON chat_sessions(owner_user_id, updated_at);
