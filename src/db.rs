use std::sync::Arc;

use smallvec::SmallVec;
use sqlx::sqlite::SqliteRow;
use sqlx::{FromRow, Row, SqlitePool};
use uuid::Uuid;

use crate::domain::{ClaudeSessionId, Session, SessionStatus, ThreadId, UserId};
use crate::error::AppError;

struct SessionRow {
    id: String,
    thread_id: i64,
    owner_id: i64,
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
            owner_id: row.try_get("owner_id")?,
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

/// Schema version.
const SCHEMA_VERSION: i32 = 3;

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), AppError> {
    let version: i32 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;

    if version < SCHEMA_VERSION {
        sqlx::query(&format!(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                thread_id INTEGER NOT NULL UNIQUE,
                owner_id INTEGER NOT NULL,
                claude_session_id TEXT,
                project TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                created_at TEXT NOT NULL DEFAULT ({NOW_UTC}),
                last_active_at TEXT NOT NULL DEFAULT ({NOW_UTC}),
                worktree_path TEXT
            )"
        ))
        .execute(pool)
        .await?;

        sqlx::query(&format!(
            "CREATE TABLE IF NOT EXISTS session_participants (
                session_thread_id INTEGER NOT NULL,
                user_id INTEGER NOT NULL,
                username TEXT NOT NULL,
                role TEXT NOT NULL DEFAULT 'participant',
                joined_at TEXT NOT NULL DEFAULT ({NOW_UTC}),
                PRIMARY KEY (session_thread_id, user_id),
                FOREIGN KEY (session_thread_id) REFERENCES sessions(thread_id) ON DELETE CASCADE
            )"
        ))
        .execute(pool)
        .await?;

        sqlx::query(&format!(
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                thread_id INTEGER NOT NULL,
                user_id INTEGER NOT NULL,
                username TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT ({NOW_UTC}),
                FOREIGN KEY (thread_id) REFERENCES sessions(thread_id) ON DELETE CASCADE
            )"
        ))
        .execute(pool)
        .await?;

        sqlx::query(&format!(
            "CREATE TABLE IF NOT EXISTS tool_uses (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                thread_id INTEGER NOT NULL,
                user_id INTEGER,
                tool TEXT NOT NULL,
                input_preview TEXT NOT NULL DEFAULT '',
                input_json TEXT NOT NULL DEFAULT '',
                is_error INTEGER NOT NULL DEFAULT 0,
                result_preview TEXT NOT NULL DEFAULT '',
                duration_ms INTEGER,
                created_at TEXT NOT NULL DEFAULT ({NOW_UTC}),
                FOREIGN KEY (thread_id) REFERENCES sessions(thread_id) ON DELETE CASCADE
            )"
        ))
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

        // Indexes
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_thread ON sessions(thread_id)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions(status)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_tool_uses_thread ON tool_uses(thread_id)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_tool_uses_user ON tool_uses(user_id)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(thread_id)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_user ON messages(user_id)")
            .execute(pool)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_participants_thread ON session_participants(session_thread_id)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_participants_user ON session_participants(user_id)",
        )
        .execute(pool)
        .await?;

        sqlx::query("PRAGMA user_version = 1").execute(pool).await?;
        tracing::info!("migration: v{version} -> v1");
    }

    if version < 2 {
        sqlx::query(&format!(
            "CREATE TABLE IF NOT EXISTS session_summaries (
                thread_id INTEGER PRIMARY KEY,
                project TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                files_touched TEXT NOT NULL DEFAULT '[]',
                tools_summary TEXT NOT NULL DEFAULT '',
                work_description TEXT NOT NULL DEFAULT '',
                last_tool_use_id INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT ({NOW_UTC}),
                FOREIGN KEY (thread_id) REFERENCES sessions(thread_id) ON DELETE CASCADE
            )"
        ))
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_summaries_project ON session_summaries(project)",
        )
        .execute(pool)
        .await?;

        sqlx::query("PRAGMA user_version = 2").execute(pool).await?;
        tracing::info!("migration: v{} -> v2", version.max(1));
    }

    if version < 3 {
        sqlx::query(
            "ALTER TABLE session_summaries ADD COLUMN signatures_changed TEXT NOT NULL DEFAULT '[]'",
        )
        .execute(pool)
        .await?;

        sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .execute(pool)
            .await?;
        tracing::info!("migration: v2 -> v{SCHEMA_VERSION}");
    }

    Ok(())
}

