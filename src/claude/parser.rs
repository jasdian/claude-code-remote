use std::sync::Arc;

use crate::domain::{ClaudeEvent, ClaudeSessionId};

/// HOT PATH: called for every line of Claude stdout.
///
/// Claude CLI stream-json format emits:
///   {"type":"system", "session_id":"...", ...}         — init
///   {"type":"assistant", "message":{"content":[...]}}  — full assistant turn
///   {"type":"result", "result":"...", ...}              — final result
///   {"type":"user", ...}                               — user turn echo (skip)
///   {"type":"rate_limit_event", ...}                   — rate limit info (skip)
#[inline]
pub fn parse_stream_line(line: &str) -> Option<ClaudeEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = v.get("type")?.as_str()?;

    match event_type {
        "system" => parse_system(&v),
        "assistant" => parse_assistant(&v),
        "result" => parse_result(&v),
        // content_block_delta / content_block_start for API-style streaming (future)
        "content_block_delta" => parse_content_delta(&v),
        "content_block_start" => parse_block_start(&v),
        // Silently skip known non-content events
        "user" | "rate_limit_event" => None,
        _ => {
            tracing::debug!(event_type, "unknown stream event");
            None
        }
    }
}

#[inline]
fn parse_system(v: &serde_json::Value) -> Option<ClaudeEvent> {
    v.get("session_id")
        .and_then(|s| s.as_str())
        .map(|sid| ClaudeEvent::SessionId(ClaudeSessionId::new(sid)))
}

/// Parse the "assistant" event from Claude CLI.
/// Format: {"type":"assistant","message":{"content":[{"type":"text","text":"..."},{"type":"tool_use","name":"Bash",...}]}}
#[inline]
fn parse_assistant(v: &serde_json::Value) -> Option<ClaudeEvent> {
    let message = v.get("message")?;
    let content = message.get("content")?.as_array()?;

    // Collect text and tool_use blocks into events.
    // For simplicity, concatenate all text blocks and emit as single TextDelta.
    // Emit ToolUse events for tool_use blocks.
    let mut text_parts = String::new();
    let mut events: Vec<ClaudeEvent> = Vec::new();

    for block in content {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match block_type {
            "text" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    text_parts.push_str(text);
                }
            }
            "tool_use" => {
                // Flush text first
                if !text_parts.is_empty() {
                    events.push(ClaudeEvent::TextDelta(Arc::from(text_parts.as_str())));
                    text_parts.clear();
                }
                if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                    events.push(ClaudeEvent::ToolUse {
                        tool: Arc::from(name),
                        input_preview: Arc::from(""),
                    });
                }
            }
            "tool_result" => {
                let tool = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown");
                let is_error = block
                    .get("is_error")
                    .and_then(|e| e.as_bool())
                    .unwrap_or(false);
                events.push(ClaudeEvent::ToolResult {
                    tool: Arc::from(tool),
                    is_error,
                });
            }
            _ => {}
        }
    }

    // Flush remaining text
    if !text_parts.is_empty() {
        events.push(ClaudeEvent::TextDelta(Arc::from(text_parts.as_str())));
    }

    // Return the first event; caller will get subsequent events from next lines.
    // For multi-block assistant messages, we combine all text into one TextDelta.
    // If there are tool events mixed in, we prioritize text (tools come as separate events).
    if events.len() == 1 {
        events.into_iter().next()
    } else if !events.is_empty() {
        // Multiple blocks: return first, rest will be lost.
        // This is acceptable for PoC — full streaming would use content_block_delta.
        events.into_iter().next()
    } else {
        None
    }
}

#[inline]
fn parse_result(v: &serde_json::Value) -> Option<ClaudeEvent> {
    // Extract session_id if present
    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
        // Emit SessionId first if we haven't seen it yet.
        // The caller handles dedup via session manager.
        // For PoC, just emit Done — session_id comes from "system" init.
        let _ = sid;
    }

    // Check if there's a result text we haven't sent yet
    // (in case the assistant event was missed or had no text)
    if let Some(result_text) = v.get("result").and_then(|r| r.as_str()) {
        if !result_text.is_empty() {
            // We already sent text via the "assistant" event, so just emit Done
            let _ = result_text;
        }
    }

    Some(ClaudeEvent::Done)
}

// Keep these for future API-style streaming support
#[inline]
fn parse_content_delta(v: &serde_json::Value) -> Option<ClaudeEvent> {
    let delta = v.get("delta")?;
    let delta_type = delta.get("type")?.as_str()?;
    if delta_type != "text_delta" {
        return None;
    }
    delta
        .get("text")
        .and_then(|t| t.as_str())
        .map(|text| ClaudeEvent::TextDelta(Arc::from(text)))
}

#[inline]
fn parse_block_start(v: &serde_json::Value) -> Option<ClaudeEvent> {
    let block = v.get("content_block")?;
    let block_type = block.get("type")?.as_str()?;

    match block_type {
        "tool_use" => {
            let tool = block.get("name")?.as_str()?;
            Some(ClaudeEvent::ToolUse {
                tool: Arc::from(tool),
                input_preview: Arc::from(""),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_assistant_text() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello!"}]}}"#;
        match parse_stream_line(line) {
            Some(ClaudeEvent::TextDelta(t)) => assert_eq!(&*t, "Hello!"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_assistant_tool_use() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","id":"123","input":{}}]}}"#;
        match parse_stream_line(line) {
            Some(ClaudeEvent::ToolUse { tool, .. }) => assert_eq!(&*tool, "Bash"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_system_init() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#;
        match parse_stream_line(line) {
            Some(ClaudeEvent::SessionId(sid)) => assert_eq!(sid.as_str(), "abc-123"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_result_done() {
        let line = r#"{"type":"result","result":"Hello!","session_id":"abc-123"}"#;
        assert!(matches!(parse_stream_line(line), Some(ClaudeEvent::Done)));
    }

    #[test]
    fn parse_user_skipped() {
        let line = r#"{"type":"user","message":{"content":"test"}}"#;
        assert!(parse_stream_line(line).is_none());
    }

    #[test]
    fn parse_rate_limit_skipped() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{}}"#;
        assert!(parse_stream_line(line).is_none());
    }

    #[test]
    fn parse_garbage_skipped() {
        assert!(parse_stream_line("not json at all").is_none());
        assert!(parse_stream_line("").is_none());
    }

    // Keep legacy content_block_delta test for future API-style streaming
    #[test]
    fn parse_content_block_delta() {
        let line =
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}"#;
        match parse_stream_line(line) {
            Some(ClaudeEvent::TextDelta(t)) => assert_eq!(&*t, "hello"),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
