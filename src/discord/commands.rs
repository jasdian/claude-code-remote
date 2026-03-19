use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::Context;
use crate::domain::{SessionStatus, ThreadId, UserId, UserMessage};
use crate::error::AppError;

#[inline]
async fn check_auth(ctx: &Context<'_>) -> Result<(), AppError> {
    let user_id = ctx.author().id.get();
    let state = ctx.data();
    let auth = &state.config.auth;

    if auth.allowed_users.contains(&user_id) || auth.admins.contains(&user_id) {
        return Ok(());
    }

    if let Some(member) = ctx.author_member().await {
        let has_role = member.roles.iter().any(|role: &serenity::RoleId| {
            auth.allowed_roles
                .iter()
                .any(|&allowed| allowed == role.get())
        });
        if has_role {
            return Ok(());
        }
    }

    // Check DB-approved users
    if crate::db::is_user_approved(&state.db, user_id).await? {
        return Ok(());
    }

    Err(AppError::unauthorized("not authorized"))
}

#[poise::command(slash_command)]
pub async fn claude(
    ctx: Context<'_>,
    #[description = "Your prompt for Claude"] prompt: String,
    #[description = "Project name or path"] project: Option<String>,
) -> Result<(), AppError> {
    ctx.defer()
        .await
        .map_err(|e| AppError::claude(&format!("defer: {e}")))?;
    check_auth(&ctx).await?;

    let state = ctx.data();
    let config = &state.config.claude;

    let tools = config.resolve_tools(project.as_deref());

    let is_dm = ctx.guild_id().is_none();
    // Resolve base cwd for project name (before worktree override)
    let base_cwd_str = config.resolve_cwd(project.as_deref()).await?;
    let project_name = crate::project_name_from_cwd(Path::new(base_cwd_str.as_ref()));
    let prompt_excerpt = truncate_prompt(&prompt, 200);
    let start_msg = format!("**{project_name}**\n> {prompt_excerpt}");

    // In DMs: reply directly in channel. In guilds: create a thread.
    let response_channel = if is_dm {
        ctx.say(&start_msg)
            .await
            .map_err(|e| AppError::claude(&format!("say (DM): {e}")))?;
        ctx.channel_id()
    } else {
        let reply = ctx
            .say(&start_msg)
            .await
            .map_err(|e| AppError::claude(&format!("say: {e}")))?;
        let msg = reply
            .message()
            .await
            .map_err(|e| AppError::claude(&format!("get message: {e}")))?;
        let thread = ctx
            .channel_id()
            .create_thread_from_message(
                ctx.http(),
                msg.id,
                serenity::CreateThread::new(thread_name(project_name, &prompt))
                    .auto_archive_duration(serenity::AutoArchiveDuration::OneDay),
            )
            .await
            .map_err(|e| AppError::claude(&format!("create thread: {e}")))?;
        thread.id
    };

    let thread_id = ThreadId::from(response_channel);

    // Resolve cwd with optional worktree isolation
    let (cwd, worktree_path) =
        crate::claude::worktree::resolve_session_cwd(config, project.as_deref(), thread_id, None)
            .await?;

    let (tx, rx) = crate::claude::process::event_channel();
    let (stdin_tx, stdin_rx) = crate::claude::process::stdin_channel();
    let cancel = state.shutdown.child_token();

    let handle = crate::claude::process::run_claude(
        config, &prompt, None, &cwd, &tools, None, tx, cancel, stdin_rx,
    )
    .await?;

    // DB write first — borrows worktree_path; register() moves it after.
    let user_id = UserId::from(ctx.author().id);
    let wt_str = worktree_path.as_ref().and_then(|p| p.to_str());
    crate::db::create_session(
        &state.db,
        thread_id,
        user_id,
        project_name,
        wt_str,
        &ctx.author().name,
    )
    .await?;
    // Track current user for tool attribution
    state.session_manager.set_current_user(
        thread_id,
        user_id,
        Arc::from(ctx.author().name.as_str()),
    );

    state
        .session_manager
        .register(thread_id, handle, stdin_tx, cwd, worktree_path)?;

    let stream_cancel = state.shutdown.child_token();
    tokio::spawn(super::formatter::stream_to_discord(
        Arc::clone(&ctx.serenity_context().http),
        response_channel,
        rx,
        state.clone(),
        stream_cancel,
    ));

    Ok(())
}

