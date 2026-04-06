CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    email TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,
    name TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS api_keys (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key_hash TEXT UNIQUE NOT NULL,
    key_prefix TEXT NOT NULL,
    name TEXT NOT NULL DEFAULT 'Default',
    is_active INTEGER NOT NULL DEFAULT 1,
    last_used_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    revoked_at TEXT
);

CREATE TABLE IF NOT EXISTS usage_logs (
    id TEXT PRIMARY KEY,
    api_key_id TEXT NOT NULL REFERENCES api_keys(id),
    endpoint TEXT NOT NULL,
    pages_processed INTEGER NOT NULL DEFAULT 0,
    credits_used INTEGER NOT NULL DEFAULT 0,
    processing_ms INTEGER NOT NULL DEFAULT 0,
    request_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys(key_hash);
CREATE INDEX IF NOT EXISTS idx_api_keys_user ON api_keys(user_id);
CREATE INDEX IF NOT EXISTS idx_usage_logs_key ON usage_logs(api_key_id);
CREATE INDEX IF NOT EXISTS idx_usage_logs_created ON usage_logs(created_at);
