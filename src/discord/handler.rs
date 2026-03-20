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
    if msg.author.bot || state.shutdown.is_cancelled() {
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
                // Update .claude-coauthors file if worktree exists
                if let Some(ref wt_path_str) = session.worktree_path
                    && let Ok(participants) =
                        crate::db::get_participants(&state.db, thread_id).await
                {
                    let content = domain::build_coauthors_file_content(
                        &participants,
                        &state.config.auth.user_identities,
                    );
                    let _ = crate::claude::worktree::write_coauthors_file(
                        std::path::Path::new(wt_path_str.as_ref()),
                        content.as_deref(),
                    )
                    .await;
                }
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

        // Resume existing session (or start fresh if previously stopped)
        let is_stopped = session.status == SessionStatus::Stopped;
        // If stopped, worktree/session were cleaned up by /end — start fresh
        let resume_id = if is_stopped {
            None
        } else {
            session.claude_session_id.as_ref().map(|s| s.as_str())
        };
        let worktree = if is_stopped {
            None
        } else {
            session.worktree_path.as_deref()
        };
        tracing::info!(
            ?thread_id,
            claude_session_id = resume_id,
            project = %session.project,
            is_stopped,
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
            resume_id,
            worktree,
        )
        .await;
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

    // Thread follow-up with no active/idle/stopped session — auto-resume expired sessions
    if !is_dm(msg)
        && is_authorized(state, msg).await
        && let Some(old) = crate::db::get_any_session_by_thread(&state.db, thread_id).await?
        && old.owner_id == user_id
        && matches!(old.status, SessionStatus::Expired)
    {
        tracing::info!(?thread_id, "auto-resuming expired session");
        return start_new_in_thread(ctx, msg, state, thread_id, user_id, &prompt).await;
    }

    Ok(())
}

/// Delete old session row and start a fresh session in the same thread.
/// Cleans up any orphaned worktree from the previous session first.
async fn start_new_in_thread(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    state: &Arc<AppState>,
    thread_id: ThreadId,
    user_id: UserId,
    prompt: &str,
) -> Result<(), AppError> {
    // Clean up old worktree before deleting session row (prevents orphan on disk)
    if let Ok(Some(old_session)) = crate::db::get_any_session_by_thread(&state.db, thread_id).await
        && let Some(ref wt_path_str) = old_session.worktree_path
    {
        crate::claude::worktree::remove_worktree(std::path::Path::new(wt_path_str.as_ref()), false)
            .await;
    }
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
    if let Err(e) = start_claude(ctx, msg, state, thread_id, prompt, None, None, None).await {
        // Clean up orphaned DB row to prevent broken state on next message
        let _ = crate::db::delete_session_by_thread(&state.db, thread_id).await;
        let _ = msg
            .channel_id
            .say(ctx, &format!("Failed to start session: {e}"))
            .await;
        return Err(e);
    }
    Ok(())
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

    // P2: map_err + ok() to log and discard error in one chain
    let participants = crate::db::get_participants(&state.db, thread_id)
        .await
        .map_err(
            |e| tracing::warn!(error = %e, "failed to fetch participants for co-author prompt"),
        )
        .ok();

    let coauthor_block = participants
        .as_ref()
        .and_then(|p| domain::build_coauthor_prompt(p, &state.config.auth.user_identities));

    // Install git hook + write .claude-coauthors file if worktree is present
    if let Some(ref wt) = worktree_path {
        let coauthors_content = participants.as_ref().and_then(|p| {
            domain::build_coauthors_file_content(p, &state.config.auth.user_identities)
        });
        crate::claude::worktree::setup_coauthor_hook(wt, coauthors_content.as_deref()).await;
    }

    // Fetch sibling context summaries (if context sharing enabled)
    let context_block = if state.config.claude.context_sharing.enabled {
        let project_name = project.unwrap_or("");
        crate::claude::context::build_context_prompt(
            &state.db,
            thread_id,
            project_name,
            state.config.claude.context_sharing.max_summary_chars,
        )
        .await
    } else {
        None
    };

    let combined_prompt = crate::claude::context::assemble_system_prompt(
        config.system_prompt.as_deref(),
        coauthor_block.as_deref(),
        context_block.as_deref(),
    );

    let (tx, rx) = crate::claude::process::event_channel();
    let (stdin_tx, stdin_rx) = crate::claude::process::stdin_channel();
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
        stdin_rx,
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
    use crate::discord::{is_affirmative, is_negative};

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

    #[test]
    fn negative_replies() {
        for word in ["no", "n", "Nah", "NOPE", "deny", "cancel", "stop"] {
            assert!(is_negative(word), "{word:?} should be negative");
        }
    }

    #[test]
    fn non_negative_replies() {
        for word in ["yes", "maybe", "fix the bug", "", "nothing", "notable"] {
            assert!(!is_negative(word), "{word:?} should not be negative");
        }
    }
}
