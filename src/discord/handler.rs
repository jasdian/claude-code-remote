use std::borrow::Cow;
use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::AppState;
use crate::domain::{self, SessionStatus, ThreadId, UserId, UserMessage};
use crate::error::AppError;

/// Best-effort reaction — log on failure but never abort the handler.
#[inline]
async fn try_react(ctx: &serenity::Context, msg: &serenity::Message, emoji: &str) {
    if let Err(e) = msg
        .react(ctx, serenity::ReactionType::Unicode(emoji.into()))
        .await
    {
        tracing::warn!(emoji, error = %e, "failed to add reaction (missing Add Reactions permission?)");
    }
}

/// Timeout for "start new session?" confirmation (seconds).
const CONFIRM_TIMEOUT_SECS: u64 = 60;

/// Check if a channel is a DM
fn is_dm(msg: &serenity::Message) -> bool {
    msg.guild_id.is_none()
}

/// Check if user is authorized (config, roles, or DB-approved)
async fn is_authorized(state: &AppState, msg: &serenity::Message) -> bool {
    let user_id = msg.author.id.get();
    let auth = &state.config.auth;
    if auth.allowed_users.contains(&user_id) || auth.admins.contains(&user_id) {
        return true;
    }
    // Check guild roles (msg.member is present for guild messages)
    if let Some(ref member) = msg.member {
        let has_role = member.roles.iter().any(|role| {
            auth.allowed_roles
                .iter()
                .any(|&allowed| allowed == role.get())
        });
        if has_role {
            return true;
        }
    }
    crate::db::is_user_approved(&state.db, user_id)
        .await
        .unwrap_or(false)
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
        // Multi-user: allow owner and participants; auto-join authorized users
        let is_owner = session.owner_id == user_id;
        if !is_owner {
            if !is_authorized(state, msg).await {
                return Ok(());
            }
            // Auto-join: authorized user in session thread becomes participant
            if !crate::db::is_participant(&state.db, thread_id, user_id).await? {
                crate::db::add_participant(&state.db, thread_id, user_id, &msg.author.name).await?;
                tracing::info!(
                    ?thread_id,
                    user = msg.author.name,
                    "user auto-joined session"
                );
            }
        }

        // Log message for audit trail
        let _ =
            crate::db::log_message(&state.db, thread_id, user_id, &msg.author.name, &prompt).await;

        if state.session_manager.has_session(thread_id) {
            // Check if there's a pending reply waiter (AskUserQuestion / permission)
            if let Some(reply_tx) = state.session_manager.take_reply_waiter(thread_id) {
                let _ = reply_tx.send(prompt.to_string());
                try_react(ctx, msg, "💬").await;
                tracing::info!(?thread_id, "routed reply to control_request waiter");
                return Ok(());
            }

            // Session is busy — check for interrupt or queue
            let (is_interrupt, clean_prompt) = parse_interrupt(&prompt);

            if is_interrupt {
                tracing::info!(?thread_id, "interrupting session");
                // Queue BEFORE interrupt to avoid race with stream_to_discord
                state.session_manager.queue_message(
                    thread_id,
                    UserMessage {
                        user_id,
                        username: Arc::from(msg.author.name.as_str()),
                        content: Arc::from(clean_prompt),
                    },
                );
                state.session_manager.interrupt(thread_id);
                try_react(ctx, msg, "⏭️").await;
            } else {
                state.session_manager.queue_message(
                    thread_id,
                    UserMessage {
                        user_id,
                        username: Arc::from(msg.author.name.as_str()),
                        content: Arc::from(prompt.as_ref()),
                    },
                );
                try_react(ctx, msg, "📨").await;
                tracing::info!(?thread_id, "message queued");
            }
            return Ok(());
        }

        // Resume existing session
        tracing::info!(
            ?thread_id,
            claude_session_id = session.claude_session_id.as_ref().map(|s| s.as_str()),
            project = %session.project,
            "resuming session",
        );
        // Track current user for tool attribution
        state.session_manager.set_current_user(
            thread_id,
            user_id,
            Arc::from(msg.author.name.as_str()),
        );
        return start_claude(
            ctx,
            msg,
            state,
            thread_id,
            &prompt,
            Some(&session.project),
            session.claude_session_id.as_ref().map(|s| s.as_str()),
            session.worktree_path.as_deref(),
        )
        .await;
    }

    // Check for pending "start new session?" confirmation (user-scoped)
    if let Some(original_prompt) = state.session_manager.take_confirm_new(thread_id, user_id) {
        if is_affirmative(&prompt) {
            tracing::info!(?thread_id, "user confirmed new session in thread");
            return start_new_in_thread(ctx, msg, state, thread_id, user_id, &original_prompt)
                .await;
        }
        // Not affirmative — discard confirmation, ignore message
        tracing::debug!(?thread_id, "user declined new session");
        try_react(ctx, msg, "👌").await;
        return Ok(());
    }

    // No existing session — if this is a DM, auto-create one
    if is_dm(msg) && is_authorized(state, msg).await {
        tracing::info!(user = msg.author.name, "new DM session");

        let cwd = state.config.claude.resolve_cwd(None).await?;
        let project_name = crate::project_name_from_cwd(std::path::Path::new(cwd.as_ref()));
        crate::db::create_session(
            &state.db,
            thread_id,
            user_id,
            project_name,
            None,
            &msg.author.name,
        )
        .await?;
        // Track current user for tool attribution
        state.session_manager.set_current_user(
            thread_id,
            user_id,
            Arc::from(msg.author.name.as_str()),
        );
        return start_claude(ctx, msg, state, thread_id, &prompt, None, None, None).await;
    }

    // Thread follow-up with no active/idle session — check if an expired/stopped session exists
    if !is_dm(msg)
        && is_authorized(state, msg).await
        && let Some(old) = crate::db::get_any_session_by_thread(&state.db, thread_id).await?
        && old.owner_id == user_id
        && matches!(old.status, SessionStatus::Stopped | SessionStatus::Expired)
    {
        let status_label = old.status.as_str();
        let mention = format!("<@{}>", user_id.get());
        let warning = format!(
            "{mention} Previous session is **{status_label}**. \
             Reply **yes** within {CONFIRM_TIMEOUT_SECS}s to start a new conversation, \
             or use `/claude` to start fresh in a new thread."
        );
        msg.channel_id.say(ctx, &warning).await?;

        // Store original prompt and schedule timeout cleanup (user-scoped)
        state
            .session_manager
            .set_confirm_new(thread_id, user_id, prompt.to_string());

        let state_ref = Arc::clone(state);
        let tid = thread_id;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(CONFIRM_TIMEOUT_SECS)).await;
            state_ref.session_manager.remove_confirm_new(tid);
            tracing::debug!(?tid, "new-session confirmation timed out");
        });

        return Ok(());
    }

    Ok(())
}

