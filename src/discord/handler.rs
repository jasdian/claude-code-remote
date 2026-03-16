use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::AppState;
use crate::domain::{ThreadId, UserId};
use crate::error::AppError;

/// Check if a channel is a DM
fn is_dm(msg: &serenity::Message) -> bool {
    msg.guild_id.is_none()
}

/// Check if user is authorized
fn is_authorized(state: &AppState, user_id: u64) -> bool {
    state.config.auth.allowed_users.contains(&user_id)
}

/// Strip bot mention prefix from message content. P2: returns Cow to avoid allocation when no mention.
#[inline]
fn strip_bot_mention(content: &str, bot_id: u64) -> Cow<'_, str> {
    let mention = format!("<@{bot_id}>");
    let mention_nick = format!("<@!{bot_id}>");

    let stripped = content
        .strip_prefix(mention.as_str())
        .or_else(|| content.strip_prefix(mention_nick.as_str()))
        .map(str::trim);

    match stripped {
        Some(s) if !s.is_empty() => Cow::Borrowed(s),
        Some(_) => Cow::Borrowed(content), // Empty after stripping — use original
        None => Cow::Borrowed(content),
    }
}

pub async fn handle_message(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    state: &Arc<AppState>,
) -> Result<(), AppError> {
    if msg.author.bot {
        return Ok(());
    }

    let bot_id = ctx.cache.current_user().id;
    let is_mentioned = msg.mentions.iter().any(|u| u.id == bot_id);
    let prompt = strip_bot_mention(&msg.content, bot_id.get());

    tracing::debug!(
        channel_id = msg.channel_id.get(),
        author = msg.author.name,
        is_dm = is_dm(msg),
        is_mentioned,
        content_len = msg.content.len(),
        "received message"
    );

    let thread_id = ThreadId::from(msg.channel_id);
    let user_id = UserId::from(msg.author.id);

    // Check for existing session (thread follow-up or DM continuation)
    if let Some(session) = crate::db::get_session_by_thread(&state.db, thread_id).await? {
        if session.user_id != user_id {
            return Ok(());
        }

        if state.session_manager.has_session(thread_id) {
            msg.reply(ctx, "Claude is still processing. Please wait.")
                .await?;
            return Ok(());
        }

        // Resume existing session
        tracing::info!(?thread_id, "resuming session");
        return start_claude(
            ctx,
            msg,
            state,
            thread_id,
            &prompt,
            Some(&session.project),
            session.claude_session_id.as_ref().map(|s| s.as_str()),
        )
        .await;
    }

    // No existing session — if this is a DM, auto-create one
    if is_dm(msg) && is_authorized(state, msg.author.id.get()) {
        tracing::info!(user = msg.author.name, "new DM session");

        crate::db::create_session(&state.db, thread_id, user_id, "default").await?;
        return start_claude(ctx, msg, state, thread_id, &prompt, None, None).await;
    }

    Ok(())
}

async fn start_claude(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    state: &Arc<AppState>,
    thread_id: ThreadId,
    prompt: &str,
    project: Option<&str>,
    resume_id: Option<&str>,
) -> Result<(), AppError> {
    let config = &state.config.claude;
    let cwd_str = config.resolve_cwd(project).await?;
    let cwd = Path::new(cwd_str.as_ref());
    let tools = config.resolve_tools(project);

    let (tx, rx) = crate::claude::process::event_channel();
    let cancel = state.shutdown.child_token();

    let handle =
        crate::claude::process::run_claude(config, prompt, resume_id, cwd, &tools, tx, cancel)
            .await?;

    state
        .session_manager
        .register(thread_id, handle, cwd.to_path_buf())?;
    crate::db::touch_session(&state.db, thread_id).await?;

    let stream_cancel = state.shutdown.child_token();
    tokio::spawn(super::formatter::stream_to_discord(
        Arc::clone(&ctx.http),
        msg.channel_id,
        rx,
        Arc::clone(state),
        stream_cancel,
    ));

    Ok(())
}
