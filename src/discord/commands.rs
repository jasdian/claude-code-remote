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
        let project = session.as_ref().map(|s| s.project.as_ref()).unwrap_or("");
        let auto_pr = ctx.data().config.claude.resolve_auto_pr(Some(project));
        let mut pr_url: Option<String> = None;
        let mut pushed = false;

        if let Some(ref wt) = wt_path {
            // Try auto-PR first if enabled
            if auto_pr {
                pr_url = crate::claude::worktree::try_create_pr(wt, project).await;
            }

            // If no PR was created, try pushing the branch standalone
            if pr_url.is_none() {
                match crate::claude::worktree::try_push_branch(wt).await {
                    Ok(did_push) => pushed = did_push,
                    Err(e) => tracing::warn!(error = %e, "worktree branch push failed"),
                }
            }
        }

        // Keep branch if work was pushed (PR or standalone push)
        let keep_branch = pr_url.is_some() || pushed;
        if let Some(ref wt) = wt_path {
            crate::claude::worktree::remove_worktree(wt, keep_branch).await;
        }

        crate::db::update_session_status(&ctx.data().db, thread_id, SessionStatus::Stopped).await?;
        let _ =
            crate::db::mark_summary_status(&ctx.data().db, thread_id, SessionStatus::Stopped).await;

        let msg = match (&pr_url, pushed) {
            (Some(url), _) => format!("Session ended. PR created: {url}"),
            (None, true) => "Session ended. Branch pushed to remote.".into(),
            (None, false) => "Session ended.".into(),
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

/// List git repositories discoverable as sibling directories of default_cwd.
#[poise::command(slash_command)]
pub async fn projects(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer().await?;
    check_auth(&ctx).await?;

    let config = &ctx.data().config.claude;
    let default_cwd = config.default_cwd.as_ref();

    // P2: early return if no parent directory
    let parent = Path::new(default_cwd)
        .parent()
        .ok_or_else(|| AppError::config("default_cwd has no parent directory"))?;

    // P4: async directory reading
    let mut entries = tokio::fs::read_dir(parent)
        .await
        .map_err(|e| AppError::config(&format!("cannot read {}: {e}", parent.display())))?;

    // P2: collect git repos in a single pass via filter_map logic
    let mut repos: Vec<(String, bool)> = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| AppError::config(&format!("reading directory entry: {e}")))?
    {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let git_dir = path.join(".git");
        // P4: async metadata check
        let is_git = tokio::fs::metadata(&git_dir)
            .await
            .is_ok_and(|m| m.is_dir());
        if !is_git {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // Check if this project has explicit config
        let has_config = config.projects.contains_key(name.as_str());
        repos.push((name, has_config));
    }

    if repos.is_empty() {
        ctx.say(format!(
            "No git repositories found in `{}`.",
            parent.display()
        ))
        .await?;
        return Ok(());
    }

    repos.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    // P2: fold into output string
    let mut out = format!(
        "**Git projects in `{}`** ({} found)\n",
        parent.display(),
        repos.len()
    );
    for (name, has_config) in &repos {
        let marker = if *has_config { " ⚙" } else { "" };
        out.push_str(&format!("• `{name}`{marker}\n"));
    }
    out.push_str("\n_⚙ = has explicit config in `[claude.projects]`_");

    send_ephemeral_chunked(&ctx, &out).await?;
    Ok(())
}

#[poise::command(slash_command)]
pub async fn sessions(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer().await?;
    check_auth(&ctx).await?;
    let state = ctx.data();
    let max = state.config.claude.max_sessions;

    let live = crate::db::get_live_sessions(&state.db).await?;
    if live.is_empty() {
        ctx.say(format!("No active sessions. (max: {max})")).await?;
        return Ok(());
    }

    let now = chrono::Utc::now();
    let mut out = format!("**Sessions: {}/{}**\n", live.len(), max);
    for s in &live {
        let age = super::formatter::format_duration(now - s.created_at);
        let status = if state.session_manager.has_session(s.thread_id) {
            "active"
        } else {
            "idle"
        };
        out.push_str(&format!(
            "• <#{}> — **{}** — {age} — <@{}> ({status})\n",
            s.thread_id.get(),
            s.project,
            s.owner_id.get(),
        ));
    }
    ctx.say(&out).await?;
    Ok(())
}

#[inline]
async fn check_admin(ctx: &Context<'_>) -> Result<(), AppError> {
    let user_id = ctx.author().id.get();
    let auth = &ctx.data().config.auth;

    if auth.admins.contains(&user_id) {
        return Ok(());
    }

    if !auth.admin_roles.is_empty()
        && let Some(member) = ctx.author_member().await
    {
        let has_role = member
            .roles
            .iter()
            .any(|role| auth.admin_roles.iter().any(|&ar| ar == role.get()));
        if has_role {
            return Ok(());
        }
    }

    Err(AppError::unauthorized("not an admin"))
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
    check_admin(&ctx).await?;
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
    check_admin(&ctx).await?;
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
    check_admin(&ctx).await?;
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

/// Authenticate with Claude via OAuth (admin only).
#[poise::command(slash_command)]
pub async fn login(ctx: Context<'_>) -> Result<(), AppError> {
    ctx.defer().await?;
    check_admin(&ctx).await?;

    // P2: early return if token is still valid
    if crate::claude::oauth::is_token_valid().await {
        ctx.say("Already authenticated (token not expired). Use `/claude` to start a session.")
            .await?;
        return Ok(());
    }

    // Generate PKCE verifier + challenge
    let pkce = crate::claude::oauth::generate_pkce().await?;
    let auth_url = crate::claude::oauth::build_authorize_url(&pkce.challenge, &pkce.state);

    ctx.say(format!(
        "**Authentication required.**\n\n\
         1. Open this URL and authorize:\n{auth_url}\n\n\
         2. After authorizing, you'll see a page with an **authorization code**.\n\
         3. **Paste that code here** within 5 minutes."
    ))
    .await?;

    let state = ctx.data();
    let channel_id = ctx.channel_id();
    let http = Arc::clone(&ctx.serenity_context().http);
    let login_thread_id = crate::domain::ThreadId::from(channel_id);

    // Set a reply_waiter to capture the authorization code from the user's next message.
    // handler.rs intercepts the message and sends it through this oneshot channel.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state.session_manager.set_reply_waiter(login_thread_id, reply_tx);

    let state_ref = state.clone();
    tokio::spawn(async move {
        // Wait for the user to paste the auth code (with 5 minute timeout)
        let code_result = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            reply_rx,
        )
        .await;

        let code = match code_result {
            Ok(Ok(code)) => code.trim().to_string(),
            Ok(Err(_)) => {
                let _ = channel_id.say(&http, "Login cancelled.").await;
                return;
            }
            Err(_) => {
                state_ref.session_manager.take_reply_waiter(login_thread_id);
                let _ = channel_id
                    .say(
                        &http,
                        "**Login timed out.** No authorization code received within 5 minutes.\n\n\
                         **Alternative:** run `claude auth login` on a machine with a browser, \
                         then copy `~/.claude/.credentials.json` to the server.",
                    )
                    .await;
                return;
            }
        };

        // The callback page shows "code#state" — split on '#' to extract just the code.
        // Also handle if user pastes the full callback URL.
        let code = if code.starts_with("http") {
            // User pasted the full callback URL — extract code param
            code.split("code=")
                .nth(1)
                .and_then(|s| s.split('&').next())
                .unwrap_or(&code)
                .to_string()
        } else if let Some((c, _)) = code.split_once('#') {
            c.to_string()
        } else {
            code
        };

        if code.is_empty() {
            let _ = channel_id.say(&http, "Empty code received. Login cancelled.").await;
            return;
        }

        let _ = channel_id.say(&http, "Exchanging authorization code for tokens...").await;

        // Exchange the code + verifier at the token endpoint
        match crate::claude::oauth::exchange_code(&code, &pkce.verifier, &pkce.state).await {
            Ok(token_response) => {
                match crate::claude::oauth::write_credentials(&token_response).await {
                    Ok(()) => {
                        tracing::info!("OAuth login successful, credentials written");
                        let _ = channel_id
                            .say(&http, "Authentication successful. Use `/claude` to start a session.")
                            .await;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to write credentials");
                        let _ = channel_id
                            .say(&http, &format!("**Error writing credentials:** {e}"))
                            .await;
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "token exchange failed");
                let _ = channel_id
                    .say(&http, &format!("**Token exchange failed:** {e}"))
                    .await;
            }
        }
    });

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
    check_admin(&ctx).await?;
    let thread_id = crate::domain::ThreadId::from(ctx.channel_id());

    // Detail mode: show full input_json for specific ID(s)
    if detail.unwrap_or(false) {
        if let Some(target_id) = id {
            return audit_detail(&ctx, target_id).await;
        }
        // No specific ID: show detail for last N entries (thread-scoped if in session thread)
        let n = count.unwrap_or(1).clamp(1, 10);
        let in_thread = crate::db::get_session_by_thread(&ctx.data().db, thread_id)
            .await?
            .is_some();
        let rows = if in_thread {
            crate::db::get_tool_uses(&ctx.data().db, thread_id, None, n).await?
        } else {
            crate::db::get_tool_uses_global(&ctx.data().db, n).await?
        };
        let mut out = String::with_capacity(2048);
        for row in &rows {
            if let Some(d) = crate::db::get_tool_use_detail(&ctx.data().db, row.id).await? {
                format_audit_detail(&mut out, &d);
            }
        }
        if out.is_empty() {
            ctx.say("No tool uses found.").await?;
        } else {
            send_ephemeral_chunked(&ctx, &out).await?;
        }
        return Ok(());
    }

    let n = count.unwrap_or(3).clamp(1, 50);

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
                let sanitized = sanitize_preview(&row.result_preview);
                let preview = if sanitized.len() > 80 {
                    format!("{}…", &sanitized[..sanitized.floor_char_boundary(80)])
                } else {
                    sanitized
                };
                format!(" → `{preview}`")
            };
            let error_marker = if row.is_error { " ❌" } else { "" };
            let duration_str = row
                .duration_ms
                .map(|ms| format!(" {}", format_duration(ms)))
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

    let mut out = String::with_capacity(2048);
    format_audit_detail(&mut out, &d);
    send_ephemeral_chunked(ctx, &out).await?;
    Ok(())
}

/// Append formatted audit detail for a single tool use to a buffer (P3: buffer reuse).
fn format_audit_detail(buf: &mut String, d: &crate::db::ToolUseDetail) {
    use std::fmt::Write;
    let error_marker = if d.is_error { " ❌" } else { "" };
    let duration_str = d
        .duration_ms
        .map(|ms| format!(" ({})", format_duration(ms)))
        .unwrap_or_default();

    let _ = writeln!(
        buf,
        "`#{}` **{}**{error_marker}{duration_str} — {}",
        d.id, d.tool, d.created_at
    );

    if !d.input_json.is_empty() {
        buf.push_str("\n**Input:**\n```json\n");
        buf.push_str(&d.input_json);
        buf.push_str("\n```\n");
    } else if !d.input_preview.is_empty() {
        buf.push_str("\n**Input:** `");
        buf.push_str(&d.input_preview);
        buf.push_str("`\n");
    }

    if !d.result_preview.is_empty() {
        buf.push_str("\n**Result:**\n```\n");
        buf.push_str(&d.result_preview);
        buf.push_str("\n```\n");
    }
    buf.push('\n');
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

/// Transfer session ownership to another participant.
#[poise::command(slash_command)]
pub async fn handoff(
    ctx: Context<'_>,
    #[description = "User to transfer ownership to"] user: serenity::User,
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

    // Only owner or admin can transfer
    let is_admin = ctx
        .data()
        .config
        .auth
        .admins
        .contains(&ctx.author().id.get());
    if session.owner_id != caller_id && !is_admin {
        ctx.say("Only the session owner or an admin can transfer ownership.")
            .await?;
        return Ok(());
    }

    let target_id = UserId::from(user.id);

    // Already owner?
    if target_id == session.owner_id {
        ctx.say(format!("**{}** is already the owner.", user.name))
            .await?;
        return Ok(());
    }

    // Must be a participant
    if !crate::db::is_participant(&ctx.data().db, thread_id, target_id).await? {
        ctx.say(format!(
            "**{}** is not a participant. They must send a message first to join.",
            user.name
        ))
        .await?;
        return Ok(());
    }

    crate::db::transfer_ownership(&ctx.data().db, thread_id, session.owner_id, target_id).await?;

    ctx.say(format!("Ownership transferred to **{}**.", user.name))
        .await?;
    // Visible notification in the thread
    ctx.channel_id()
        .say(
            ctx.http(),
            format!("Session ownership transferred to <@{}>.", user.id.get()),
        )
        .await?;
    Ok(())
}

/// Ban a user from session and revoke their dynamic access.
#[poise::command(slash_command, rename = "sessionban")]
pub async fn sessionban(
    ctx: Context<'_>,
    #[description = "User to ban from session and revoke access"] user: serenity::User,
) -> Result<(), AppError> {
    ctx.defer_ephemeral().await?;
    check_admin(&ctx).await?;
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

/// Send potentially long text as chunked messages.
/// First chunk completes the deferred interaction; subsequent chunks are followups.
async fn send_ephemeral_chunked(ctx: &Context<'_>, text: &str) -> Result<(), AppError> {
    use super::formatter::{find_split_point, update_fence_state};
    const CHUNK_MAX: usize = 1900; // leave room for fence close/open markers

    if text.len() <= 2000 {
        ctx.say(text).await?;
        return Ok(());
    }

    let mut remaining = text.to_string();
    let mut in_fence = false;

    while !remaining.is_empty() {
        if remaining.len() <= 2000 {
            ctx.say(&remaining).await?;
            break;
        }
        let split = find_split_point(&remaining, CHUNK_MAX);
        let mut chunk = remaining[..split].to_string();

        // Track fence state of this chunk
        update_fence_state(&chunk, &mut in_fence);

        if in_fence {
            chunk.push_str("\n```");
        }

        ctx.say(&chunk).await?;

        // Prepare remainder
        let rest_start = if remaining[split..].starts_with('\n') {
            split + 1
        } else {
            split
        };
        remaining = if in_fence {
            format!("```\n{}", &remaining[rest_start..])
        } else {
            remaining[rest_start..].to_string()
        };
    }
    Ok(())
}

/// Format milliseconds as human-readable duration.
#[inline]
fn format_duration(ms: i64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let mins = ms / 60_000;
        let secs = (ms % 60_000) / 1000;
        format!("{mins}m{secs}s")
    }
}

/// Sanitize a result preview for inline display: collapse whitespace, strip HTML tags.
#[inline]
fn sanitize_preview(s: &str) -> String {
    // Strip HTML tags (simple heuristic: remove <...> sequences)
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    // Collapse whitespace (newlines, tabs, multiple spaces) into single space
    out.split_whitespace().collect::<Vec<_>>().join(" ")
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
