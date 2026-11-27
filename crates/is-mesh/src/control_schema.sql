CREATE TABLE IF NOT EXISTS control_sessions (
    session BLOB PRIMARY KEY, principal BLOB NOT NULL, object BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS control_bindings (
    object BLOB PRIMARY KEY, state BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS control_subscribers (
    subscriber BLOB PRIMARY KEY, object BLOB UNIQUE NOT NULL
);
CREATE TABLE IF NOT EXISTS closed_control_sessions (session BLOB PRIMARY KEY);
-- Secure external admissions read subscriber frames before they can steer.
-- Legacy trusted grants retain their original behavior.
CREATE TABLE IF NOT EXISTS control_sync_pending (object BLOB PRIMARY KEY);
CREATE TABLE IF NOT EXISTS control_attempts (
    connection BLOB PRIMARY KEY, expires_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS control_attempt_expiry ON control_attempts(expires_ms);

-- A staged world may perform its local ECS swap only while its exact durable
-- generation/revision is reserved. A crashed client never pins another group.
CREATE TABLE IF NOT EXISTS group_installations (
    unit BLOB PRIMARY KEY REFERENCES units(id), slot INTEGER NOT NULL, token BLOB NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS worker_installations ON group_installations(slot);
-- A raw ECS spawn reserves an object id only until its first checkpoint or
-- cancellation. Dropping/reaping the exact install lease drops the name claim.
CREATE TABLE IF NOT EXISTS pending_raw_names (
    object BLOB PRIMARY KEY, unit BLOB NOT NULL UNIQUE REFERENCES group_installations(unit) ON DELETE CASCADE
);
