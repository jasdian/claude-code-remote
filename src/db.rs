use std::sync::Arc;

use sqlx::sqlite::SqliteRow;
use sqlx::{FromRow, Row, SqlitePool};
use uuid::Uuid;

use crate::domain::{ClaudeSessionId, Session, SessionStatus, ThreadId, UserId};
use crate::error::AppError;

struct SessionRow {
    id: String,
    thread_id: i64,
    user_id: i64,
    claude_session_id: Option<String>,
    project: Option<String>,
    status: String,
    created_at: String,
    last_active_at: String,
}

impl<'r> FromRow<'r, SqliteRow> for SessionRow {
    fn from_row(row: &'r SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            thread_id: row.try_get("thread_id")?,
            user_id: row.try_get("user_id")?,
            claude_session_id: row.try_get("claude_session_id")?,
            project: row.try_get("project")?,
            status: row.try_get("status")?,
            created_at: row.try_get("created_at")?,
            last_active_at: row.try_get("last_active_at")?,
        })
    }
}

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), AppError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            thread_id INTEGER NOT NULL UNIQUE,
            user_id INTEGER NOT NULL,
            claude_session_id TEXT,
            project TEXT,
            status TEXT NOT NULL DEFAULT 'idle',
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            last_active_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_thread ON sessions(thread_id)")
        .execute(pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions(status)")
        .execute(pool)
        .await?;

    Ok(())
}

pub async fn create_session(
    pool: &SqlitePool,
    thread_id: ThreadId,
    user_id: UserId,
    project: Option<&str>,
) -> Result<(), AppError> {
    let id = Uuid::new_v4().to_string();
    let tid = thread_id.get() as i64;
    let uid = user_id.get() as i64;
    sqlx::query(
        "INSERT INTO sessions (id, thread_id, user_id, project, status)
         VALUES (?, ?, ?, ?, 'idle')",
    )
    .bind(&id)
    .bind(tid)
    .bind(uid)
    .bind(project)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_session_by_thread(
    pool: &SqlitePool,
    thread_id: ThreadId,
) -> Result<Option<Session>, AppError> {
    let tid = thread_id.get() as i64;
    let row: Option<SessionRow> = sqlx::query_as(
        "SELECT id, thread_id, user_id, claude_session_id, project, status,
                created_at, last_active_at
         FROM sessions WHERE thread_id = ? AND status IN ('active', 'idle')",
    )
    .bind(tid)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| Session {
        id: Uuid::parse_str(&r.id).unwrap_or_default(),
        thread_id: ThreadId::new(r.thread_id as u64),
        user_id: UserId::new(r.user_id as u64),
        status: SessionStatus::parse(&r.status),
        last_active_at: r.last_active_at.parse().unwrap_or_default(),
        claude_session_id: r.claude_session_id.map(|s| ClaudeSessionId::new(&s)),
        project: r.project.map(|s| Arc::from(s.as_str())),
        created_at: r.created_at.parse().unwrap_or_default(),
    }))
}

pub async fn update_session_id(
    pool: &SqlitePool,
    thread_id: ThreadId,
    claude_session_id: &str,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query(
        "UPDATE sessions SET claude_session_id = ?, status = 'active',
         last_active_at = datetime('now') WHERE thread_id = ?",
    )
    .bind(claude_session_id)
    .bind(tid)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_session_status(
    pool: &SqlitePool,
    thread_id: ThreadId,
    status: &str,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query(
        "UPDATE sessions SET status = ?, last_active_at = datetime('now')
         WHERE thread_id = ?",
    )
    .bind(status)
    .bind(tid)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn touch_session(pool: &SqlitePool, thread_id: ThreadId) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query("UPDATE sessions SET last_active_at = datetime('now') WHERE thread_id = ?")
        .bind(tid)
        .execute(pool)
        .await?;
    Ok(())
}
