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

    // If stale worktree dir exists, clean it up first (P4: async IO only)
    if tokio::fs::metadata(&worktree_path).await.is_ok() {
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
/// If `keep_branch` is true, the git branch is preserved (e.g. for a PR).
pub async fn remove_worktree(worktree_path: &Path, keep_branch: bool) {
    // Derive repo root: worktree is at {repo_root}/.claude-worktrees/{tid}
    let repo_root = worktree_path.parent().and_then(Path::parent);

    let Some(repo_root) = repo_root else {
        tracing::warn!(?worktree_path, "cannot derive repo root from worktree path");
        return;
    };

    remove_worktree_impl(repo_root, worktree_path).await;

    if !keep_branch && let Some(tid_str) = worktree_path.file_name().and_then(|n| n.to_str()) {
        let branch = format!("{BRANCH_PREFIX}{tid_str}");
        let _ = Command::new("git")
            .args(["branch", "-D", &branch])
            .current_dir(repo_root)
            .output()
            .await;
        tracing::debug!(%branch, "deleted worktree branch");
    }

    // Also clean up any orphaned CLI agent worktrees (created by Claude CLI's Agent tool)
    cleanup_cli_worktrees(repo_root).await;
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

/// Detect the default branch (main/master) of a repository.
async fn detect_default_branch(repo_root: &Path) -> Option<String> {
    // Try origin/HEAD first
    let output = Command::new("git")
        .args(["symbolic-ref", "refs/remotes/origin/HEAD", "--short"])
        .current_dir(repo_root)
        .output()
        .await
        .ok()?;

    if output.status.success() {
        let full = String::from_utf8_lossy(&output.stdout);
        // Returns "origin/main" — strip "origin/"
        return full.trim().strip_prefix("origin/").map(String::from);
    }

    // Fallback: check if main or master branch exists locally
    for name in &["main", "master"] {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{name}")])
            .current_dir(repo_root)
            .output()
            .await
            .ok()?;
        if output.status.success() {
            return Some((*name).to_string());
        }
    }

    None
}

/// Try to create a PR from the worktree branch if it has commits ahead of the
/// default branch. Best-effort: returns the PR URL on success, None otherwise.
/// Requires `gh` CLI to be available.
pub async fn try_create_pr(worktree_path: &Path, project: &str) -> Option<String> {
    let repo_root = worktree_path.parent().and_then(Path::parent)?;
    let tid_str = worktree_path.file_name()?.to_str()?;
    let branch = format!("{BRANCH_PREFIX}{tid_str}");

    let default_branch = detect_default_branch(repo_root).await?;

    // Count commits ahead of default branch
    let output = Command::new("git")
        .args(["rev-list", "--count", &format!("{default_branch}..HEAD")])
        .current_dir(worktree_path)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let count: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    if count == 0 {
        tracing::debug!(%branch, "no commits ahead of {default_branch}, skipping PR");
        return None;
    }

    // Gather commit log for PR body
    let log_output = Command::new("git")
        .args(["log", "--oneline", &format!("{default_branch}..HEAD")])
        .current_dir(worktree_path)
        .output()
        .await
        .ok()?;
    let commits = String::from_utf8_lossy(&log_output.stdout);

    // Use first commit subject as PR title
    let first_subject = commits.lines().last().unwrap_or("Claude session changes");
    let title = if count == 1 {
        first_subject
            .split_once(' ')
            .map_or(first_subject, |(_, msg)| msg)
            .to_string()
    } else {
        format!("{project}: {count} changes from session {tid_str}")
    };

    // Push branch to remote
    let push = Command::new("git")
        .args(["push", "-u", "origin", &branch])
        .current_dir(repo_root)
        .output()
        .await
        .ok()?;

    if !push.status.success() {
        let stderr = String::from_utf8_lossy(&push.stderr);
        tracing::warn!(%branch, %stderr, "failed to push branch for auto-PR");
        return None;
    }

    // Create PR via gh CLI
    let body = format!(
        "Auto-created by `/end` from Discord session.\n\n\
         **Commits ({count}):**\n```\n{commits}```"
    );

    let pr = Command::new("gh")
        .args([
            "pr",
            "create",
            "--head",
            &branch,
            "--base",
            &default_branch,
            "--title",
            &title,
            "--body",
            &body,
        ])
        .current_dir(repo_root)
        .output()
        .await
        .ok()?;

    if !pr.status.success() {
        let stderr = String::from_utf8_lossy(&pr.stderr);
        tracing::warn!(%branch, %stderr, "gh pr create failed");
        return None;
    }

    let url = String::from_utf8_lossy(&pr.stdout).trim().to_string();
    tracing::info!(%branch, %url, count, "auto-created PR");
    Some(url)
}

/// Best-effort push of the worktree branch to the remote.
/// Returns `Ok(true)` if pushed, `Ok(false)` if nothing to push, `Err` on failure.
pub async fn try_push_branch(worktree_path: &Path) -> Result<bool, Box<str>> {
    let repo_root = worktree_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| Box::<str>::from("cannot derive repo root"))?;
    let tid_str = worktree_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Box::<str>::from("invalid worktree path"))?;
    let branch = format!("{BRANCH_PREFIX}{tid_str}");

    let default_branch = detect_default_branch(repo_root)
        .await
        .ok_or_else(|| Box::<str>::from("cannot detect default branch"))?;

    // Count commits ahead of default branch
    let output = Command::new("git")
        .args(["rev-list", "--count", &format!("{default_branch}..HEAD")])
        .current_dir(worktree_path)
        .output()
        .await
        .map_err(|e| Box::<str>::from(format!("rev-list failed: {e}")))?;

    let count: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    if count == 0 {
        return Ok(false);
    }

    let push = Command::new("git")
        .args(["push", "-u", "origin", &branch])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| Box::<str>::from(format!("push spawn failed: {e}")))?;

    if !push.status.success() {
        let stderr = String::from_utf8_lossy(&push.stderr);
        return Err(Box::from(format!("git push failed: {stderr}").as_str()));
    }

    tracing::info!(%branch, count, "pushed worktree branch to remote");
    Ok(true)
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

