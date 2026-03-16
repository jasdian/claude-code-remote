use std::sync::Arc;

use poise::serenity_prelude as serenity;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::{ClaudeEvent, ThreadId};
use crate::AppState;

const BUFFER_INITIAL_CAPACITY: usize = 2048;
const FLUSH_THRESHOLD: usize = 1800;

pub async fn stream_to_discord(
    http: Arc<serenity::Http>,
    channel_id: serenity::ChannelId,
    mut rx: mpsc::Receiver<ClaudeEvent>,
    state: Arc<AppState>,
    cancel: CancellationToken,
) {
    let mut buffer = String::with_capacity(BUFFER_INITIAL_CAPACITY);
    let mut in_code_fence = false;
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

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!(?thread_id, "stream_to_discord cancelled");
                break;
            }
            event = rx.recv() => {
                match event {
                    Some(ClaudeEvent::TextDelta(text)) => {
                        buffer.push_str(&text);
                        update_fence_state(&text, &mut in_code_fence);

                        if buffer.len() >= FLUSH_THRESHOLD {
                            let chunk = take_chunk(&mut buffer, in_code_fence);
                            send_message(&http, channel_id, &chunk).await;
                        }
                    }
                    Some(ClaudeEvent::ToolUse { tool, .. }) => {
                        if !buffer.is_empty() {
                            let chunk = take_all(&mut buffer);
                            send_message(&http, channel_id, &chunk).await;
                            in_code_fence = false;
                        }
                        send_message(&http, channel_id,
                            &format!("_Using {} ..._", &*tool)).await;
                    }
                    Some(ClaudeEvent::ToolResult { tool, is_error }) => {
                        let status = if is_error { "failed" } else { "done" };
                        send_message(&http, channel_id,
                            &format!("_{} {status}_", &*tool)).await;
                    }
                    Some(ClaudeEvent::SessionId(sid)) => {
                        state.session_manager.set_session_id(thread_id, sid.clone());
                        let _ = crate::db::update_session_id(
                            &state.db, thread_id, sid.as_str()
                        ).await;
                    }
                    Some(ClaudeEvent::Error(e)) => {
                        send_message(&http, channel_id,
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
        send_message(&http, channel_id, &chunk).await;
    }

    // Cleanup
    typing_cancel_trigger.cancel();
    typing_task.abort();
    state.session_manager.remove(thread_id);
    let _ = crate::db::update_session_status(&state.db, thread_id, "idle").await;
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

    let remainder = buffer[remainder_start..].to_string();
    buffer.clear();
    if in_code_fence {
        buffer.push_str("```\n");
    }
    buffer.push_str(&remainder);

    chunk
}

fn take_all(buffer: &mut String) -> String {
    let chunk = buffer.clone();
    buffer.clear();
    chunk
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
        for chunk in content.as_bytes().chunks(1990) {
            let s = String::from_utf8_lossy(chunk);
            let _ = channel_id.say(http, &*s).await;
        }
    } else {
        let _ = channel_id.say(http, content).await;
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
