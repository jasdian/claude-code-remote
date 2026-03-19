use std::collections::HashMap;
use std::fmt::Write;

use smallvec::SmallVec;
use sqlx::SqlitePool;

use crate::db::{self, ContextToolUseRow, SessionSummaryRow};
use crate::domain::ThreadId;

// --- File path extraction ---

/// Extract file path from a tool use's input_json.
/// P2: and_then chain for nested optional access.
#[inline]
fn extract_file_path(tool: &str, input_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(input_json).ok()?;
    match tool {
        "Read" | "Write" | "Edit" => v.get("file_path")?.as_str().map(shorten_path),
        "Grep" | "Glob" => v.get("path").and_then(|p| p.as_str()).map(shorten_path),
        _ => None,
    }
}

/// Shorten an absolute path to a relative-looking one for compact display.
/// P2: find last meaningful prefix boundary.
#[inline]
fn shorten_path(path: &str) -> String {
    // Try to find "src/" or similar common prefix to trim at
    if let Some(pos) = path.find("/src/") {
        return path[pos + 1..].to_string();
    }
    // Fall back to last 3 path components
    let parts: Vec<&str> = path.rsplit('/').take(3).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

// --- Tool activity summary ---

/// Build compact tool summary like "Read:12, Edit:3, Bash:5".
/// P2: fold for single-pass accumulation.
fn build_tools_summary(tool_uses: &[ContextToolUseRow]) -> String {
    let counts: HashMap<&str, usize> = tool_uses.iter().fold(HashMap::new(), |mut acc, tu| {
        *acc.entry(tu.tool.as_str()).or_insert(0) += 1;
        acc
    });

    // Sort by count descending for most relevant tools first
    let mut sorted: SmallVec<[(&str, usize); 8]> = counts.into_iter().collect();
    sorted.sort_unstable_by(|a, b| b.1.cmp(&a.1));

    sorted
        .iter()
        .map(|(tool, count)| format!("{tool}:{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// --- Work description ---

/// Build work description from recent messages + tool activity.
/// P3: pre-allocate with estimated capacity.
fn build_work_description(
    recent_messages: &[(String, String)],
    tools_summary: &str,
    file_count: usize,
) -> String {
    let mut desc = String::with_capacity(256);

    // Last user message (most relevant context)
    if let Some((username, content)) = recent_messages.last() {
        let truncated = if content.len() > 100 {
            &content[..content.floor_char_boundary(100)]
        } else {
            content.as_str()
        };
        let _ = write!(desc, "[{username}]: {truncated}");
    }

    if !tools_summary.is_empty() || file_count > 0 {
        let _ = write!(desc, " | {file_count} files, {tools_summary}");
    }

    desc
}

// --- Conflict detection ---

/// Detect file overlap between a session's files and sibling summaries.
/// P3: SmallVec for typically-small overlap sets.
pub fn detect_conflicts(
    my_files: &[String],
    siblings: &[SessionSummaryRow],
) -> SmallVec<[(i64, Vec<String>); 2]> {
    if my_files.is_empty() {
        return SmallVec::new();
    }

    siblings
        .iter()
        .filter_map(|sib| {
            let sib_files: Vec<String> =
                serde_json::from_str(&sib.files_touched).unwrap_or_default();
            let overlap: Vec<String> = my_files
                .iter()
                .filter(|f| sib_files.iter().any(|sf| sf == *f))
                .cloned()
                .collect();
            if overlap.is_empty() {
                None
            } else {
                Some((sib.thread_id, overlap))
            }
        })
        .collect()
}

// --- Context prompt builder ---

/// Build the sibling awareness block for --append-system-prompt.
/// Returns None if no siblings exist.
pub async fn build_context_prompt(
    pool: &SqlitePool,
    thread_id: ThreadId,
    project: &str,
    max_chars: usize,
) -> Option<String> {
    let siblings = db::get_sibling_summaries(pool, project, thread_id)
        .await
        .unwrap_or_default();

    if siblings.is_empty() {
        return None;
    }

    // Get our own files for conflict detection
    let my_summary: Option<SessionSummaryRow> = sqlx::query_as(
        "SELECT thread_id, project, status, files_touched, tools_summary,
                work_description, last_tool_use_id, updated_at
         FROM session_summaries WHERE thread_id = ?",
    )
    .bind(thread_id.get() as i64)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let my_files: Vec<String> = my_summary
        .as_ref()
        .and_then(|s| serde_json::from_str(&s.files_touched).ok())
        .unwrap_or_default();

    let conflicts = detect_conflicts(&my_files, &siblings);

    let mut out = String::with_capacity(max_chars);
    out.push_str("## Sibling Sessions (same project)\nYou are one of multiple Claude sessions on this project. Coordinate to avoid conflicts.\n");

    // Budget per sibling: divide remaining space
    let header_len = out.len();
    let conflict_budget = 200; // reserve for conflict warnings
    let per_sibling =
        (max_chars.saturating_sub(header_len + conflict_budget)) / siblings.len().max(1);

    for sib in &siblings {
        if out.len() + per_sibling > max_chars.saturating_sub(conflict_budget) {
            break;
        }

        // Parse age from updated_at
        let age = chrono::DateTime::parse_from_str(&sib.updated_at, "%Y-%m-%dT%H:%M:%S%.fZ")
            .ok()
            .map(|dt| {
                let mins = (chrono::Utc::now() - dt.to_utc()).num_minutes();
                if mins < 1 {
                    "just now".to_string()
                } else if mins < 60 {
                    format!("{mins}m ago")
                } else {
                    format!("{}h ago", mins / 60)
                }
            })
            .unwrap_or_else(|| "unknown".to_string());

        let _ = writeln!(
            out,
            "\n### Thread #{} ({}, {})",
            sib.thread_id, sib.status, age
        );

        // Files (compact list)
        let files: Vec<String> = serde_json::from_str(&sib.files_touched).unwrap_or_default();
        if !files.is_empty() {
            let files_str = files.join(", ");
            let _ = writeln!(out, "Files: {files_str}");
        }

        // Work description
        if !sib.work_description.is_empty() {
            let _ = writeln!(out, "Work: {}", sib.work_description);
        }
    }

    // Conflict warnings
    if !conflicts.is_empty() {
        out.push_str("\n### CONFLICT WARNING\n");
        for (sib_tid, overlap) in &conflicts {
            let files_str = overlap.join(", ");
            let _ = writeln!(out, "Both you and Thread #{sib_tid} touch: {files_str}");
        }
        out.push_str("Coordinate before editing shared files. Ask the user if unsure.\n");
    }

    // Truncate to budget
    if out.len() > max_chars {
        out.truncate(out.floor_char_boundary(max_chars - 4));
        out.push_str("...\n");
    }

    Some(out)
}

// --- System prompt assembly ---

/// Assemble system prompt from optional parts.
/// P2: filter_map chain, single allocation.
pub fn assemble_system_prompt(
    base: Option<&str>,
    coauthor_block: Option<&str>,
    context_block: Option<&str>,
) -> Option<String> {
    let parts: SmallVec<[&str; 3]> = [base, coauthor_block, context_block]
        .into_iter()
        .flatten()
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("\n\n"))
}

// --- Background summarizer ---

/// Update summaries for all active/idle sessions.
/// Skips sessions with no new activity since last tick (high watermark).
pub async fn update_summaries(pool: &SqlitePool) {
    let sessions = match db::get_active_sessions_for_summary(pool).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "context summarizer: failed to fetch sessions");
            return;
        }
    };

    for (tid_i64, project, status) in &sessions {
        let thread_id = ThreadId::new(*tid_i64 as u64);

        // Get current high watermark
        let watermark = db::get_summary_watermark(pool, thread_id)
            .await
            .unwrap_or(0);

        // Fetch new tool uses since last tick
        let new_tool_uses = match db::get_tool_uses_after(pool, thread_id, watermark).await {
            Ok(tu) => tu,
            Err(e) => {
                tracing::debug!(error = %e, ?thread_id, "context summarizer: failed to fetch tool uses");
                continue;
            }
        };

        // Skip if no new activity (no need to regenerate context)
        if new_tool_uses.is_empty() && watermark > 0 {
            continue;
        }

        // Extract file paths from new tool uses (deduplicated)
        let mut files: Vec<String> = Vec::new();

        // Load existing files from previous summary (P2: collapsed let-chains)
        if let Ok(existing_summary) =
            db::get_sibling_summaries(pool, project, ThreadId::new(0)).await
            && let Some(prev) = existing_summary.iter().find(|s| s.thread_id == *tid_i64)
            && let Ok(prev_files) = serde_json::from_str::<Vec<String>>(&prev.files_touched)
        {
            files = prev_files;
        }

        // Add new files (P2: filter_map + dedup)
        new_tool_uses
            .iter()
            .filter_map(|tu| extract_file_path(&tu.tool, &tu.input_json))
            .for_each(|path| {
                if !files.contains(&path) {
                    files.push(path);
                }
            });

        // Build tool summary (all new tool uses)
        let tools_summary = build_tools_summary(&new_tool_uses);

        // Fetch recent messages for work description
        let recent_msgs = db::get_recent_messages(pool, thread_id, 2)
            .await
            .unwrap_or_default();

        let work_description = build_work_description(&recent_msgs, &tools_summary, files.len());

        let files_json = serde_json::to_string(&files).unwrap_or_else(|_| "[]".to_string());

        let new_watermark = new_tool_uses.last().map(|tu| tu.id).unwrap_or(watermark);

        if let Err(e) = db::upsert_session_summary(
            pool,
            &db::SummaryUpsert {
                thread_id,
                project,
                status,
                files_touched: &files_json,
                tools_summary: &tools_summary,
                work_description: &work_description,
                last_tool_use_id: new_watermark,
            },
        )
        .await
        {
            tracing::warn!(error = %e, ?thread_id, "context summarizer: failed to upsert summary");
        }
    }

    // Mark summaries stale for sessions that are no longer active/idle
    if let Err(e) = mark_stale_summaries(pool).await {
        tracing::debug!(error = %e, "context summarizer: failed to mark stale summaries");
    }
}

