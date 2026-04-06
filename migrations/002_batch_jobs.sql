CREATE TABLE IF NOT EXISTS batch_jobs (
    id TEXT PRIMARY KEY,
    api_key_id TEXT NOT NULL REFERENCES api_keys(id),
    status TEXT NOT NULL DEFAULT 'processing',
    total_files INTEGER NOT NULL DEFAULT 0,
    succeeded INTEGER NOT NULL DEFAULT 0,
    failed INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at TEXT
);

CREATE TABLE IF NOT EXISTS batch_results (
    id TEXT PRIMARY KEY,
    batch_job_id TEXT NOT NULL REFERENCES batch_jobs(id) ON DELETE CASCADE,
    file_index INTEGER NOT NULL,
    file_name TEXT,
    status TEXT NOT NULL DEFAULT 'pending',
    result_json TEXT,
    error_message TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_batch_jobs_api_key ON batch_jobs(api_key_id);
CREATE INDEX IF NOT EXISTS idx_batch_jobs_status ON batch_jobs(status);
CREATE INDEX IF NOT EXISTS idx_batch_results_job ON batch_results(batch_job_id);