#[poise::command(slash_command)]
pub async fn end(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer().await?;
    check_auth(&ctx).await?;
    let thread_id = ThreadId::from(ctx.channel_id());
    let user_id = UserId::from(ctx.author().id);

    // Look up session for worktree cleanup before changing status
    let session = crate::db::get_session_by_thread(&ctx.data().db, thread_id).await?;

    // Only owner or admin can end a session
    if let Some(ref s) = session {
        let is_admin = ctx
            .data()
            .config
            .auth
            .admins
            .contains(&ctx.author().id.get());
        if s.owner_id != user_id && !is_admin {
            ctx.say("Only the session owner or an admin can end it.")
                .await?;
            return Ok(());
        }
    }

    // Resolve worktree path from either active session or DB
    let wt_path: Option<std::path::PathBuf> =
        if let Some((handle, wt_path)) = ctx.data().session_manager.remove(thread_id) {
            handle.kill().await?;
            wt_path
        } else {
            session
                .as_ref()
                .and_then(|s| s.worktree_path.as_deref())
                .map(std::path::PathBuf::from)
        };

    let had_session = session.is_some();

    if had_session {
        // Try auto-PR before cleaning up the worktree
        let project = session.as_ref().map(|s| s.project.as_ref()).unwrap_or("");
        let auto_pr = ctx.data().config.claude.resolve_auto_pr(Some(project));
        let mut pr_url: Option<String> = None;

        if auto_pr && let Some(ref wt) = wt_path {
            pr_url = crate::claude::worktree::try_create_pr(wt, project).await;
        }

        // Remove worktree; keep branch alive if PR was created
        let keep_branch = pr_url.is_some();
        if let Some(ref wt) = wt_path {
            crate::claude::worktree::remove_worktree(wt, keep_branch).await;
        }

        crate::db::update_session_status(&ctx.data().db, thread_id, SessionStatus::Stopped).await?;
        let _ =
            crate::db::mark_summary_status(&ctx.data().db, thread_id, SessionStatus::Stopped).await;

        let msg = match pr_url {
            Some(url) => format!("Session ended. PR created: {url}"),
            None => "Session ended.".to_string(),
        };
        ctx.say(&msg).await?;
        // Archive the thread (not in DMs). Users can still send messages
        // which auto-unarchives the thread, allowing silent session resume.
        if ctx.guild_id().is_some() {
            let channel = ctx.channel_id();
            let _ = channel
                .edit_thread(ctx.http(), serenity::EditThread::new().archived(true))
                .await;
        }
    } else {
        ctx.say("No session here.").await?;
    }
    Ok(())
}

#[poise::command(slash_command)]
pub async fn interrupt(
    ctx: Context<'_>,
    #[description = "New prompt to send after interrupting"] prompt: Option<String>,
) -> Result<(), AppError> {
    ctx.defer().await?;
    check_auth(&ctx).await?;
    let thread_id = ThreadId::from(ctx.channel_id());

    if !ctx.data().session_manager.has_session(thread_id) {
        ctx.say("Nothing to interrupt.").await?;
        return Ok(());
    }

    if let Some(msg) = prompt {
        ctx.data().session_manager.queue_message(
            thread_id,
            UserMessage {
                user_id: UserId::from(ctx.author().id),
                username: Arc::from(ctx.author().name.as_str()),
                content: Arc::from(msg.as_str()),
            },
        );
    }

    ctx.data().session_manager.interrupt(thread_id);
    ctx.say("Interrupted.").await?;
    Ok(())
}

