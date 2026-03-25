use std::sync::Arc;

use smallvec::SmallVec;

use crate::domain::{
    ClaudeEvent, ClaudeExitReason, ClaudeSessionId, ControlRequestData, ToolInputEntry,
};

/// Parsed output from a single stdout line.
pub struct ParsedLine {
    pub events: SmallVec<[ClaudeEvent; 2]>,
    /// Result text from a "result" event — only present once per session.
    /// Caller should emit as TextDelta only if no prior content was received
    /// (to avoid duplicating text already sent via "assistant" events).
    pub result_text: Option<Arc<str>>,
}

/// HOT PATH: called for every line of Claude stdout.
///
/// Claude CLI stream-json format (with --verbose --include-partial-messages) emits:
///   {"type":"system", "subtype":"init", "session_id":"...", ...}  — init
///   {"type":"stream_event", "event":{...}}                        — streaming partial events
///   {"type":"assistant", "message":{"content":[...]}}             — full assistant turn
///   {"type":"result", "subtype":"success", "result":"...", ...}   — final result
///   {"type":"user", "message":{"content":[...]}}                  — tool results
///   {"type":"system", "subtype":"api_retry", ...}                 — API retry info
///   {"type":"rate_limit_event", ...}                              — rate limit info (skip)
#[inline]
pub fn parse_stream_line(line: &str) -> ParsedLine {
    let empty = ParsedLine {
        events: SmallVec::new(),
        result_text: None,
    };
    let Some(v) = serde_json::from_str::<serde_json::Value>(line).ok() else {
        return empty;
    };
    let Some(event_type) = v.get("type").and_then(|t| t.as_str()) else {
        return empty;
    };

    match event_type {
        "system" => ParsedLine {
            events: parse_system(&v).into_iter().collect(),
            result_text: None,
        },
        // stream_event wraps raw API streaming events (content_block_delta, etc.)
        // Emitted with --include-partial-messages flag.
        "stream_event" => {
            let Some(inner) = v.get("event") else {
                return empty;
            };
            let Some(inner_type) = inner.get("type").and_then(|t| t.as_str()) else {
                return empty;
            };
            match inner_type {
                "content_block_delta" => ParsedLine {
                    events: parse_content_delta(inner).into_iter().collect(),
                    result_text: None,
                },
                "content_block_start" => ParsedLine {
                    events: parse_block_start(inner).into_iter().collect(),
                    result_text: None,
                },
                // message_start, content_block_stop, message_delta, message_stop — skip
                _ => empty,
            }
        }
        // With --include-partial-messages, assistant events duplicate text already
        // received via stream_event deltas. We skip text, but extract tool_use
        // input_json (which is complete here but empty on content_block_start).
        // Also detect auth/API errors embedded in the assistant event's "error" field.
        "assistant" => {
            if let Some(err) = v.get("error").and_then(|e| e.as_str())
                && matches!(err, "authentication_failed" | "unauthorized")
            {
                let detail = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|b| b.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or(err);
                return ParsedLine {
                    events: SmallVec::from_elem(
                        ClaudeEvent::ExitError(ClaudeExitReason::AuthFailure(detail.into())),
                        1,
                    ),
                    result_text: None,
                };
            }
            ParsedLine {
                events: parse_assistant_tool_inputs(&v).into_iter().collect(),
                result_text: None,
            }
        }
        "result" => ParsedLine {
            events: SmallVec::from_elem(ClaudeEvent::Done, 1),
            result_text: parse_result(&v),
        },
        "user" => ParsedLine {
            events: parse_user(&v),
            result_text: None,
        },
        "control_request" => ParsedLine {
            events: parse_control_request(&v).into_iter().collect(),
            result_text: None,
        },
        // Legacy top-level events (without stream_event wrapper)
        "content_block_delta" => ParsedLine {
            events: parse_content_delta(&v).into_iter().collect(),
            result_text: None,
        },
        "content_block_start" => ParsedLine {
            events: parse_block_start(&v).into_iter().collect(),
            result_text: None,
        },
        // Silently skip known non-content events
        "rate_limit_event" => empty,
        _ => {
            tracing::debug!(event_type, "unknown stream event");
            empty
        }
    }
}

