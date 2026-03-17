use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use tokio::process::Command;

use crate::config::ClaudeConfig;
use crate::domain::ThreadId;
use crate::error::AppError;

const WORKTREE_DIR: &str = ".claude-worktrees";
const BRANCH_PREFIX: &str = "claude/";

/// Check if a path is inside a git repo. Returns the repo root if so.
/// P4: async IO only.
pub async fn git_repo_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["-C", &cwd.to_string_lossy(), "rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let root = String::from_utf8_lossy(&output.stdout);
    Some(PathBuf::from(root.trim()))
}

/// Create a git worktree for a session. Returns the worktree path.
///
/// Places worktrees at `{repo_root}/.claude-worktrees/{thread_id}`.
/// Creates branch `claude/{thread_id}` from HEAD.
/// If branch already exists, reuses it without `-b`.
/// If worktree dir already exists (stale), removes it first.
pub async fn create_worktree(repo_root: &Path, thread_id: ThreadId) -> Result<PathBuf, AppError> {
    let tid = thread_id.get();
    let worktree_path = repo_root.join(WORKTREE_DIR).join(tid.to_string());
    let branch = format!("{BRANCH_PREFIX}{tid}");

    // If stale worktree dir exists, clean it up first
    if worktree_path.exists() {
        tracing::info!(?worktree_path, "removing stale worktree dir");
        remove_worktree_impl(repo_root, &worktree_path).await;
    }

    // Ensure parent directory exists
    let parent = repo_root.join(WORKTREE_DIR);
    tokio::fs::create_dir_all(&parent)
        .await
        .map_err(|e| AppError::claude(&format!("creating worktree dir: {e}")))?;

    let path_str = worktree_path.to_string_lossy();

    // Try creating with new branch first
    let result = Command::new("git")
        .args(["worktree", "add", "-b", &branch, &path_str])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| AppError::claude(&format!("git worktree add: {e}")))?;

    if result.status.success() {
        tracing::info!(?worktree_path, %branch, "created worktree with new branch");
        return Ok(worktree_path);
    }

    // Branch may already exist (e.g., previous session on same thread) — retry without -b
    let stderr = String::from_utf8_lossy(&result.stderr);
    tracing::debug!(%stderr, "worktree add with -b failed, retrying without -b");

    let result = Command::new("git")
        .args(["worktree", "add", &path_str, &branch])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| AppError::claude(&format!("git worktree add (reuse branch): {e}")))?;

    if result.status.success() {
        tracing::info!(?worktree_path, %branch, "created worktree with existing branch");
        return Ok(worktree_path);
    }

    let stderr = String::from_utf8_lossy(&result.stderr);
    Err(AppError::claude(&format!(
        "git worktree add failed: {stderr}"
    )))
}

/// Remove a worktree by path. Best-effort — logs errors, never fails.
pub async fn remove_worktree(worktree_path: &Path) {
    // Derive repo root: worktree is at {repo_root}/.claude-worktrees/{tid}
    let repo_root = worktree_path.parent().and_then(Path::parent);

    let Some(repo_root) = repo_root else {
        tracing::warn!(?worktree_path, "cannot derive repo root from worktree path");
        return;
    };

    remove_worktree_impl(repo_root, worktree_path).await;

    // Try to delete the branch too
    if let Some(tid_str) = worktree_path.file_name().and_then(|n| n.to_str()) {
        let branch = format!("{BRANCH_PREFIX}{tid_str}");
        let _ = Command::new("git")
            .args(["branch", "-D", &branch])
            .current_dir(repo_root)
            .output()
            .await;
        tracing::debug!(%branch, "deleted worktree branch");
    }
}

/// Internal: remove worktree via git command, then force-remove dir if needed.
async fn remove_worktree_impl(repo_root: &Path, worktree_path: &Path) {
    let path_str = worktree_path.to_string_lossy();

    let result = Command::new("git")
        .args(["worktree", "remove", "--force", &path_str])
        .current_dir(repo_root)
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            tracing::debug!(?worktree_path, "removed worktree via git");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(?worktree_path, %stderr, "git worktree remove failed, force-removing dir");
            let _ = tokio::fs::remove_dir_all(worktree_path).await;
            // Prune stale worktree entries
            let _ = Command::new("git")
                .args(["worktree", "prune"])
                .current_dir(repo_root)
                .output()
                .await;
        }
        Err(e) => {
            tracing::warn!(?worktree_path, error = %e, "git worktree remove command failed");
            let _ = tokio::fs::remove_dir_all(worktree_path).await;
        }
    }
}

/// Resolve the effective cwd for a Claude session.
///
/// If an existing worktree path is provided and still exists on disk, reuses it.
/// If worktrees are enabled and cwd is a git repo, creates a new worktree.
/// Otherwise returns the base cwd with no worktree.
///
/// Returns `(effective_cwd, worktree_path_for_db)`.
pub async fn resolve_session_cwd(
    config: &ClaudeConfig,
    project: Option<&str>,
    thread_id: ThreadId,
    existing_worktree: Option<&str>,
) -> Result<(PathBuf, Option<PathBuf>), AppError> {
    let base_cwd_str = config.resolve_cwd(project).await?;
    let base_cwd = PathBuf::from(base_cwd_str.as_ref());

    // Reuse existing worktree if it still exists
    if let Some(wt) = existing_worktree {
        let wt_path = PathBuf::from(wt);
        if tokio::fs::metadata(&wt_path).await.is_ok() {
            tracing::debug!(?wt_path, "reusing existing worktree");
            return Ok((wt_path.clone(), Some(wt_path)));
        }
        tracing::warn!(
            path = wt,
            "existing worktree path gone, will re-create if enabled"
        );
    }

    // Check if worktrees are enabled
    if !config.resolve_worktrees(project) {
        return Ok((base_cwd, None));
    }

    // Check if cwd is a git repo
    let Some(repo_root) = git_repo_root(&base_cwd).await else {
        tracing::debug!(?base_cwd, "not a git repo, skipping worktree");
        return Ok((base_cwd, None));
    };

    // Create worktree — fall back to base cwd on failure
    match create_worktree(&repo_root, thread_id).await {
        Ok(wt_path) => Ok((wt_path.clone(), Some(wt_path))),
        Err(e) => {
            tracing::warn!(error = %e, "worktree creation failed, using base cwd");
            Ok((base_cwd, None))
        }
    }
}

/// Clean up orphaned worktrees from stopped/expired sessions on startup.
pub async fn cleanup_orphaned(pool: &SqlitePool) {
    let rows: Vec<(i64, String)> = match sqlx::query_as(
        "SELECT thread_id, worktree_path FROM sessions
         WHERE worktree_path IS NOT NULL AND status IN ('stopped', 'expired')",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "failed to query orphaned worktrees");
            return;
        }
    };

    if rows.is_empty() {
        return;
    }

    tracing::info!(count = rows.len(), "cleaning up orphaned worktrees");

    for (tid, path) in &rows {
        remove_worktree(Path::new(path)).await;
        let _ = sqlx::query("UPDATE sessions SET worktree_path = NULL WHERE thread_id = ?")
            .bind(tid)
            .execute(pool)
            .await;
    }
}