#[poise::command(slash_command)]
pub async fn sessions(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer().await?;
    check_auth(&ctx).await?;
    let count = ctx.data().session_manager.active_count();
    let max = ctx.data().config.claude.max_sessions;
    ctx.say(format!("Active sessions: {count}/{max}")).await?;
    Ok(())
}

#[inline]
fn check_admin(ctx: &Context<'_>) -> Result<(), AppError> {
    let user_id = ctx.author().id.get();
    if ctx.data().config.auth.admins.contains(&user_id) {
        Ok(())
    } else {
        Err(AppError::unauthorized("not an admin"))
    }
}

// --- Access request commands ---

#[poise::command(slash_command)]
pub async fn optin(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer_ephemeral().await?;
    let state = ctx.data();
    let user_id = ctx.author().id.get();

    // Already authorized?
    if state.config.auth.allowed_users.contains(&user_id)
        || state.config.auth.admins.contains(&user_id)
        || crate::db::is_user_approved(&state.db, user_id).await?
    {
        ctx.say("You already have access.").await?;
        return Ok(());
    }

    crate::db::create_access_request(&state.db, user_id, &ctx.author().name).await?;
    ctx.say("Access requested. An admin will review it.")
        .await?;
    Ok(())
}

#[poise::command(slash_command)]
pub async fn optout(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer_ephemeral().await?;
    crate::db::revoke_access(&ctx.data().db, ctx.author().id.get()).await?;
    ctx.say("Access removed.").await?;
    Ok(())
}

#[poise::command(slash_command)]
pub async fn approve(
    ctx: Context<'_>,
    #[description = "User to approve"] user: serenity::User,
) -> Result<(), AppError> {
    ctx.defer_ephemeral().await?;
    check_admin(&ctx)?;
    if crate::db::approve_access(&ctx.data().db, user.id.get()).await? {
        ctx.say(format!("Approved **{}**.", user.name)).await?;
    } else {
        ctx.say(format!("No pending request from **{}**.", user.name))
            .await?;
    }
    Ok(())
}

#[poise::command(slash_command)]
pub async fn revoke(
    ctx: Context<'_>,
    #[description = "User to revoke access from"] user: serenity::User,
) -> Result<(), AppError> {
    ctx.defer_ephemeral().await?;
    check_admin(&ctx)?;
    if crate::db::revoke_access(&ctx.data().db, user.id.get()).await? {
        ctx.say(format!("Revoked access for **{}**.", user.name))
            .await?;
    } else {
        ctx.say(format!("**{}** has no access to revoke.", user.name))
            .await?;
    }
    Ok(())
}

#[poise::command(slash_command)]
pub async fn pending(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer_ephemeral().await?;
    check_admin(&ctx)?;
    let requests = crate::db::get_pending_requests(&ctx.data().db).await?;
    if requests.is_empty() {
        ctx.say("No pending requests.").await?;
    } else {
        let list = requests
            .iter()
            .map(|(id, name, ts)| format!("• **{name}** (<@{id}>) — {ts}"))
            .collect::<Vec<_>>()
            .join("\n");
        ctx.say(format!("**Pending requests:**\n{list}")).await?;
    }
    Ok(())
}

