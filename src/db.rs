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
    worktree_path: Option<String>,
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
            worktree_path: row.try_get("worktree_path")?,
        })
    }
}

/// ISO 8601 UTC timestamp expression for SQLite.
const NOW_UTC: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

async fn current_version(pool: &SqlitePool) -> Result<i32, AppError> {
    let version: i32 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    Ok(version)
}

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), AppError> {
    let version = current_version(pool).await?;

    if version < 1 {
        // v0 -> v1: full schema
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

        sqlx::query("PRAGMA user_version = 1").execute(pool).await?;
        tracing::info!("migration: v0 -> v1 (sessions, access_requests, tool_uses)");
    }

    if version < 2 {
        // v1 -> v2: full audit blob storage + result tracking
        sqlx::query("ALTER TABLE tool_uses ADD COLUMN input_json TEXT NOT NULL DEFAULT ''")
            .execute(pool)
            .await?;
        sqlx::query("ALTER TABLE tool_uses ADD COLUMN is_error INTEGER NOT NULL DEFAULT 0")
            .execute(pool)
            .await?;
        sqlx::query("ALTER TABLE tool_uses ADD COLUMN result_preview TEXT NOT NULL DEFAULT ''")
            .execute(pool)
            .await?;
        sqlx::query("ALTER TABLE tool_uses ADD COLUMN duration_ms INTEGER")
            .execute(pool)
            .await?;

        sqlx::query("PRAGMA user_version = 2").execute(pool).await?;
        tracing::info!("migration: v1 -> v2 (tool_uses audit columns)");
    }

    if version < 3 {
        sqlx::query("ALTER TABLE sessions ADD COLUMN worktree_path TEXT")
            .execute(pool)
            .await?;
        sqlx::query("PRAGMA user_version = 3").execute(pool).await?;
        tracing::info!("migration: v2 -> v3 (worktree_path column)");
    }

    Ok(())
}

pub async fn create_session(
    pool: &SqlitePool,
    thread_id: ThreadId,
    user_id: UserId,
    project: &str,
    worktree_path: Option<&str>,
) -> Result<(), AppError> {
    let id = Uuid::new_v4().to_string();
    let tid = thread_id.get() as i64;
    let uid = user_id.get() as i64;
    sqlx::query(
        "INSERT INTO sessions (id, thread_id, user_id, project, status, worktree_path)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(tid)
    .bind(uid)
    .bind(project)
    .bind(SessionStatus::Active.as_str())
    .bind(worktree_path)
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
                created_at, last_active_at, worktree_path
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
        worktree_path: r.worktree_path.map(|s| Arc::from(s.as_str())),
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
    input_json: &str,
) -> Result<i64, AppError> {
    let tid = thread_id.get() as i64;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO tool_uses (thread_id, tool, input_preview, input_json)
         VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(tid)
    .bind(tool)
    .bind(input_preview)
    .bind(input_json)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn update_tool_result(
    pool: &SqlitePool,
    tool_use_id: i64,
    is_error: bool,
    result_preview: &str,
    duration_ms: Option<i64>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE tool_uses SET is_error = ?, result_preview = ?, duration_ms = ? WHERE id = ?",
    )
    .bind(is_error)
    .bind(result_preview)
    .bind(duration_ms)
    .bind(tool_use_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Row from tool_uses list query.
pub struct ToolUseRow {
    pub id: i64,
    pub tool: String,
    pub input_preview: String,
    pub is_error: bool,
    pub duration_ms: Option<i64>,
    pub created_at: String,
}

impl<'r> FromRow<'r, SqliteRow> for ToolUseRow {
    fn from_row(row: &'r SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            tool: row.try_get("tool")?,
            input_preview: row.try_get("input_preview")?,
            is_error: row.try_get::<i32, _>("is_error").map(|v| v != 0)?,
            duration_ms: row.try_get("duration_ms")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

pub async fn get_tool_uses(
    pool: &SqlitePool,
    thread_id: ThreadId,
    at_id: Option<i64>,
    count: i64,
) -> Result<Vec<ToolUseRow>, AppError> {
    let tid = thread_id.get() as i64;
    let rows: Vec<ToolUseRow> = match at_id {
        Some(id) => {
            sqlx::query_as(
                "SELECT id, tool, input_preview, is_error, duration_ms, created_at
                 FROM tool_uses WHERE thread_id = ? AND id <= ?
                 ORDER BY id DESC LIMIT ?",
            )
            .bind(tid)
            .bind(id)
            .bind(count)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT id, tool, input_preview, is_error, duration_ms, created_at
                 FROM tool_uses WHERE thread_id = ?
                 ORDER BY id DESC LIMIT ?",
            )
            .bind(tid)
            .bind(count)
            .fetch_all(pool)
            .await?
        }
    };
    // Results come DESC, reverse to chronological order
    Ok(rows.into_iter().rev().collect())
}

/// Full detail for a single tool use (includes input_json).
pub struct ToolUseDetail {
    pub id: i64,
    pub tool: String,
    pub input_preview: String,
    pub input_json: String,
    pub is_error: bool,
    pub result_preview: String,
    pub duration_ms: Option<i64>,
    pub created_at: String,
}

impl<'r> FromRow<'r, SqliteRow> for ToolUseDetail {
    fn from_row(row: &'r SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            tool: row.try_get("tool")?,
            input_preview: row.try_get("input_preview")?,
            input_json: row.try_get("input_json")?,
            is_error: row.try_get::<i32, _>("is_error").map(|v| v != 0)?,
            result_preview: row.try_get("result_preview")?,
            duration_ms: row.try_get("duration_ms")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

pub async fn get_tool_use_detail(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<ToolUseDetail>, AppError> {
    let row: Option<ToolUseDetail> = sqlx::query_as(
        "SELECT id, tool, input_preview, input_json, is_error, result_preview,
                duration_ms, created_at
         FROM tool_uses WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
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
    let result = sqlx::query("UPDATE access_requests SET status = 'approved' WHERE user_id = ?")
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
    Ok(rows
        .into_iter()
        .map(|(id, name, ts)| (id as u64, name, ts))
        .collect())
}
