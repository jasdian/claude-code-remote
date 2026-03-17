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
    project: String,
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

/// ISO 8601 UTC timestamp expression for SQLite.
const NOW_UTC: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

/// Schema version tracked via SQLite user_version pragma.
const SCHEMA_VERSION: i32 = 2;

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), AppError> {
    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            thread_id INTEGER NOT NULL UNIQUE,
            user_id INTEGER NOT NULL,
            claude_session_id TEXT,
            project TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            created_at TEXT NOT NULL DEFAULT ({NOW_UTC}),
            last_active_at TEXT NOT NULL DEFAULT ({NOW_UTC})
        )"
    ))
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_thread ON sessions(thread_id)")
        .execute(pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions(status)")
        .execute(pool)
        .await?;

    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS access_requests (
            user_id INTEGER PRIMARY KEY,
            username TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            requested_at TEXT NOT NULL DEFAULT ({NOW_UTC})
        )"
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS tool_uses (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            thread_id INTEGER NOT NULL,
            tool TEXT NOT NULL,
            input_preview TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT ({NOW_UTC}),
            FOREIGN KEY (thread_id) REFERENCES sessions(thread_id)
        )"
    ))
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_tool_uses_thread ON tool_uses(thread_id)")
        .execute(pool)
        .await?;

    sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
        .execute(pool)
        .await?;

    Ok(())
}

pub async fn create_session(
    pool: &SqlitePool,
    thread_id: ThreadId,
    user_id: UserId,
    project: &str,
) -> Result<(), AppError> {
    let id = Uuid::new_v4().to_string();
    let tid = thread_id.get() as i64;
    let uid = user_id.get() as i64;
    sqlx::query(
        "INSERT INTO sessions (id, thread_id, user_id, project, status)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(tid)
    .bind(uid)
    .bind(project)
    .bind(SessionStatus::Active.as_str())
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
         FROM sessions WHERE thread_id = ? AND status IN (?, ?)",
    )
    .bind(tid)
    .bind(SessionStatus::Active.as_str())
    .bind(SessionStatus::Idle.as_str())
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| Session {
        id: Uuid::parse_str(&r.id).unwrap_or_default(),
        thread_id: ThreadId::new(r.thread_id as u64),
        user_id: UserId::new(r.user_id as u64),
        status: SessionStatus::from(r.status.as_str()),
        last_active_at: r.last_active_at.parse().unwrap_or_default(),
        claude_session_id: r.claude_session_id.map(|s| ClaudeSessionId::new(&s)),
        project: Arc::from(r.project.as_str()),
        created_at: r.created_at.parse().unwrap_or_default(),
    }))
}

pub async fn update_session_id(
    pool: &SqlitePool,
    thread_id: ThreadId,
    claude_session_id: &str,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query(&format!(
        "UPDATE sessions SET claude_session_id = ?, status = ?,
         last_active_at = {NOW_UTC} WHERE thread_id = ?"
    ))
    .bind(claude_session_id)
    .bind(SessionStatus::Active.as_str())
    .bind(tid)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_session_status(
    pool: &SqlitePool,
    thread_id: ThreadId,
    status: SessionStatus,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query(&format!(
        "UPDATE sessions SET status = ?, last_active_at = {NOW_UTC}
         WHERE thread_id = ?"
    ))
    .bind(status.as_str())
    .bind(tid)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn touch_session(pool: &SqlitePool, thread_id: ThreadId) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query(&format!(
        "UPDATE sessions SET status = ?, last_active_at = {NOW_UTC} WHERE thread_id = ?"
    ))
    .bind(SessionStatus::Active.as_str())
    .bind(tid)
    .execute(pool)
    .await?;
    Ok(())
}

// --- Tool audit ---

pub async fn log_tool_use(
    pool: &SqlitePool,
    thread_id: ThreadId,
    tool: &str,
    input_preview: &str,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query("INSERT INTO tool_uses (thread_id, tool, input_preview) VALUES (?, ?, ?)")
        .bind(tid)
        .bind(tool)
        .bind(input_preview)
        .execute(pool)
        .await?;
    Ok(())
}

// --- Access requests ---

pub async fn create_access_request(
    pool: &SqlitePool,
    user_id: u64,
    username: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT OR REPLACE INTO access_requests (user_id, username, status)
         VALUES (?, ?, 'pending')",
    )
    .bind(user_id as i64)
    .bind(username)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn approve_access(pool: &SqlitePool, user_id: u64) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE access_requests SET status = 'approved' WHERE user_id = ?",
    )
    .bind(user_id as i64)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn revoke_access(pool: &SqlitePool, user_id: u64) -> Result<bool, AppError> {
    let result = sqlx::query("DELETE FROM access_requests WHERE user_id = ?")
        .bind(user_id as i64)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn is_user_approved(pool: &SqlitePool, user_id: u64) -> Result<bool, AppError> {
    let approved: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM access_requests WHERE user_id = ? AND status = 'approved'",
    )
    .bind(user_id as i64)
    .fetch_one(pool)
    .await?;
    Ok(approved)
}

pub async fn get_pending_requests(
    pool: &SqlitePool,
) -> Result<Vec<(u64, String, String)>, AppError> {
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT user_id, username, requested_at FROM access_requests WHERE status = 'pending' ORDER BY requested_at",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id, name, ts)| (id as u64, name, ts)).collect())
}