/// Install the `prepare-commit-msg` hook in a worktree for co-author trailers.
/// P4: All IO via tokio.
async fn install_coauthor_hook(worktree_path: &Path) -> Result<(), AppError> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(worktree_path)
        .output()
        .await
        .map_err(|e| AppError::claude(&format!("git rev-parse --git-dir: {e}")))?;

    if !output.status.success() {
        return Err(AppError::claude(
            "could not determine git dir for hook installation",
        ));
    }

    let git_dir_raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let git_dir = if Path::new(&git_dir_raw).is_absolute() {
        PathBuf::from(&git_dir_raw)
    } else {
        worktree_path.join(&git_dir_raw)
    };

    let hooks_dir = git_dir.join("hooks");
    tokio::fs::create_dir_all(&hooks_dir)
        .await
        .map_err(|e| AppError::claude(&format!("creating hooks dir: {e}")))?;

    let hook_path = hooks_dir.join("prepare-commit-msg");
    tokio::fs::write(&hook_path, crate::domain::PREPARE_COMMIT_MSG_HOOK)
        .await
        .map_err(|e| AppError::claude(&format!("writing hook: {e}")))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        tokio::fs::set_permissions(&hook_path, perms)
            .await
            .map_err(|e| AppError::claude(&format!("chmod hook: {e}")))?;
    }

    tracing::debug!(?hook_path, "installed prepare-commit-msg hook");
    Ok(())
}

/// Write (or remove) the `.claude-coauthors` file in a worktree.
/// P4: async IO only.
pub async fn write_coauthors_file(
    worktree_path: &Path,
    content: Option<&str>,
) -> Result<(), AppError> {
    let file_path = worktree_path.join(".claude-coauthors");

    match content {
        Some(text) => {
            tokio::fs::write(&file_path, text)
                .await
                .map_err(|e| AppError::claude(&format!("writing .claude-coauthors: {e}")))?;
            tracing::debug!(?file_path, "wrote coauthors file");
        }
        None => {
            if tokio::fs::metadata(&file_path).await.is_ok() {
                let _ = tokio::fs::remove_file(&file_path).await;
                tracing::debug!(?file_path, "removed coauthors file (solo session)");
            }
        }
    }
    Ok(())
}

/// Exclude `.claude-coauthors` via the worktree's git exclude file.
/// Uses `info/exclude` rather than `.gitignore` to avoid modifying tracked files.
async fn ensure_coauthors_excluded(worktree_path: &Path) {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(worktree_path)
        .output()
        .await;

    let git_dir = match output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if Path::new(&s).is_absolute() {
                PathBuf::from(s)
            } else {
                worktree_path.join(s)
            }
        }
        _ => return,
    };

    let info_dir = git_dir.join("info");
    let exclude_path = info_dir.join("exclude");
    let entry = ".claude-coauthors";

    let existing = tokio::fs::read_to_string(&exclude_path)
        .await
        .unwrap_or_default();
    if existing.lines().any(|line| line.trim() == entry) {
        return;
    }

    let new_content = if existing.is_empty() || existing.ends_with('\n') {
        format!("{existing}{entry}\n")
    } else {
        format!("{existing}\n{entry}\n")
    };

    let _ = tokio::fs::create_dir_all(&info_dir).await;
    if let Err(e) = tokio::fs::write(&exclude_path, new_content).await {
        tracing::warn!(error = %e, "failed to update git exclude for coauthors file");
    }
}

