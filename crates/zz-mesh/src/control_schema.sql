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