/// Send a Claude CLI internal command (e.g. /compact, /context) to the session.
async fn send_session_command(
    ctx: &Context<'_>,
    cli_cmd: &str,
    status_msg: &str,
    queue_msg: &str,
) -> Result<(), AppError> {
    let state = ctx.data();
    let thread_id = ThreadId::from(ctx.channel_id());

    let session = crate::db::get_session_by_thread(&state.db, thread_id).await?;
    let Some(session) = session else {
        ctx.say("No session here.").await?;
        return Ok(());
    };

    // If busy, queue the command
    if state.session_manager.has_session(thread_id) {
        state.session_manager.queue_message(
            thread_id,
            UserMessage {
                user_id: UserId::from(ctx.author().id),
                username: Arc::from(ctx.author().name.as_str()),
                content: Arc::from(cli_cmd),
            },
        );
        ctx.say(queue_msg).await?;
        return Ok(());
    }

    // Resume with the CLI command
    let config = &state.config.claude;
    let resume_id = session.claude_session_id.as_ref().map(|s| s.as_str());
    let (cwd, worktree_path) = crate::claude::worktree::resolve_session_cwd(
        config,
        Some(&session.project),
        thread_id,
        session.worktree_path.as_deref(),
    )
    .await?;
    let tools = config.resolve_tools(Some(&session.project));

    let (tx, rx) = crate::claude::process::event_channel();
    let (stdin_tx, stdin_rx) = crate::claude::process::stdin_channel();
    let cancel = state.shutdown.child_token();

    let handle = crate::claude::process::run_claude(
        config, cli_cmd, resume_id, &cwd, &tools, None, tx, cancel, stdin_rx,
    )
    .await?;

    // Persist worktree path to DB before register() moves it (P1: borrow first, move last)
    if let Some(ref wt) = worktree_path
        && let Some(wt_str) = wt.to_str()
    {
        let _ = crate::db::set_worktree_path(&state.db, thread_id, wt_str).await;
    }

    state
        .session_manager
        .register(thread_id, handle, stdin_tx, cwd, worktree_path)?;
    crate::db::touch_session(&state.db, thread_id).await?;

    ctx.say(status_msg).await?;

    let stream_cancel = state.shutdown.child_token();
    tokio::spawn(super::formatter::stream_to_discord(
        Arc::clone(&ctx.serenity_context().http),
        ctx.channel_id(),
        rx,
        state.clone(),
        stream_cancel,
    ));

    Ok(())
}

#[poise::command(slash_command)]
pub async fn compact(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer().await?;
    check_auth(&ctx).await?;
    send_session_command(
        &ctx,
        "/compact",
        "_Compacting conversation..._",
        "📨 _Compact queued._",
    )
    .await
}

#[poise::command(slash_command)]
pub async fn context(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer().await?;
    check_auth(&ctx).await?;
    send_session_command(
        &ctx,
        "/context",
        "_Fetching context info..._",
        "📨 _Context queued._",
    )
    .await
}

#[poise::command(slash_command)]
pub async fn audit(
    ctx: Context<'_>,
    #[description = "Tool use ID (omit for latest)"] id: Option<i64>,
    #[description = "Number of entries to show"] count: Option<i64>,
    #[description = "Show full detail for a specific ID"] detail: Option<bool>,
) -> Result<(), AppError> {
    ctx.defer_ephemeral().await?;
    check_admin(&ctx)?;
    let thread_id = crate::domain::ThreadId::from(ctx.channel_id());

    // Detail mode: show full input_json for a specific ID (latest if omitted)
    if detail.unwrap_or(false) {
        let target_id = match id {
            Some(i) => i,
            None => crate::db::get_latest_tool_use_id(&ctx.data().db)
                .await?
                .unwrap_or(0),
        };
        return audit_detail(&ctx, target_id).await;
    }

    let n = count.unwrap_or(10).clamp(1, 50);

    // Auto-detect: if invoked inside a session thread, scope to that thread.
    // Otherwise show global results across all threads.
    let in_thread = crate::db::get_session_by_thread(&ctx.data().db, thread_id)
        .await?
        .is_some();

    let rows = if in_thread {
        crate::db::get_tool_uses(&ctx.data().db, thread_id, id, n).await?
    } else {
        crate::db::get_tool_uses_global(&ctx.data().db, n).await?
    };

    if rows.is_empty() {
        ctx.say("No tool uses found.").await?;
    } else {
        let mut out = String::new();
        for row in &rows {
            let input_str = if row.input_preview.is_empty() {
                String::new()
            } else {
                format!(" — `{}`", row.input_preview)
            };
            let result_str = if row.result_preview.is_empty() {
                String::new()
            } else {
                // Truncate result preview for listing view
                let preview = if row.result_preview.len() > 80 {
                    format!(
                        "{}…",
                        &row.result_preview[..row.result_preview.floor_char_boundary(80)]
                    )
                } else {
                    row.result_preview.clone()
                };
                format!(" → `{preview}`")
            };
            let error_marker = if row.is_error { " ❌" } else { "" };
            let duration_str = row
                .duration_ms
                .map(|ms| format!(" {ms}ms"))
                .unwrap_or_default();
            out.push_str(&format!(
                "`#{}` **{}**{input_str}{result_str}{error_marker}{duration_str} ({})\n",
                row.id, row.tool, row.created_at
            ));
        }
        send_ephemeral_chunked(&ctx, &out).await?;
    }
    Ok(())
}

