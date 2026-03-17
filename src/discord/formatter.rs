use std::sync::Arc;

use poise::serenity_prelude as serenity;
use smallvec::SmallVec;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::AppState;
use crate::domain::{ClaudeEvent, SessionStatus, ThreadId};

const BUFFER_INITIAL_CAPACITY: usize = 2048;
const FLUSH_THRESHOLD: usize = 1800;
/// Inline tool timer entry — typically 1-2 concurrent tool calls.
type ToolTimers = SmallVec<[(i64, Instant); 4]>;
/// Inline audit ID tracker — maps tool name Arc to audit row ID.
type AuditIds = SmallVec<[(Arc<str>, i64); 4]>;

pub async fn stream_to_discord(
    http: Arc<serenity::Http>,
    channel_id: serenity::ChannelId,
    mut rx: mpsc::Receiver<ClaudeEvent>,
    state: Arc<AppState>,
    cancel: CancellationToken,
) {
    let thread_id = ThreadId::from(channel_id);

    // Typing indicator task
    let typing_cancel = cancel.child_token();
    let typing_cancel_trigger = typing_cancel.clone();
    let typing_http = Arc::clone(&http);
    let typing_channel = channel_id;
    let typing_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = typing_cancel.cancelled() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(8)) => {
                    let _ = typing_channel.broadcast_typing(&typing_http).await;
                }
            }
        }
    });

    // Main stream loop — processes events, then checks for pending messages
    loop {
        let sent_any = stream_events(&http, channel_id, &mut rx, &state, &cancel, thread_id).await;

        // Remove the active session handle (process is done)
        state.session_manager.remove(thread_id);

        // Check for pending messages before reporting "no response"
        if cancel.is_cancelled() {
            break;
        }

        let pending = state.session_manager.take_pending(thread_id);

        if !sent_any && pending.is_none() {
            tracing::warn!(
                ?thread_id,
                "stream finished with no content sent to discord"
            );
            send_message(&http, channel_id, "_Claude produced no response._").await;
        }

        let Some(messages) = pending else {
            break;
        };

        let msg_count = messages.len();
        // Check if multiple users contributed to decide on username prefix
        let multi_user = messages.iter().any(|m| m.user_id != messages[0].user_id);
        let combined = if multi_user {
            messages
                .iter()
                .map(|m| format!("[{}]: {}", m.username, m.content))
                .collect::<Vec<_>>()
                .join("\n\n")
        } else {
            messages
                .iter()
                .map(|m| m.content.as_ref())
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        // Update current user to the last message's author for tool attribution
        if let Some(last) = messages.last() {
            state.session_manager.set_current_user(
                thread_id,
                last.user_id,
                Arc::clone(&last.username),
            );
        }
        tracing::info!(?thread_id, count = msg_count, "processing queued messages");

        // Notify the user that a queued message is being picked up
        send_message(&http, channel_id, &format!("📨 _Queued ({msg_count} msg)_")).await;

        // Get session info for resume
        let session = crate::db::get_session_by_thread(&state.db, thread_id).await;
        let Ok(Some(session)) = session else {
            tracing::error!(?thread_id, "no session in db for pending messages");
            break;
        };

        let config = &state.config.claude;
        let resume_id = session.claude_session_id.as_ref().map(|s| s.as_str());
        let cwd_result = crate::claude::worktree::resolve_session_cwd(
            config,
            Some(&session.project),
            thread_id,
            session.worktree_path.as_deref(),
        )
        .await;
        let Ok((cwd, worktree_path)) = cwd_result else {
            tracing::error!(?thread_id, "could not resolve cwd for pending messages");
            break;
        };
        let tools = config.resolve_tools(Some(&session.project));

        let (tx, new_rx) = crate::claude::process::event_channel();
        let process_cancel = state.shutdown.child_token();

        // Rebuild co-author prompt for queued follow-ups (belt-and-suspenders with git hook)
        let coauthor_block = crate::db::get_participants(&state.db, thread_id)
            .await
            .ok()
            .and_then(|p| {
                crate::domain::build_coauthor_prompt(&p, &state.config.auth.user_identities)
            });
        let system_prompt_override = match (&config.system_prompt, &coauthor_block) {
            (Some(base), Some(coauthor)) => Some(format!("{base}\n\n{coauthor}")),
            (None, Some(coauthor)) => Some(coauthor.clone()),
            _ => None,
        };

        let handle = crate::claude::process::run_claude(
            config,
            &combined,
            resume_id,
            &cwd,
            &tools,
            system_prompt_override.as_deref(),
            tx,
            process_cancel,
        )
        .await;

        let Ok(handle) = handle else {
            tracing::error!(?thread_id, "failed to spawn claude for queued messages");
            send_message(
                &http,
                channel_id,
                "**Error:** failed to start Claude for queued message.",
            )
            .await;
            break;
        };

        if let Err(e) = state
            .session_manager
            .register(thread_id, handle, cwd, worktree_path)
        {
            tracing::error!(?thread_id, error = %e, "failed to register session for queued messages");
            break;
        }

        let _ = crate::db::touch_session(&state.db, thread_id).await;
        rx = new_rx;
    }

    // Final cleanup
    tracing::info!(?thread_id, "stream finished");
    typing_cancel_trigger.cancel();
    typing_task.abort();
    state.session_manager.remove(thread_id);
    let _ = crate::db::update_session_status(&state.db, thread_id, SessionStatus::Idle).await;
}