#[inline]
fn parse_system(v: &serde_json::Value) -> Option<ClaudeEvent> {
    let subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
    match subtype {
        "init" | "" => v
            .get("session_id")
            .and_then(|s| s.as_str())
            .map(|sid| ClaudeEvent::SessionId(ClaudeSessionId::new(sid))),
        "api_retry" => {
            let attempt = v.get("attempt").and_then(|a| a.as_u64()).unwrap_or(0);
            let error = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
            tracing::warn!(attempt, error, "claude API retry");
            None
        }
        _ => {
            tracing::debug!(subtype, "unknown system subtype");
            None
        }
    }
}

/// Parse "user" events — these contain tool_result blocks.
/// P2: filter_map + collect for single-pass extraction.
#[inline]
fn parse_user(v: &serde_json::Value) -> SmallVec<[ClaudeEvent; 2]> {
    let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return SmallVec::new();
    };
    content
        .iter()
        .filter_map(|block| {
            let block_type = block.get("type").and_then(|t| t.as_str())?;
            if block_type != "tool_result" {
                return None;
            }
            // Prefer tool name; fall back to tool_use_id for formatter's
            // FIFO audit matching (the formatter replaces it with the real name).
            let tool = block
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_else(|| {
                    block
                        .get("tool_use_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or("tool")
                });
            let is_error = block
                .get("is_error")
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            let output_preview = extract_result_preview(block);
            Some(ClaudeEvent::ToolResult {
                tool: Arc::from(tool),
                is_error,
                output_preview: Arc::from(output_preview.as_str()),
            })
        })
        .collect()
}

/// Extract tool_use input_json from "assistant" events.
/// With --include-partial-messages, text and tool events are already handled by
/// stream_event deltas. But assistant events have the *complete* input objects
/// (content_block_start has "input": {}). We extract these for DB backfill.
/// P2: filter_map chain for single-pass extraction.
#[inline]
fn parse_assistant_tool_inputs(v: &serde_json::Value) -> Option<ClaudeEvent> {
    let content = v.get("message")?.get("content")?.as_array()?;
    let entries: SmallVec<[ToolInputEntry; 2]> = content
        .iter()
        .filter_map(|block| {
            if block.get("type")?.as_str()? != "tool_use" {
                return None;
            }
            let name = block.get("name")?.as_str()?;
            let input = block.get("input")?;
            // Skip empty input objects (defensive)
            if input.as_object().is_some_and(|m| m.is_empty()) {
                return None;
            }
            Some(ToolInputEntry {
                tool: Arc::from(name),
                input_json: Arc::from(input.to_string().as_str()),
            })
        })
        .collect();

    if entries.is_empty() {
        None
    } else {
        Some(ClaudeEvent::ToolInputBackfill(Box::new(entries)))
    }
}

/// Parse the "assistant" event from Claude CLI.
/// Format: {"type":"assistant","message":{"content":[{"type":"text","text":"..."},{"type":"tool_use","name":"Bash",...}]}}
/// P2: fold for (events, text_buf) accumulation in a single pass.
/// NOTE: Currently unused — with --include-partial-messages, stream_event deltas
/// provide the same content. Kept as fallback for non-streaming mode.
#[allow(dead_code)]
#[inline]
fn parse_assistant(v: &serde_json::Value) -> SmallVec<[ClaudeEvent; 2]> {
    let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return SmallVec::new();
    };

    let (mut events, text_buf) = content.iter().fold(
        (SmallVec::<[ClaudeEvent; 2]>::new(), String::new()),
        |(mut events, mut text_buf), block| {
            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match block_type {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        text_buf.push_str(text);
                    }
                }
                "tool_use" => {
                    // Flush text first
                    if !text_buf.is_empty() {
                        events.push(ClaudeEvent::TextDelta(Arc::from(text_buf.as_str())));
                        text_buf.clear();
                    }
                    if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                        let preview = extract_tool_preview(name, block);
                        let input_json = extract_input_json(block);
                        events.push(ClaudeEvent::ToolUse {
                            tool: Arc::from(name),
                            input_preview: Arc::from(preview.as_str()),
                            input_json: Arc::from(input_json.as_str()),
                        });
                    }
                }
                _ => {}
            }
            (events, text_buf)
        },
    );

    // Flush remaining text
    if !text_buf.is_empty() {
        events.push(ClaudeEvent::TextDelta(Arc::from(text_buf.as_str())));
    }

    events
}