/// Show full audit detail for a single tool use.
async fn audit_detail(ctx: &Context<'_>, id: i64) -> Result<(), AppError> {
    let detail = crate::db::get_tool_use_detail(&ctx.data().db, id).await?;
    let Some(d) = detail else {
        ctx.say(format!("No tool use found with id `#{id}`"))
            .await?;
        return Ok(());
    };

    let error_marker = if d.is_error { " ❌" } else { "" };
    let duration_str = d
        .duration_ms
        .map(|ms| format!(" ({ms}ms)"))
        .unwrap_or_default();

    let mut out = format!(
        "`#{}` **{}**{error_marker}{duration_str} — {}\n",
        d.id, d.tool, d.created_at
    );

    if !d.input_json.is_empty() {
        out.push_str("\n**Input:**\n```json\n");
        out.push_str(&d.input_json);
        out.push_str("\n```\n");
    } else if !d.input_preview.is_empty() {
        out.push_str("\n**Input:** `");
        out.push_str(&d.input_preview);
        out.push_str("`\n");
    }

    if !d.result_preview.is_empty() {
        out.push_str("\n**Result:**\n```\n");
        out.push_str(&d.result_preview);
        out.push_str("\n```\n");
    }

    send_ephemeral_chunked(ctx, &out).await?;
    Ok(())
}

// --- Multi-user session commands ---

#[poise::command(slash_command)]
pub async fn participants(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer_ephemeral().await?;
    check_auth(&ctx).await?;
    let thread_id = ThreadId::from(ctx.channel_id());

    let session = crate::db::get_session_by_thread(&ctx.data().db, thread_id).await?;
    if session.is_none() {
        ctx.say("No active session in this thread.").await?;
        return Ok(());
    }

    let parts = crate::db::get_participants(&ctx.data().db, thread_id).await?;
    if parts.is_empty() {
        ctx.say("No participants found.").await?;
    } else {
        let list = parts
            .iter()
            .map(|p| {
                let badge = if p.role == "owner" { " (owner)" } else { "" };
                format!("• **{}** (<@{}>){badge}", p.username, p.user_id)
            })
            .collect::<Vec<_>>()
            .join("\n");
        ctx.say(format!("**Participants:**\n{list}")).await?;
    }
    Ok(())
}

/// Remove a participant from the current session.
#[poise::command(slash_command, rename = "sessionkick")]
pub async fn sessionkick(
    ctx: Context<'_>,
    #[description = "User to remove from session"] user: serenity::User,
) -> Result<(), AppError> {
    ctx.defer_ephemeral().await?;
    check_auth(&ctx).await?;
    let thread_id = ThreadId::from(ctx.channel_id());
    let caller_id = UserId::from(ctx.author().id);

    let session = crate::db::get_session_by_thread(&ctx.data().db, thread_id).await?;
    let Some(session) = session else {
        ctx.say("No active session in this thread.").await?;
        return Ok(());
    };

    // Only owner or admin can kick
    let is_admin = ctx
        .data()
        .config
        .auth
        .admins
        .contains(&ctx.author().id.get());
    if session.owner_id != caller_id && !is_admin {
        ctx.say("Only the session owner or an admin can kick participants.")
            .await?;
        return Ok(());
    }

    let target_id = UserId::from(user.id);
    if target_id == session.owner_id {
        ctx.say("Cannot kick the session owner.").await?;
        return Ok(());
    }

    if crate::db::remove_participant(&ctx.data().db, thread_id, target_id).await? {
        ctx.say(format!("Removed **{}** from the session.", user.name))
            .await?;
    } else {
        ctx.say(format!("**{}** is not a participant.", user.name))
            .await?;
    }
    Ok(())
}