/// Set up co-author hook and initial coauthors file for a worktree.
/// Best-effort: logs warnings but does not fail the session.
pub async fn setup_coauthor_hook(worktree_path: &Path, coauthors_content: Option<&str>) {
    if let Err(e) = install_coauthor_hook(worktree_path).await {
        tracing::warn!(error = %e, "failed to install co-author hook");
    }
    if let Err(e) = write_coauthors_file(worktree_path, coauthors_content).await {
        tracing::warn!(error = %e, "failed to write coauthors file");
    }
    ensure_coauthors_excluded(worktree_path).await;
}

/// Clean up orphaned agent worktrees left behind by Claude CLI when the process
/// is killed. CLI creates worktrees at `{repo_root}/.claude/worktrees/agent-*`
/// with branches `worktree-agent-*`.
///
/// If an agent worktree has commits ahead of its base (work in progress when
/// killed), we preserve the branch and only remove the worktree directory.
/// If it has no unique commits, both the worktree and branch are removed.
pub async fn cleanup_cli_worktrees(repo_root: &Path) {
    let cli_wt_dir = repo_root.join(".claude").join("worktrees");

    // Nothing to clean if directory doesn't exist
    if tokio::fs::metadata(&cli_wt_dir).await.is_err() {
        return;
    }

    // Prune stale worktree entries first
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(repo_root)
        .output()
        .await;

    // List remaining valid worktrees (still active — skip these)
    let valid = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .await;
    let valid_paths: Vec<String> = valid
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.strip_prefix("worktree ").map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Scan .claude/worktrees/ for agent dirs that are no longer valid worktrees
    let mut entries = match tokio::fs::read_dir(&cli_wt_dir).await {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut cleaned = 0u32;
    let mut preserved = 0u32;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("agent-") {
            continue;
        }

        let path_str = path.to_string_lossy().to_string();
        if valid_paths.iter().any(|v| v == &path_str) {
            continue; // still an active worktree, skip
        }

        let branch = format!("worktree-{name_str}");

        // Check if the branch has commits ahead of HEAD (unmerged agent work)
        let has_commits = has_branch_commits(repo_root, &branch).await;

        // Always remove the orphaned directory
        let _ = tokio::fs::remove_dir_all(&path).await;

        if has_commits {
            // Preserve branch — agent was killed mid-work, changes may be valuable
            preserved += 1;
            tracing::warn!(
                %branch,
                "preserved CLI agent branch with unmerged commits (worktree dir removed)"
            );
        } else {
            // No unique commits — safe to delete branch too
            let _ = Command::new("git")
                .args(["branch", "-D", &branch])
                .current_dir(repo_root)
                .output()
                .await;
            cleaned += 1;
            tracing::debug!(%name_str, "cleaned up orphaned CLI agent worktree");
        }
    }

    if cleaned > 0 || preserved > 0 {
        tracing::info!(cleaned, preserved, "CLI agent worktree cleanup complete");
    }
}

/// Check if a branch has commits ahead of HEAD (i.e., contains unmerged work).
async fn has_branch_commits(repo_root: &Path, branch: &str) -> bool {
    let output = Command::new("git")
        .args(["rev-list", "--count", &format!("HEAD..{branch}")])
        .current_dir(repo_root)
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let count: u32 = String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
            count > 0
        }
        _ => false, // branch doesn't exist or error — treat as no commits
    }
}

/// Clean up orphaned worktrees from stopped/expired sessions on startup.
/// Also cleans up CLI agent worktrees that were left behind by killed processes.
pub async fn cleanup_orphaned(pool: &SqlitePool, config: &ClaudeConfig) {
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

    if !rows.is_empty() {
        tracing::info!(count = rows.len(), "cleaning up orphaned worktrees");

        for (tid, path) in &rows {
            remove_worktree(Path::new(path), false).await;
            let _ = sqlx::query("UPDATE sessions SET worktree_path = NULL WHERE thread_id = ?")
                .bind(tid)
                .execute(pool)
                .await;
        }
    }

    // Also clean up CLI agent worktrees even if no bot worktrees are orphaned.
    // Resolve repo root from default cwd (best-effort).
    if let Ok(cwd_str) = config.resolve_cwd(None).await
        && let Some(repo_root) = git_repo_root(Path::new(cwd_str.as_ref())).await
    {
        cleanup_cli_worktrees(&repo_root).await;
    }
}
