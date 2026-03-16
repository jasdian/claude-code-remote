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

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), AppError> {
    // Check if we need to migrate from v1 schema (project nullable, datetime without tz)
    let needs_migrate: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('sessions') WHERE name = 'project' AND \"notnull\" = 0",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(false);

    if needs_migrate {
        tracing::info!("migrating sessions table: project NOT NULL, timestamps with timezone");
        sqlx::query("ALTER TABLE sessions RENAME TO sessions_old")
            .execute(pool)
            .await?;
        sqlx::query("DROP INDEX IF EXISTS idx_sessions_thread")
            .execute(pool)
            .await?;
        sqlx::query("DROP INDEX IF EXISTS idx_sessions_status")
            .execute(pool)
            .await?;
    }

    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            thread_id INTEGER NOT NULL UNIQUE,
            user_id INTEGER NOT NULL,
            claude_session_id TEXT,
            project TEXT NOT NULL DEFAULT 'default',
            status TEXT NOT NULL DEFAULT 'idle',
            created_at TEXT NOT NULL DEFAULT ({NOW_UTC}),
            last_active_at TEXT NOT NULL DEFAULT ({NOW_UTC})
        )"
    ))
    .execute(pool)
    .await?;

    if needs_migrate {
        sqlx::query(
            "INSERT INTO sessions (id, thread_id, user_id, claude_session_id, project, status, created_at, last_active_at)
             SELECT id, thread_id, user_id, claude_session_id, COALESCE(project, 'default'), status,
                    CASE WHEN created_at LIKE '%T%' THEN created_at ELSE created_at || 'Z' END,
                    CASE WHEN last_active_at LIKE '%T%' THEN last_active_at ELSE last_active_at || 'Z' END
             FROM sessions_old"
        )
        .execute(pool)
        .await?;
        sqlx::query("DROP TABLE sessions_old").execute(pool).await?;
        tracing::info!("migration complete");
    }

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
    project: &str,
) -> Result<(), AppError> {
    let id = Uuid::new_v4().to_string();
    let tid = thread_id.get() as i64;
    let uid = user_id.get() as i64;
    sqlx::query(
        "INSERT INTO sessions (id, thread_id, user_id, project, status)
         VALUES (?, ?, ?, ?, 'active')",
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
        "UPDATE sessions SET claude_session_id = ?, status = 'active',
         last_active_at = {NOW_UTC} WHERE thread_id = ?"
    ))
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
    sqlx::query(&format!(
        "UPDATE sessions SET status = ?, last_active_at = {NOW_UTC}
         WHERE thread_id = ?"
    ))
    .bind(status)
    .bind(tid)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn touch_session(pool: &SqlitePool, thread_id: ThreadId) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query(&format!(
        "UPDATE sessions SET status = 'active', last_active_at = {NOW_UTC} WHERE thread_id = ?"
    ))
    .bind(tid)
    .execute(pool)
    .await?;
    Ok(())
}