/// Parse the "result" event. Returns the result text (if any) for the caller
/// to decide whether to emit it (avoids duplicating content from assistant events).
/// Suppresses error results — those are already handled by the assistant event's "error" field.
#[inline]
fn parse_result(v: &serde_json::Value) -> Option<Arc<str>> {
    if v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false) {
        return None;
    }
    v.get("result")
        .and_then(|r| r.as_str())
        .filter(|s| !s.is_empty())
        .map(Arc::from)
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
            let preview = extract_tool_preview(tool, block);
            let input_json = extract_input_json(block);
            Some(ClaudeEvent::ToolUse {
                tool: Arc::from(tool),
                input_preview: Arc::from(preview.as_str()),
                input_json: Arc::from(input_json.as_str()),
            })
        }
        _ => None,
    }
}

/// Parse a `control_request` event (permission prompt or AskUserQuestion).
#[inline]
fn parse_control_request(v: &serde_json::Value) -> Option<ClaudeEvent> {
    let request_id = v.get("request_id")?.as_str()?;
    let request = v.get("request")?;
    let tool_name = request
        .get("tool_name")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown");

    let question = extract_control_question(tool_name, request);
    let input_json = request
        .get("input")
        .map(|i| i.to_string())
        .unwrap_or_default();

    Some(ClaudeEvent::ControlRequest(Box::new(ControlRequestData {
        request_id: Arc::from(request_id),
        tool_name: Arc::from(tool_name),
        question: Arc::from(question.as_str()),
        input_json: Arc::from(input_json.as_str()),
    })))
}

/// Extract human-readable question text from a control_request.
#[inline]
fn extract_control_question(tool_name: &str, request: &serde_json::Value) -> String {
    let input = request.get("input");

    if tool_name == "AskUserQuestion" {
        if let Some(q) = input
            .and_then(|i| i.get("question"))
            .and_then(|q| q.as_str())
        {
            return q.to_string();
        }
        if let Some(q) = input
            .and_then(|i| i.get("questions"))
            .and_then(|qs| qs.as_array())
            .and_then(|qs| qs.first())
            .and_then(|q| q.get("question"))
            .and_then(|q| q.as_str())
        {
            return q.to_string();
        }
    }

    request
        .get("title")
        .and_then(|t| t.as_str())
        .or_else(|| request.get("description").and_then(|d| d.as_str()))
        .map(String::from)
        .unwrap_or_else(|| format!("{tool_name} requires approval"))
}

/// Serialize the full `input` field of a tool_use block to JSON string.
#[inline]
fn extract_input_json(block: &serde_json::Value) -> String {
    block
        .get("input")
        .map(|i| i.to_string())
        .unwrap_or_default()
}