/// On startup, mark all "active" sessions as "idle".
/// After a crash/reboot no Claude process is running, but the session ID is
/// still valid for `--resume`.  Setting them to "idle" lets the next message
/// in the thread resume the session normally.
pub async fn reconcile_stale_sessions(pool: &SqlitePool) -> Result<u64, AppError> {
    let result = sqlx::query(&format!(
        "UPDATE sessions SET status = ?, last_active_at = {NOW_UTC} WHERE status = ?"
    ))
    .bind(SessionStatus::Idle.as_str())
    .bind(SessionStatus::Active.as_str())
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

// --- Session CRUD ---

pub async fn create_session(
    pool: &SqlitePool,
    thread_id: ThreadId,
    owner_id: UserId,
    project: &str,
    worktree_path: Option<&str>,
    owner_username: &str,
) -> Result<(), AppError> {
    let id = Uuid::new_v4().to_string();
    let tid = thread_id.get() as i64;
    let uid = owner_id.get() as i64;
    sqlx::query(
        "INSERT INTO sessions (id, thread_id, owner_id, project, status, worktree_path)
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

    // Auto-add owner as participant
    sqlx::query(
        "INSERT OR IGNORE INTO session_participants (session_thread_id, user_id, username, role)
         VALUES (?, ?, ?, 'owner')",
    )
    .bind(tid)
    .bind(uid)
    .bind(owner_username)
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
        "SELECT id, thread_id, owner_id, claude_session_id, project, status,
                created_at, last_active_at, worktree_path
         FROM sessions WHERE thread_id = ? AND status IN (?, ?, ?)",
    )
    .bind(tid)
    .bind(SessionStatus::Active.as_str())
    .bind(SessionStatus::Idle.as_str())
    .bind(SessionStatus::Stopped.as_str())
    .fetch_optional(pool)
    .await?;

    Ok(row.map(session_from_row))
}

/// Find any session for a thread, regardless of status (including stopped/expired).
pub async fn get_any_session_by_thread(
    pool: &SqlitePool,
    thread_id: ThreadId,
) -> Result<Option<Session>, AppError> {
    let tid = thread_id.get() as i64;
    let row: Option<SessionRow> = sqlx::query_as(
        "SELECT id, thread_id, owner_id, claude_session_id, project, status,
                created_at, last_active_at, worktree_path
         FROM sessions WHERE thread_id = ?",
    )
    .bind(tid)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(session_from_row))
}

/// Fetch all active/idle sessions, ordered by creation time (newest first).
pub async fn get_live_sessions(pool: &SqlitePool) -> Result<Vec<Session>, AppError> {
    let rows: Vec<SessionRow> = sqlx::query_as(
        "SELECT id, thread_id, owner_id, claude_session_id, project, status,
                created_at, last_active_at, worktree_path
         FROM sessions WHERE status IN (?, ?)
         ORDER BY created_at DESC",
    )
    .bind(SessionStatus::Active.as_str())
    .bind(SessionStatus::Idle.as_str())
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(session_from_row).collect())
}

/// Delete session row by thread_id (to allow creating a fresh one on the same thread).
pub async fn delete_session_by_thread(
    pool: &SqlitePool,
    thread_id: ThreadId,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query("DELETE FROM sessions WHERE thread_id = ?")
        .bind(tid)
        .execute(pool)
        .await?;
    Ok(())
}