/// Check if the user's reply is an affirmative ("yes", "y", "yeah", etc.)
#[inline]
fn is_affirmative(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "yes" | "y" | "yeah" | "yep" | "yup" | "sure" | "ok" | "okay"
    )
}

/// Delete old session row and start a fresh session in the same thread.
async fn start_new_in_thread(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    state: &Arc<AppState>,
    thread_id: ThreadId,
    user_id: UserId,
    prompt: &str,
) -> Result<(), AppError> {
    crate::db::delete_session_by_thread(&state.db, thread_id).await?;

    let cwd = state.config.claude.resolve_cwd(None).await?;
    let project_name = crate::project_name_from_cwd(std::path::Path::new(cwd.as_ref()));
    crate::db::create_session(
        &state.db,
        thread_id,
        user_id,
        project_name,
        None,
        &msg.author.name,
    )
    .await?;
    // Track current user for tool attribution
    state
        .session_manager
        .set_current_user(thread_id, user_id, Arc::from(msg.author.name.as_str()));
    start_claude(ctx, msg, state, thread_id, prompt, None, None, None).await
}

/// Check if message is an interrupt request. `!` prefix interrupts current and sends the rest.
#[inline]
fn parse_interrupt(prompt: &str) -> (bool, &str) {
    if let Some(rest) = prompt.strip_prefix('!') {
        (true, rest.trim())
    } else {
        (false, prompt)
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_claude(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    state: &Arc<AppState>,
    thread_id: ThreadId,
    prompt: &str,
    project: Option<&str>,
    resume_id: Option<&str>,
    existing_worktree: Option<&str>,
) -> Result<(), AppError> {
    let config = &state.config.claude;
    let (cwd, worktree_path) =
        crate::claude::worktree::resolve_session_cwd(config, project, thread_id, existing_worktree)
            .await?;
    let tools = config.resolve_tools(project);

    // Build combined system prompt: config base + co-author trailers for multi-user sessions
    let coauthor_block = match crate::db::get_participants(&state.db, thread_id).await {
        Ok(participants) => {
            domain::build_coauthor_prompt(&participants, &state.config.auth.user_identities)
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to fetch participants for co-author prompt");
            None
        }
    };

    let combined_prompt = match (&config.system_prompt, &coauthor_block) {
        (Some(base), Some(coauthor)) => Some(format!("{base}\n\n{coauthor}")),
        (None, Some(coauthor)) => Some(coauthor.clone()),
        (Some(_), None) => None, // use config default via None override
        (None, None) => None,
    };

    let (tx, rx) = crate::claude::process::event_channel();
    let cancel = state.shutdown.child_token();

    let handle = crate::claude::process::run_claude(
        config,
        prompt,
        resume_id,
        &cwd,
        &tools,
        combined_prompt.as_deref(),
        tx,
        cancel,
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
        .register(thread_id, handle, cwd, worktree_path)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affirmative_replies() {
        for word in ["yes", "y", "Yeah", "YEP", "sure", "OK", "okay", "  Yes  "] {
            assert!(is_affirmative(word), "{word:?} should be affirmative");
        }
    }

    #[test]
    fn non_affirmative_replies() {
        for word in ["no", "nope", "fix the bug", "", "yesterday", "yesbutno"] {
            assert!(!is_affirmative(word), "{word:?} should not be affirmative");
        }
    }
}