/// Ban a user from session and revoke their dynamic access.
#[poise::command(slash_command, rename = "sessionban")]
pub async fn sessionban(
    ctx: Context<'_>,
    #[description = "User to ban from session and revoke access"] user: serenity::User,
) -> Result<(), AppError> {
    ctx.defer_ephemeral().await?;
    check_admin(&ctx)?;
    let thread_id = ThreadId::from(ctx.channel_id());

    let session = crate::db::get_session_by_thread(&ctx.data().db, thread_id).await?;

    let target_id = UserId::from(user.id);

    // Remove from session if one exists
    let mut actions: Vec<&str> = Vec::new();
    if let Some(ref session) = session {
        if target_id == session.owner_id {
            ctx.say("Cannot ban the session owner.").await?;
            return Ok(());
        }
        if crate::db::remove_participant(&ctx.data().db, thread_id, target_id).await? {
            actions.push("removed from session");
        }
    }

    // Revoke dynamic access (DB-approved via /optin)
    if crate::db::revoke_access(&ctx.data().db, user.id.get()).await? {
        actions.push("access revoked");
    }

    if actions.is_empty() {
        ctx.say(format!(
            "**{}** is not a participant and has no dynamic access.",
            user.name
        ))
        .await?;
    } else {
        ctx.say(format!("**{}**: {}.", user.name, actions.join(", ")))
            .await?;
    }
    Ok(())
}

/// Send potentially long text as ephemeral chunked messages.
async fn send_ephemeral_chunked(ctx: &Context<'_>, text: &str) -> Result<(), AppError> {
    if text.len() <= 2000 {
        ctx.say(text).await?;
    } else {
        let mut rest = text;
        while !rest.is_empty() {
            let end = if rest.len() <= 1990 {
                rest.len()
            } else {
                rest.floor_char_boundary(1990)
            };
            let (chunk, tail) = rest.split_at(end);
            rest = tail;
            ctx.channel_id().say(ctx.http(), chunk).await?;
        }
    }
    Ok(())
}

/// Truncate prompt at word boundary for display in the startup message.
#[inline]
fn truncate_prompt(prompt: &str, max: usize) -> String {
    let first_line = prompt.lines().next().unwrap_or(prompt);
    if first_line.len() <= max {
        first_line.to_string()
    } else {
        let end = first_line.floor_char_boundary(max - 3);
        let end = first_line[..end].rfind(' ').unwrap_or(end);
        format!("{}...", &first_line[..end])
    }
}

/// Build a clean thread name: "project — first words of prompt"
/// Truncates at word boundary, max 100 chars (Discord limit).
#[inline]
fn thread_name(project: &str, prompt: &str) -> String {
    const MAX: usize = 100;
    const ELLIPSIS: &str = "...";

    // "project — prompt excerpt"
    let prefix = format!("{project} — ");
    if prefix.len() >= MAX {
        let end = project.floor_char_boundary(MAX - ELLIPSIS.len());
        return format!("{}{ELLIPSIS}", &project[..end]);
    }
    let budget = MAX - prefix.len();

    // Take first line only
    let first_line = prompt.lines().next().unwrap_or(prompt);

    if first_line.len() <= budget {
        format!("{prefix}{first_line}")
    } else {
        let trunc = budget.saturating_sub(ELLIPSIS.len());
        let end = first_line.floor_char_boundary(trunc);
        let end = first_line[..end].rfind(' ').unwrap_or(end);
        format!("{prefix}{}{ELLIPSIS}", &first_line[..end])
    }
}
