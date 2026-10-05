-- V14: When a live run last showed it is still working on its attempt.
-- A run refreshes the heartbeat of its pending attempt every minute, and the
-- orphan sweep releases only pending attempts whose latest heartbeat (or, until
-- the first one, their attempted_at) has gone stale, so a run waiting on
-- approval, a question or its agent is never swept while it is alive.
ALTER TABLE fix_attempts ADD COLUMN heartbeat_at TEXT;