#[inline]
fn session_from_row(r: SessionRow) -> Session {
    Session {
        id: Uuid::parse_str(&r.id).unwrap_or_default(),
        thread_id: ThreadId::new(r.thread_id as u64),
        owner_id: UserId::new(r.owner_id as u64),
        status: SessionStatus::from(r.status.as_str()),
        last_active_at: r.last_active_at.parse().unwrap_or_default(),
        claude_session_id: r.claude_session_id.map(|s| ClaudeSessionId::new(&s)),
        project: Arc::from(r.project.as_str()),
        created_at: r.created_at.parse().unwrap_or_default(),
        worktree_path: r.worktree_path.map(|s| Arc::from(s.as_str())),
    }
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

/// Persist the worktree path for a session (idempotent — skips if already set).
pub async fn set_worktree_path(
    pool: &SqlitePool,
    thread_id: ThreadId,
    worktree_path: &str,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query(
        "UPDATE sessions SET worktree_path = ? WHERE thread_id = ? AND worktree_path IS NULL",
    )
    .bind(worktree_path)
    .bind(tid)
    .execute(pool)
    .await?;
    Ok(())
}

// --- Session participants ---

pub async fn add_participant(
    pool: &SqlitePool,
    thread_id: ThreadId,
    user_id: UserId,
    username: &str,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    let uid = user_id.get() as i64;
    sqlx::query(
        "INSERT OR IGNORE INTO session_participants (session_thread_id, user_id, username, role)
         VALUES (?, ?, ?, 'participant')",
    )
    .bind(tid)
    .bind(uid)
    .bind(username)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn is_participant(
    pool: &SqlitePool,
    thread_id: ThreadId,
    user_id: UserId,
) -> Result<bool, AppError> {
    let tid = thread_id.get() as i64;
    let uid = user_id.get() as i64;
    let found: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM session_participants
         WHERE session_thread_id = ? AND user_id = ?",
    )
    .bind(tid)
    .bind(uid)
    .fetch_one(pool)
    .await?;
    Ok(found)
}

pub struct ParticipantRow {
    pub user_id: u64,
    pub username: String,
    pub role: String,
    pub joined_at: String,
}

pub async fn get_participants(
    pool: &SqlitePool,
    thread_id: ThreadId,
) -> Result<SmallVec<[ParticipantRow; 4]>, AppError> {
    let tid = thread_id.get() as i64;
    let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT user_id, username, role, joined_at
         FROM session_participants WHERE session_thread_id = ?
         ORDER BY joined_at",
    )
    .bind(tid)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(uid, username, role, joined_at)| ParticipantRow {
            user_id: uid as u64,
            username,
            role,
            joined_at,
        })
        .collect())
}

/// Atomically transfer session ownership: update sessions.owner_id,
/// demote old owner to 'participant', promote new owner to 'owner'.
pub async fn transfer_ownership(
    pool: &SqlitePool,
    thread_id: ThreadId,
    old_owner_id: UserId,
    new_owner_id: UserId,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    let old_uid = old_owner_id.get() as i64;
    let new_uid = new_owner_id.get() as i64;

    let mut tx = pool.begin().await?;

    sqlx::query("UPDATE sessions SET owner_id = ? WHERE thread_id = ?")
        .bind(new_uid)
        .bind(tid)
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "UPDATE session_participants SET role = 'participant'
         WHERE session_thread_id = ? AND user_id = ?",
    )
    .bind(tid)
    .bind(old_uid)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE session_participants SET role = 'owner'
         WHERE session_thread_id = ? AND user_id = ?",
    )
    .bind(tid)
    .bind(new_uid)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