/// Extract a short preview from a tool_result block's content.
#[inline]
fn extract_result_preview(block: &serde_json::Value) -> String {
    if let Some(content) = block.get("content") {
        if let Some(s) = content.as_str() {
            return truncate_preview(s, 200);
        }
        if let Some(arr) = content.as_array() {
            let text = arr
                .iter()
                .filter_map(|b| {
                    (b.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .then(|| b.get("text").and_then(|t| t.as_str()))
                        .flatten()
                })
                .fold(String::new(), |mut acc, s| {
                    if !acc.is_empty() {
                        acc.push('\n');
                    }
                    acc.push_str(s);
                    acc
                });
            if !text.is_empty() {
                return truncate_preview(&text, 200);
            }
        }
    }
    block
        .get("output")
        .and_then(|o| o.as_str())
        .map(|s| truncate_preview(s, 200))
        .unwrap_or_default()
}

/// Truncate a string at a word/char boundary for preview display.
#[inline]
fn truncate_preview(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max - 3);
        format!("{}...", &s[..end])
    }
}

/// Extract a short preview from tool input for display.
#[inline]
fn extract_tool_preview(tool: &str, block: &serde_json::Value) -> String {
    let input = match block.get("input") {
        Some(i) => i,
        None => return String::new(),
    };

    let preview = match tool {
        "Bash" => input.get("command").and_then(|v| v.as_str()).unwrap_or(""),
        "Read" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "Write" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "Edit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "Grep" => input.get("pattern").and_then(|v| v.as_str()).unwrap_or(""),
        "Glob" => input.get("pattern").and_then(|v| v.as_str()).unwrap_or(""),
        "Agent" => input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        _ => "",
    };

    if preview.len() > 120 {
        let end = preview.floor_char_boundary(117);
        format!("{}...", &preview[..end])
    } else {
        preview.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_assistant_text_only_no_events() {
        // Text-only assistant events produce no events (text already streamed via deltas)
        let line =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello!"}]}}"#;
        let parsed = parse_stream_line(line);
        assert!(parsed.events.is_empty());
    }

    #[test]
    fn parse_assistant_extracts_tool_inputs() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"reading"},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/src/main.rs"}},{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/src/lib.rs","old_string":"a","new_string":"b"}}]}}"#;
        let parsed = parse_stream_line(line);
        assert_eq!(parsed.events.len(), 1);
        match &parsed.events[0] {
            ClaudeEvent::ToolInputBackfill(entries) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(&*entries[0].tool, "Read");
                assert!(entries[0].input_json.contains("file_path"));
                assert_eq!(&*entries[1].tool, "Edit");
                assert!(entries[1].input_json.contains("old_string"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_assistant_empty_input_skipped() {
        // content_block_start-style empty input should not produce backfill entries
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}"#;
        let parsed = parse_stream_line(line);
        assert!(parsed.events.is_empty());
    }

    #[test]
    fn parse_assistant_fn_directly() {
        // parse_assistant still works if called directly (fallback for non-streaming)
        let v: serde_json::Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello!"},{"type":"tool_use","name":"Bash","id":"123","input":{"command":"ls"}}]}}"#
        ).unwrap();
        let events = parse_assistant(&v);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ClaudeEvent::TextDelta(t) if &**t == "Hello!"));
        assert!(matches!(&events[1], ClaudeEvent::ToolUse { tool, .. } if &**tool == "Bash"));
    }

    #[test]
    fn parse_system_init() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#;
        let parsed = parse_stream_line(line);
        assert_eq!(parsed.events.len(), 1);
        match &parsed.events[0] {
            ClaudeEvent::SessionId(sid) => assert_eq!(sid.as_str(), "abc-123"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_result_with_text() {
        let line = r#"{"type":"result","result":"Hello!","session_id":"abc-123"}"#;
        let parsed = parse_stream_line(line);
        assert_eq!(parsed.events.len(), 1);
        assert!(matches!(&parsed.events[0], ClaudeEvent::Done));
        assert_eq!(&*parsed.result_text.unwrap(), "Hello!");
    }

    #[test]
    fn parse_result_empty_text() {
        let line = r#"{"type":"result","result":"","session_id":"abc-123"}"#;
        let parsed = parse_stream_line(line);
        assert_eq!(parsed.events.len(), 1);
        assert!(matches!(&parsed.events[0], ClaudeEvent::Done));
        assert!(parsed.result_text.is_none());
    }

    #[test]
    fn parse_user_tool_result() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","name":"Bash","content":"hello world","is_error":false}]}}"#;
        let parsed = parse_stream_line(line);
        assert_eq!(parsed.events.len(), 1);
        match &parsed.events[0] {
            ClaudeEvent::ToolResult {
                tool,
                is_error,
                output_preview,
            } => {
                assert_eq!(&**tool, "Bash");
                assert!(!is_error);
                assert_eq!(&**output_preview, "hello world");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_user_tool_result_error() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool_abc","content":"command failed","is_error":true}]}}"#;
        let parsed = parse_stream_line(line);
        assert_eq!(parsed.events.len(), 1);
        match &parsed.events[0] {
            ClaudeEvent::ToolResult {
                tool,
                is_error,
                output_preview,
            } => {
                assert_eq!(&**tool, "tool_abc");
                assert!(is_error);
                assert_eq!(&**output_preview, "command failed");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_user_no_tool_result() {
        let line = r#"{"type":"user","message":{"content":[{"type":"text","text":"hello"}]}}"#;
        let parsed = parse_stream_line(line);
        assert!(parsed.events.is_empty());
    }

    #[test]
    fn parse_rate_limit_skipped() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{}}"#;
        assert!(parse_stream_line(line).events.is_empty());
    }

    #[test]
    fn parse_garbage_skipped() {
        assert!(parse_stream_line("not json at all").events.is_empty());
        assert!(parse_stream_line("").events.is_empty());
    }

    #[test]
    fn parse_content_block_delta() {
        let line = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}"#;
        let parsed = parse_stream_line(line);
        assert_eq!(parsed.events.len(), 1);
        match &parsed.events[0] {
            ClaudeEvent::TextDelta(t) => assert_eq!(&**t, "hello"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_stream_event_text_delta() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"streaming!"}}}"#;
        let parsed = parse_stream_line(line);
        assert_eq!(parsed.events.len(), 1);
        match &parsed.events[0] {
            ClaudeEvent::TextDelta(t) => assert_eq!(&**t, "streaming!"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_stream_event_tool_start() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Read","id":"123"}}}"#;
        let parsed = parse_stream_line(line);
        assert_eq!(parsed.events.len(), 1);
        match &parsed.events[0] {
            ClaudeEvent::ToolUse { tool, .. } => assert_eq!(&**tool, "Read"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_stream_event_message_stop_skipped() {
        let line = r#"{"type":"stream_event","event":{"type":"message_stop"}}"#;
        let parsed = parse_stream_line(line);
        assert!(parsed.events.is_empty());
    }

    #[test]
    fn parse_assistant_auth_error() {
        let line = r#"{"type":"assistant","message":{"id":"abc","content":[{"type":"text","text":"Failed to authenticate. API Error: 401"}]},"error":"authentication_failed","session_id":"s1"}"#;
        let parsed = parse_stream_line(line);
        assert_eq!(parsed.events.len(), 1);
        match &parsed.events[0] {
            ClaudeEvent::ExitError(ClaudeExitReason::AuthFailure(detail)) => {
                assert!(
                    detail.contains("401"),
                    "detail should contain 401: {detail}"
                );
            }
            other => panic!("expected AuthFailure, got: {other:?}"),
        }
        assert!(parsed.result_text.is_none());
    }

    #[test]
    fn parse_result_error_suppressed() {
        let line = r#"{"type":"result","subtype":"success","is_error":true,"result":"Failed to authenticate."}"#;
        let parsed = parse_stream_line(line);
        assert_eq!(parsed.events.len(), 1);
        assert!(matches!(&parsed.events[0], ClaudeEvent::Done));
        assert!(
            parsed.result_text.is_none(),
            "error result text should be suppressed"
        );
    }

    #[test]
    fn parse_system_api_retry() {
        let line = r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":3,"error":"rate_limit"}"#;
        let parsed = parse_stream_line(line);
        assert!(parsed.events.is_empty());
    }
}
