use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::Context;
use crate::domain::{SessionStatus, ThreadId, UserId};
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
    ctx.defer().await?;
    check_auth(&ctx).await?;

    let state = ctx.data();
    let config = &state.config.claude;

    let cwd_str = config.resolve_cwd(project.as_deref()).await?;
    let cwd = Path::new(cwd_str.as_ref());
    let tools = config.resolve_tools(project.as_deref());

    let is_dm = ctx.guild_id().is_none();
    let project_name = crate::project_name_from_cwd(cwd);
    let prompt_excerpt = truncate_prompt(&prompt, 200);
    let start_msg = format!("**{project_name}**\n> {prompt_excerpt}");

    // In DMs: reply directly in channel. In guilds: create a thread.
    let response_channel = if is_dm {
        ctx.say(&start_msg).await?;
        ctx.channel_id()
    } else {
        let reply = ctx.say(&start_msg).await?;
        let msg = reply.message().await?;
        let thread = ctx
            .channel_id()
            .create_thread_from_message(
                ctx.http(),
                msg.id,
                serenity::CreateThread::new(thread_name(project_name, &prompt))
                    .auto_archive_duration(serenity::AutoArchiveDuration::OneDay),
            )
            .await?;
        thread.id
    };

    let thread_id = ThreadId::from(response_channel);

    let (tx, rx) = crate::claude::process::event_channel();
    let cancel = state.shutdown.child_token();

    let handle =
        crate::claude::process::run_claude(config, &prompt, None, cwd, &tools, tx, cancel).await?;

    state
        .session_manager
        .register(thread_id, handle, cwd.to_path_buf())?;

    let user_id = UserId::from(ctx.author().id);
    crate::db::create_session(&state.db, thread_id, user_id, project_name).await?;

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

    let had_session = if let Some(handle) = ctx.data().session_manager.remove(thread_id) {
        handle.kill().await?;
        crate::db::update_session_status(&ctx.data().db, thread_id, SessionStatus::Stopped).await?;
        true
    } else if crate::db::get_session_by_thread(&ctx.data().db, thread_id)
        .await?
        .is_some()
    {
        crate::db::update_session_status(&ctx.data().db, thread_id, SessionStatus::Stopped).await?;
        true
    } else {
        false
    };

    if had_session {
        ctx.say("Session ended.").await?;
        // Archive the thread (not in DMs)
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
        ctx.data()
            .session_manager
            .queue_message(thread_id, msg);
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
    ctx.say("Access requested. An admin will review it.").await?;
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

#[poise::command(slash_command)]
pub async fn compact(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer().await?;
    check_auth(&ctx).await?;

    let state = ctx.data();
    let thread_id = ThreadId::from(ctx.channel_id());

    let session = crate::db::get_session_by_thread(&state.db, thread_id).await?;
    let Some(session) = session else {
        ctx.say("No session here.").await?;
        return Ok(());
    };

    // If busy, queue the compact command
    if state.session_manager.has_session(thread_id) {
        state
            .session_manager
            .queue_message(thread_id, "/compact".to_string());
        ctx.say("📨 _Compact queued._").await?;
        return Ok(());
    }

    // Resume with /compact
    let config = &state.config.claude;
    let resume_id = session.claude_session_id.as_ref().map(|s| s.as_str());
    let cwd_str = config.resolve_cwd(Some(&session.project)).await?;
    let cwd = Path::new(cwd_str.as_ref());
    let tools = config.resolve_tools(Some(&session.project));

    let (tx, rx) = crate::claude::process::event_channel();
    let cancel = state.shutdown.child_token();

    let handle =
        crate::claude::process::run_claude(config, "/compact", resume_id, cwd, &tools, tx, cancel)
            .await?;

    state
        .session_manager
        .register(thread_id, handle, cwd.to_path_buf())?;
    crate::db::touch_session(&state.db, thread_id).await?;

    ctx.say("_Compacting conversation..._").await?;

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
pub async fn audit(
    ctx: Context<'_>,
    #[description = "Tool use ID (omit for latest)"] id: Option<i64>,
    #[description = "Number of entries to show"] count: Option<i64>,
) -> Result<(), AppError> {
    ctx.defer_ephemeral().await?;
    check_admin(&ctx)?;
    let thread_id = crate::domain::ThreadId::from(ctx.channel_id());
    let n = count.unwrap_or(1).clamp(1, 50);
    let rows = crate::db::get_tool_uses(&ctx.data().db, thread_id, id, n).await?;
    if rows.is_empty() {
        ctx.say("No tool uses found.").await?;
    } else {
        let mut out = String::new();
        for (row_id, tool, preview, ts) in &rows {
            let preview_str = if preview.is_empty() {
                String::new()
            } else {
                format!(" — `{preview}`")
            };
            out.push_str(&format!("`#{row_id}` **{tool}**{preview_str} ({ts})\n"));
        }
        // Chunk if over Discord limit
        if out.len() > 2000 {
            let mut rest = out.as_str();
            while !rest.is_empty() {
                let end = if rest.len() <= 1990 { rest.len() } else { rest.floor_char_boundary(1990) };
                let (chunk, tail) = rest.split_at(end);
                rest = tail;
                ctx.channel_id().say(ctx.http(), chunk).await?;
            }
        } else {
            ctx.say(out).await?;
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
        let end = first_line[..end]
            .rfind(' ')
            .unwrap_or(end);
        format!("{prefix}{}{ELLIPSIS}", &first_line[..end])
    }
}