/// Stream events from a single Claude process invocation. Returns whether any content was sent.
async fn stream_events(
    http: &serenity::Http,
    channel_id: serenity::ChannelId,
    rx: &mut mpsc::Receiver<ClaudeEvent>,
    state: &AppState,
    cancel: &CancellationToken,
    thread_id: ThreadId,
) -> bool {
    let mut buffer = String::with_capacity(BUFFER_INITIAL_CAPACITY);
    let mut in_code_fence = false;
    let mut sent_any = false;
    // P3: SmallVec for typically-small collections (1-2 concurrent tool calls)
    let mut tool_timers: ToolTimers = SmallVec::new();
    let mut latest_audit_id: AuditIds = SmallVec::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!(?thread_id, "stream_to_discord cancelled");
                break;
            }
            event = rx.recv() => {
                tracing::debug!(?thread_id, event = ?event.as_ref().map(std::mem::discriminant), "stream event");
                match event {
                    Some(ClaudeEvent::TextDelta(text)) => {
                        sent_any = true;
                        buffer.push_str(&text);
                        update_fence_state(&text, &mut in_code_fence);

                        if buffer.len() >= FLUSH_THRESHOLD {
                            let chunk = take_chunk(&mut buffer, in_code_fence);
                            send_message(http, channel_id, &chunk).await;
                        }
                    }
                    Some(ClaudeEvent::ToolUse { tool, input_preview, input_json }) => {
                        if !buffer.is_empty() {
                            let chunk = take_all(&mut buffer);
                            send_message(http, channel_id, &chunk).await;
                            in_code_fence = false;
                        }
                        // Log to audit table with full JSON (best-effort)
                        let current_user = state.session_manager.get_current_user(thread_id);
                        let msg = match crate::db::log_tool_use(
                            &state.db, thread_id, current_user, &tool, &input_preview, &input_json,
                        ).await {
                            Ok(id) => {
                                tool_timers.push((id, Instant::now()));
                                // Remove previous entry for same tool, then push new
                                latest_audit_id.retain(|(t, _)| t != &tool);
                                latest_audit_id.push((Arc::clone(&tool), id));
                                format!("_Using {} ..._ `#{id}`", &*tool)
                            }
                            Err(e) => {
                                tracing::warn!(?thread_id, tool = &*tool, error = %e, "failed to log tool use");
                                format!("_Using {} ..._", &*tool)
                            }
                        };
                        send_message(http, channel_id, &msg).await;
                    }
                    Some(ClaudeEvent::ToolResult { tool, is_error, output_preview }) => {
                        let status = if is_error { "failed" } else { "done" };
                        // Update audit record with result (best-effort)
                        // P2: find + retain for linear scan on small collection
                        if let Some(pos) = latest_audit_id.iter().position(|(t, _)| t == &tool) {
                            let (_, audit_id) = latest_audit_id.remove(pos);
                            let duration_ms = tool_timers
                                .iter()
                                .position(|(id, _)| *id == audit_id)
                                .map(|i| tool_timers.remove(i).1.elapsed().as_millis() as i64);
                            let _ = crate::db::update_tool_result(
                                &state.db, audit_id, is_error, &output_preview, duration_ms,
                            ).await;
                        }
                        send_message(http, channel_id,
                            &format!("_{} {status}_", &*tool)).await;
                    }
                    Some(ClaudeEvent::ControlRequest(cr)) => {
                        if !buffer.is_empty() {
                            let chunk = take_all(&mut buffer);
                            send_message(http, channel_id, &chunk).await;
                            in_code_fence = false;
                        }
                        // Log to audit and track for result update (best-effort)
                        let current_user = state.session_manager.get_current_user(thread_id);
                        if let Ok(id) = crate::db::log_tool_use(
                            &state.db, thread_id, current_user, &cr.tool_name, &cr.question, &cr.input_json,
                        ).await {
                            tool_timers.push((id, Instant::now()));
                            latest_audit_id.retain(|(t, _)| t != &cr.tool_name);
                            latest_audit_id.push((Arc::clone(&cr.tool_name), id));
                        }

                        // Mention the user who triggered this tool call, falling back to owner
                        let mention = if let Some(uid) = current_user {
                            format!("<@{}>", uid.get())
                        } else {
                            crate::db::get_session_by_thread(&state.db, thread_id)
                                .await
                                .ok()
                                .flatten()
                                .map(|s| format!("<@{}>", s.owner_id.get()))
                                .unwrap_or_default()
                        };

                        // Display question with @mention to notify the thread owner
                        let display = if &*cr.tool_name == "AskUserQuestion" {
                            format!("{mention} **Claude asks:** {}", &*cr.question)
                        } else {
                            format!("{mention} **Permission required ({}):** {}", &*cr.tool_name, &*cr.question)
                        };
                        send_message(http, channel_id, &display).await;
                    }
                    Some(ClaudeEvent::SessionId(sid)) => {
                        state.session_manager.set_session_id(thread_id, sid.clone());
                        let _ = crate::db::update_session_id(
                            &state.db, thread_id, sid.as_str()
                        ).await;
                    }
                    Some(ClaudeEvent::ExitError(reason)) => {
                        if let Some(msg) = reason.user_message() {
                            sent_any = true;
                            send_message(http, channel_id, &msg).await;
                        }
                    }
                    Some(ClaudeEvent::Error(e)) => {
                        sent_any = true;
                        send_message(http, channel_id,
                            &format!("**Error:** {e}")).await;
                        break;
                    }
                    Some(ClaudeEvent::Done) | None => break,
                }
            }
        }
    }

    if !buffer.is_empty() {
        let chunk = take_all(&mut buffer);
        send_message(http, channel_id, &chunk).await;
    }

    // Finalize any unresolved audit entries (e.g. auto-denied control_requests)
    for (audit_id, start) in tool_timers.drain(..) {
        let duration_ms = start.elapsed().as_millis() as i64;
        let _ = crate::db::update_tool_result(
            &state.db,
            audit_id,
            false,
            "(no result — auto-denied or stream ended)",
            Some(duration_ms),
        )
        .await;
    }

    sent_any
}

