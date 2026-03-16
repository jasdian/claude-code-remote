use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::Context;
use crate::domain::{ThreadId, UserId};
use crate::error::AppError;

#[inline]
async fn check_auth(ctx: &Context<'_>) -> Result<(), AppError> {
    let user_id = ctx.author().id.get();
    let auth = &ctx.data().config.auth;

    if auth.allowed_users.contains(&user_id) {
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

    Err(AppError::unauthorized(
        "not in allowed_users or allowed_roles",
    ))
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

    let cwd_str = config.resolve_cwd(project.as_deref());
    let cwd = Path::new(cwd_str);
    let tools = config.resolve_tools(project.as_deref());

    let is_dm = ctx.guild_id().is_none();

    // In DMs: reply directly in channel. In guilds: create a thread.
    let response_channel = if is_dm {
        ctx.say("Starting Claude session...").await?;
        ctx.channel_id()
    } else {
        let reply = ctx.say("Starting Claude session...").await?;
        let msg = reply.message().await?;
        let thread = ctx
            .channel_id()
            .create_thread_from_message(
                ctx.http(),
                msg.id,
                serenity::CreateThread::new(truncate_thread_name(&prompt))
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
    let project_name = project.as_deref().unwrap_or("default");
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
pub async fn stop(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer().await?;
    check_auth(&ctx).await?;
    let thread_id = ThreadId::from(ctx.channel_id());

    if let Some(handle) = ctx.data().session_manager.remove(thread_id) {
        handle.kill().await?;
        crate::db::update_session_status(&ctx.data().db, thread_id, "stopped").await?;
        ctx.say("Session stopped.").await?;
    } else if crate::db::get_session_by_thread(&ctx.data().db, thread_id)
        .await?
        .is_some()
    {
        crate::db::update_session_status(&ctx.data().db, thread_id, "stopped").await?;
        ctx.say("Session stopped (process already finished).")
            .await?;
    } else {
        ctx.say("No session in this thread.").await?;
    }
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
fn truncate_thread_name(prompt: &str) -> String {
    const PREFIX: &str = "CC: ";
    const SUFFIX: &str = "...";
    const MAX: usize = 100;
    const BUDGET: usize = MAX - PREFIX.len(); // 96
    const TRUNC_BUDGET: usize = BUDGET - SUFFIX.len(); // 93

    if prompt.len() <= BUDGET {
        format!("{PREFIX}{prompt}")
    } else {
        // Find a valid char boundary at or before TRUNC_BUDGET
        let end = prompt.floor_char_boundary(TRUNC_BUDGET);
        format!("{PREFIX}{}{SUFFIX}", &prompt[..end])
    }
}