pub async fn remove_participant(
    pool: &SqlitePool,
    thread_id: ThreadId,
    user_id: UserId,
) -> Result<bool, AppError> {
    let tid = thread_id.get() as i64;
    let uid = user_id.get() as i64;
    let result = sqlx::query(
        "DELETE FROM session_participants
         WHERE session_thread_id = ? AND user_id = ? AND role != 'owner'",
    )
    .bind(tid)
    .bind(uid)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

// --- Messages ---

pub async fn log_message(
    pool: &SqlitePool,
    thread_id: ThreadId,
    user_id: UserId,
    username: &str,
    content: &str,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    let uid = user_id.get() as i64;
    sqlx::query(
        "INSERT INTO messages (thread_id, user_id, username, content)
         VALUES (?, ?, ?, ?)",
    )
    .bind(tid)
    .bind(uid)
    .bind(username)
    .bind(content)
    .execute(pool)
    .await?;
    Ok(())
}

// --- Tool audit ---

pub async fn log_tool_use(
    pool: &SqlitePool,
    thread_id: ThreadId,
    user_id: Option<UserId>,
    tool: &str,
    input_preview: &str,
    input_json: &str,
) -> Result<i64, AppError> {
    let tid = thread_id.get() as i64;
    let uid = user_id.map(|u| u.get() as i64);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO tool_uses (thread_id, user_id, tool, input_preview, input_json)
         VALUES (?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(tid)
    .bind(uid)
    .bind(tool)
    .bind(input_preview)
    .bind(input_json)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Backfill input_json from assistant event (only overwrites empty values).
pub async fn backfill_tool_input_json(
    pool: &SqlitePool,
    tool_use_id: i64,
    input_json: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE tool_uses SET input_json = ? WHERE id = ? AND (input_json IS NULL OR input_json = '' OR input_json = '{}')",
    )
    .bind(input_json)
    .bind(tool_use_id)
    .execute(pool)
    .await?;
    Ok(())
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
    pub result_preview: String,
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
            result_preview: row.try_get("result_preview")?,
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
    let rows: Vec<ToolUseRow> =
        match at_id {
            Some(id) => sqlx::query_as(
                "SELECT id, tool, input_preview, result_preview, is_error, duration_ms, created_at
                 FROM tool_uses WHERE thread_id = ? AND id <= ?
                 ORDER BY id DESC LIMIT ?",
            )
            .bind(tid)
            .bind(id)
            .bind(count)
            .fetch_all(pool)
            .await?,
            None => sqlx::query_as(
                "SELECT id, tool, input_preview, result_preview, is_error, duration_ms, created_at
                 FROM tool_uses WHERE thread_id = ?
                 ORDER BY id DESC LIMIT ?",
            )
            .bind(tid)
            .bind(count)
            .fetch_all(pool)
            .await?,
        };
    // Results come DESC, reverse to chronological order
    Ok(rows.into_iter().rev().collect())
}

/// Fetch recent tool uses across ALL threads (for use outside a session thread).
pub async fn get_tool_uses_global(
    pool: &SqlitePool,
    count: i64,
) -> Result<Vec<ToolUseRow>, AppError> {
    let rows: Vec<ToolUseRow> = sqlx::query_as(
        "SELECT id, tool, input_preview, result_preview, is_error, duration_ms, created_at
         FROM tool_uses ORDER BY id DESC LIMIT ?",
    )
    .bind(count)
    .fetch_all(pool)
    .await?;
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

pub async fn get_latest_tool_use_id(pool: &SqlitePool) -> Result<Option<i64>, AppError> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM tool_uses ORDER BY id DESC LIMIT 1")
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.0))
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

// --- Session summaries (context sharing) ---

/// Row for context-aware session summaries.
pub struct SessionSummaryRow {
    pub thread_id: i64,
    pub project: String,
    pub status: String,
    pub files_touched: String,
    pub signatures_changed: String,
    pub tools_summary: String,
    pub work_description: String,
    pub last_tool_use_id: i64,
    pub updated_at: String,
}

impl<'r> FromRow<'r, SqliteRow> for SessionSummaryRow {
    fn from_row(row: &'r SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            thread_id: row.try_get("thread_id")?,
            project: row.try_get("project")?,
            status: row.try_get("status")?,
            files_touched: row.try_get("files_touched")?,
            signatures_changed: row.try_get("signatures_changed")?,
            tools_summary: row.try_get("tools_summary")?,
            work_description: row.try_get("work_description")?,
            last_tool_use_id: row.try_get("last_tool_use_id")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// Parameters for upserting a session summary.
pub struct SummaryUpsert<'a> {
    pub thread_id: ThreadId,
    pub project: &'a str,
    pub status: &'a str,
    pub files_touched: &'a str,
    pub signatures_changed: &'a str,
    pub tools_summary: &'a str,
    pub work_description: &'a str,
    pub last_tool_use_id: i64,
}

/// Upsert a session summary (INSERT OR REPLACE).
pub async fn upsert_session_summary(
    pool: &SqlitePool,
    params: &SummaryUpsert<'_>,
) -> Result<(), AppError> {
    let tid = params.thread_id.get() as i64;
    sqlx::query(&format!(
        "INSERT OR REPLACE INTO session_summaries
         (thread_id, project, status, files_touched, signatures_changed, tools_summary, work_description, last_tool_use_id, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, {NOW_UTC})"
    ))
    .bind(tid)
    .bind(params.project)
    .bind(params.status)
    .bind(params.files_touched)
    .bind(params.signatures_changed)
    .bind(params.tools_summary)
    .bind(params.work_description)
    .bind(params.last_tool_use_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch sibling summaries for the same project, excluding the given thread.
/// P2: returns only active/idle siblings.
pub async fn get_sibling_summaries(
    pool: &SqlitePool,
    project: &str,
    exclude_thread_id: ThreadId,
) -> Result<Vec<SessionSummaryRow>, AppError> {
    let exclude_tid = exclude_thread_id.get() as i64;
    let rows: Vec<SessionSummaryRow> = sqlx::query_as(
        "SELECT thread_id, project, status, files_touched, signatures_changed,
                tools_summary, work_description, last_tool_use_id, updated_at
         FROM session_summaries
         WHERE project = ? AND thread_id != ? AND status IN ('active', 'idle')
         ORDER BY updated_at DESC",
    )
    .bind(project)
    .bind(exclude_tid)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Mark a summary's status (e.g. when session stops/expires).
pub async fn mark_summary_status(
    pool: &SqlitePool,
    thread_id: ThreadId,
    status: SessionStatus,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query("UPDATE session_summaries SET status = ? WHERE thread_id = ?")
        .bind(status.as_str())
        .bind(tid)
        .execute(pool)
        .await?;
    Ok(())
}

/// Get the current high-watermark for a session's summary.
pub async fn get_summary_watermark(
    pool: &SqlitePool,
    thread_id: ThreadId,
) -> Result<i64, AppError> {
    let tid = thread_id.get() as i64;
    let wm: Option<(i64,)> =
        sqlx::query_as("SELECT last_tool_use_id FROM session_summaries WHERE thread_id = ?")
            .bind(tid)
            .fetch_optional(pool)
            .await?;
    Ok(wm.map(|r| r.0).unwrap_or(0))
}

/// Minimal tool use row for context extraction (includes input_json).
pub struct ContextToolUseRow {
    pub id: i64,
    pub tool: String,
    pub input_json: String,
}

impl<'r> FromRow<'r, SqliteRow> for ContextToolUseRow {
    fn from_row(row: &'r SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            tool: row.try_get("tool")?,
            input_json: row.try_get("input_json")?,
        })
    }
}

/// Fetch tool uses after a given ID for incremental summarization.
pub async fn get_tool_uses_after(
    pool: &SqlitePool,
    thread_id: ThreadId,
    after_id: i64,
) -> Result<Vec<ContextToolUseRow>, AppError> {
    let tid = thread_id.get() as i64;
    let rows: Vec<ContextToolUseRow> = sqlx::query_as(
        "SELECT id, tool, input_json FROM tool_uses
         WHERE thread_id = ? AND id > ?
         ORDER BY id ASC",
    )
    .bind(tid)
    .bind(after_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Fetch recent messages for a session (for work description).
pub async fn get_recent_messages(
    pool: &SqlitePool,
    thread_id: ThreadId,
    limit: i64,
) -> Result<Vec<(String, String)>, AppError> {
    let tid = thread_id.get() as i64;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT username, content FROM messages
         WHERE thread_id = ?
         ORDER BY id DESC LIMIT ?",
    )
    .bind(tid)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    // Reverse to chronological order
    Ok(rows.into_iter().rev().collect())
}

/// Get all active/idle sessions (for the summarizer background task).
pub async fn get_active_sessions_for_summary(
    pool: &SqlitePool,
) -> Result<Vec<(i64, String, String)>, AppError> {
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT thread_id, project, status FROM sessions
         WHERE status IN ('active', 'idle')
         ORDER BY last_active_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