#[inline]
fn update_fence_state(text: &str, in_fence: &mut bool) {
    let count = text.matches("```").count();
    if count % 2 == 1 {
        *in_fence = !*in_fence;
    }
}

fn take_chunk(buffer: &mut String, in_code_fence: bool) -> String {
    let split_at = find_split_point(buffer, FLUSH_THRESHOLD);

    let mut chunk = String::with_capacity(split_at + 8);
    chunk.push_str(&buffer[..split_at]);

    if in_code_fence {
        chunk.push_str("\n```");
    }

    let remainder_start = if buffer[split_at..].starts_with('\n') {
        split_at + 1
    } else {
        split_at
    };

    // P3: drain prefix in-place, avoid intermediate String allocation
    let remainder = buffer[remainder_start..].to_string();
    buffer.clear();
    if in_code_fence {
        buffer.push_str("```\n");
    }
    buffer.push_str(&remainder);

    chunk
}

fn take_all(buffer: &mut String) -> String {
    std::mem::take(buffer)
}

#[inline]
fn find_split_point(text: &str, max: usize) -> usize {
    let search_range = &text[..max.min(text.len())];

    search_range
        .rfind("\n\n")
        .map(|i| i + 1)
        .or_else(|| search_range.rfind('\n'))
        .or_else(|| search_range.rfind(' '))
        .unwrap_or(max.min(text.len()))
}

async fn send_message(http: &serenity::Http, channel_id: serenity::ChannelId, content: &str) {
    if content.is_empty() {
        return;
    }

    if content.len() > 2000 {
        let mut rest = content;
        while !rest.is_empty() {
            let end = if rest.len() <= 1990 {
                rest.len()
            } else {
                rest.floor_char_boundary(1990)
            };
            let (chunk, tail) = rest.split_at(end);
            rest = tail;
            if let Err(e) = channel_id.say(http, chunk).await {
                tracing::error!(channel = channel_id.get(), error = %e, "discord send failed");
            }
        }
    } else if let Err(e) = channel_id.say(http, content).await {
        tracing::error!(channel = channel_id.get(), error = %e, "discord send failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_state_tracking() {
        let mut in_fence = false;
        update_fence_state("```rust\nfn main() {}", &mut in_fence);
        assert!(in_fence);
        update_fence_state("}\n```", &mut in_fence);
        assert!(!in_fence);
    }

    #[test]
    fn fence_double_toggle() {
        let mut in_fence = false;
        update_fence_state("```code```", &mut in_fence);
        assert!(!in_fence);
    }

    #[test]
    fn split_at_double_newline() {
        let text = "line1\n\nline2\n\nline3";
        let pos = find_split_point(text, 15);
        assert!(text[..pos].ends_with('\n'));
    }

    #[test]
    fn split_at_newline_fallback() {
        let text = "line1\nline2\nline3";
        let pos = find_split_point(text, 12);
        assert_eq!(&text[..pos], "line1\nline2");
    }

    #[test]
    fn buffer_capacity_preserved() {
        let mut buf = String::with_capacity(BUFFER_INITIAL_CAPACITY);
        buf.push_str(&"x".repeat(1900));
        let _chunk = take_chunk(&mut buf, false);
        assert!(buf.capacity() >= BUFFER_INITIAL_CAPACITY);
    }
}