/// Mark summaries as stopped/expired when their parent session transitions.
async fn mark_stale_summaries(pool: &SqlitePool) -> Result<(), crate::error::AppError> {
    sqlx::query(
        "UPDATE session_summaries SET status = s.status
         FROM sessions s
         WHERE session_summaries.thread_id = s.thread_id
         AND s.status IN ('stopped', 'expired')
         AND session_summaries.status NOT IN ('stopped', 'expired')",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_file_path_read() {
        let json = r#"{"file_path":"/home/user/project/src/main.rs"}"#;
        let path = extract_file_path("Read", json);
        assert!(path.is_some());
        assert!(path.unwrap().contains("main.rs"));
    }

    #[test]
    fn extract_file_path_edit() {
        let json = r#"{"file_path":"/home/user/project/src/config.rs","old_string":"foo","new_string":"bar"}"#;
        let path = extract_file_path("Edit", json);
        assert!(path.is_some());
        assert!(path.unwrap().contains("config.rs"));
    }

    #[test]
    fn extract_file_path_grep() {
        let json = r#"{"pattern":"fn main","path":"/home/user/project/src"}"#;
        let path = extract_file_path("Grep", json);
        assert!(path.is_some());
    }

    #[test]
    fn extract_file_path_bash_returns_none() {
        let json = r#"{"command":"cargo build"}"#;
        assert!(extract_file_path("Bash", json).is_none());
    }

    #[test]
    fn extract_file_path_invalid_json() {
        assert!(extract_file_path("Read", "not json").is_none());
    }

    #[test]
    fn tools_summary_counts() {
        let uses = vec![
            ContextToolUseRow {
                id: 1,
                tool: "Read".into(),
                input_json: String::new(),
            },
            ContextToolUseRow {
                id: 2,
                tool: "Read".into(),
                input_json: String::new(),
            },
            ContextToolUseRow {
                id: 3,
                tool: "Edit".into(),
                input_json: String::new(),
            },
            ContextToolUseRow {
                id: 4,
                tool: "Bash".into(),
                input_json: String::new(),
            },
        ];
        let summary = build_tools_summary(&uses);
        assert!(summary.contains("Read:2"));
        assert!(summary.contains("Edit:1"));
        assert!(summary.contains("Bash:1"));
    }

    #[test]
    fn work_description_truncation() {
        let long_msg = "x".repeat(200);
        let msgs = vec![("user".to_string(), long_msg)];
        let desc = build_work_description(&msgs, "Read:5", 3);
        assert!(desc.len() < 300);
        assert!(desc.contains("[user]"));
        assert!(desc.contains("3 files"));
    }

    #[test]
    fn assemble_system_prompt_all_parts() {
        let result = assemble_system_prompt(
            Some("base prompt"),
            Some("co-author block"),
            Some("context block"),
        );
        let s = result.unwrap();
        assert!(s.contains("base prompt"));
        assert!(s.contains("co-author block"));
        assert!(s.contains("context block"));
    }

    #[test]
    fn assemble_system_prompt_none() {
        assert!(assemble_system_prompt(None, None, None).is_none());
    }

    #[test]
    fn assemble_system_prompt_partial() {
        let result = assemble_system_prompt(None, Some("coauthor"), None);
        assert_eq!(result.unwrap(), "coauthor");
    }

    #[test]
    fn conflict_detection_overlap() {
        let my_files = vec!["src/config.rs".to_string(), "src/main.rs".to_string()];
        let sibling = SessionSummaryRow {
            thread_id: 123,
            project: "proj".into(),
            status: "active".into(),
            files_touched: r#"["src/config.rs","src/db.rs"]"#.into(),
            tools_summary: String::new(),
            work_description: String::new(),
            last_tool_use_id: 0,
            updated_at: String::new(),
        };
        let conflicts = detect_conflicts(&my_files, &[sibling]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].0, 123);
        assert_eq!(conflicts[0].1, vec!["src/config.rs"]);
    }

    #[test]
    fn conflict_detection_no_overlap() {
        let my_files = vec!["src/main.rs".to_string()];
        let sibling = SessionSummaryRow {
            thread_id: 123,
            project: "proj".into(),
            status: "active".into(),
            files_touched: r#"["src/db.rs"]"#.into(),
            tools_summary: String::new(),
            work_description: String::new(),
            last_tool_use_id: 0,
            updated_at: String::new(),
        };
        let conflicts = detect_conflicts(&my_files, &[sibling]);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn shorten_path_with_src() {
        assert_eq!(
            shorten_path("/home/user/project/src/config.rs"),
            "src/config.rs"
        );
    }

    #[test]
    fn shorten_path_without_src() {
        let result = shorten_path("/a/b/c/d/e.rs");
        assert_eq!(result, "c/d/e.rs");
    }
}
