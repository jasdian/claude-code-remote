CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    thread_id INTEGER NOT NULL UNIQUE,
    user_id INTEGER NOT NULL,
    claude_session_id TEXT,
    project TEXT,
    status TEXT NOT NULL DEFAULT 'idle',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_active_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_sessions_thread ON sessions(thread_id);
CREATE INDEX idx_sessions_status ON sessions(status);
