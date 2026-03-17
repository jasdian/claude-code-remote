use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use uuid::Uuid;

use crate::config::UserIdentity;
use crate::db::ParticipantRow;

// P6: Newtypes prevent mixing up bare u64 IDs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClaudeSessionId(pub Arc<str>);

impl ThreadId {
    #[inline]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl UserId {
    #[inline]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl ClaudeSessionId {
    pub fn new(s: &str) -> Self {
        Self(Arc::from(s))
    }
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<serenity::ChannelId> for ThreadId {
    fn from(id: serenity::ChannelId) -> Self {
        Self(id.get())
    }
}

impl From<serenity::UserId> for UserId {
    fn from(id: serenity::UserId) -> Self {
        Self(id.get())
    }
}

// Session: hot/cold field separation (P5)

#[derive(Debug, Clone)]
pub struct Session {
    pub id: Uuid,
    pub thread_id: ThreadId,
    pub owner_id: UserId,
    // Hot path fields first (P5: locality)
    pub status: SessionStatus,
    pub last_active_at: DateTime<Utc>,
    // Cold path fields
    pub claude_session_id: Option<ClaudeSessionId>,
    pub project: Arc<str>,
    pub created_at: DateTime<Utc>,
    pub worktree_path: Option<Arc<str>>,
}

// Multi-user support types

/// User-attributed message for pending queue (replaces bare String).
#[derive(Debug, Clone)]
pub struct UserMessage {
    pub user_id: UserId,
    pub username: Arc<str>,
    pub content: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Active,
    Idle,
    Stopped,
    Expired,
}

impl SessionStatus {
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Stopped => "stopped",
            Self::Expired => "expired",
        }
    }
}

impl From<&str> for SessionStatus {
    #[inline]
    fn from(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "idle" => Self::Idle,
            "stopped" => Self::Stopped,
            "expired" => Self::Expired,
            _ => Self::Expired,
        }
    }
}

impl AsRef<str> for SessionStatus {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// P3: Boxed payload for ControlRequest — keeps ClaudeEvent enum compact.
#[derive(Debug, Clone)]
pub struct ControlRequestData {
    pub request_id: Arc<str>,
    pub tool_name: Arc<str>,
    /// Human-readable question or tool description for display.
    pub question: Arc<str>,
    /// Full input JSON for audit logging.
    pub input_json: Arc<str>,
}

// Co-authored commit support

/// Format a single `Co-authored-by` trailer per GitHub spec.
/// Prefers email, falls back to GitHub noreply address, returns None if neither set.
pub fn format_co_author(identity: &UserIdentity, discord_name: &str) -> Option<String> {
    // Sanitize: strip control chars and newlines to prevent prompt injection via display names
    let safe_name: String = discord_name.chars().filter(|c| !c.is_control()).collect();

    if let Some(ref email) = identity.email {
        Some(format!("Co-authored-by: {safe_name} <{email}>"))
    } else {
        identity
            .github_username
            .as_ref()
            .map(|github| format!("Co-authored-by: {github} <{github}@users.noreply.github.com>"))
    }
}

/// Build a system prompt block with Co-authored-by trailers for all mapped participants.
/// Returns None for solo sessions or when no participants have identities mapped.
pub fn build_coauthor_prompt(
    participants: &[ParticipantRow],
    identities: &HashMap<u64, UserIdentity>,
) -> Option<String> {
    // P2: early return — solo sessions never need trailers, skip allocation
    if participants.len() < 2 {
        return None;
    }

    let lines: Vec<String> = participants
        .iter()
        .filter_map(|p| {
            let identity = identities.get(&p.user_id)?;
            format_co_author(identity, &p.username)
        })
        .collect();

    if lines.len() < 2 {
        return None;
    }

    Some(format!(
        "When making git commits in this session, append these Co-authored-by trailers:\n\n{}",
        lines.join("\n")
    ))
}

/// Shell script for `prepare-commit-msg` hook that appends co-author trailers.
/// Reads `.claude-coauthors` from the repo root at commit time.
/// Deduplicates (skips lines already present) and only runs for normal commits.
/// P6: compile-time constant.
pub const PREPARE_COMMIT_MSG_HOOK: &str = r#"#!/bin/sh
# Auto-generated by claude-crew for co-authored commits
COAUTHORS_FILE="$(git rev-parse --show-toplevel)/.claude-coauthors"
[ -f "$COAUTHORS_FILE" ] || exit 0
COMMIT_SOURCE="$2"
[ -n "$COMMIT_SOURCE" ] && [ "$COMMIT_SOURCE" != "message" ] && exit 0
while IFS= read -r line; do
    [ -n "$line" ] && ! grep -qF "$line" "$1" && printf '\n%s' "$line" >> "$1"
done < "$COAUTHORS_FILE"
"#;

/// Build the contents of a `.claude-coauthors` file for the prepare-commit-msg hook.
/// Returns just the trailer lines (no instruction text), or None for solo/unmapped sessions.
pub fn build_coauthors_file_content(
    participants: &[ParticipantRow],
    identities: &HashMap<u64, UserIdentity>,
) -> Option<String> {
    if participants.len() < 2 {
        return None;
    }

    let lines: Vec<String> = participants
        .iter()
        .filter_map(|p| {
            let identity = identities.get(&p.user_id)?;
            format_co_author(identity, &p.username)
        })
        .collect();

    if lines.len() < 2 {
        return None;
    }

    Some(lines.join("\n"))
}

// P3: Compact enum for Claude process exit classification.
// Box<str> for messages keeps variant size small.

#[derive(Debug, Clone)]
pub enum ClaudeExitReason {
    /// Clean exit (code 0).
    Success,
    /// Binary not found in PATH.
    NotFound,
    /// Auth/token errors detected in stderr.
    AuthFailure(Box<str>),
    /// Rate limiting / overload detected in stderr.
    RateLimited(Box<str>),
    /// Non-zero exit with captured stderr.
    Crashed(i32, Box<str>),
    /// Fallback when exit code is unavailable (signal death, etc).
    Unknown(Box<str>),
}

impl ClaudeExitReason {
    /// Classify a process exit from IO error, exit code, and stderr output.
    #[inline]
    pub fn classify(io_err: Option<&std::io::Error>, exit_code: Option<i32>, stderr: &str) -> Self {
        if let Some(e) = io_err
            && e.kind() == std::io::ErrorKind::NotFound
        {
            return Self::NotFound;
        }

        let stderr_lower = stderr.to_ascii_lowercase();

        if stderr_lower.contains("rate limit")
            || stderr_lower.contains("429")
            || stderr_lower.contains("overloaded")
        {
            return Self::RateLimited(extract_meaningful_stderr(stderr).into());
        }

        if stderr_lower.contains("unauthorized")
            || stderr_lower.contains("invalid api key")
            || (stderr_lower.contains("auth")
                && (stderr_lower.contains("fail") || stderr_lower.contains("error")))
        {
            return Self::AuthFailure(extract_meaningful_stderr(stderr).into());
        }

        match exit_code {
            Some(0) => Self::Success,
            Some(code) => Self::Crashed(code, extract_meaningful_stderr(stderr).into()),
            None => Self::Unknown(extract_meaningful_stderr(stderr).into()),
        }
    }

    /// Format a user-friendly Discord message for this exit reason.
    pub fn user_message(&self) -> Option<String> {
        match self {
            Self::Success => None,
            Self::NotFound => Some(
                "**Error:** Claude CLI not found in PATH. Check the `binary` setting in your config.".into()
            ),
            Self::AuthFailure(detail) => Some(format!(
                "**Error:** Claude authentication failed. Run `claude` manually to re-authenticate.\n```\n{detail}\n```"
            )),
            Self::RateLimited(detail) => Some(format!(
                "**Rate limited:** Claude is overloaded. Try again in a minute.\n```\n{detail}\n```"
            )),
            Self::Crashed(code, detail) => Some(format!(
                "**Error:** Claude exited with code {code}.\n```\n{detail}\n```"
            )),
            Self::Unknown(detail) => Some(format!(
                "**Error:** Claude process failed unexpectedly.\n```\n{detail}\n```"
            )),
        }
    }
}

/// Extract the last meaningful line from stderr, skipping stack traces and blank lines.
fn extract_meaningful_stderr(stderr: &str) -> &str {
    stderr
        .lines()
        .rev()
        .find(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty()
                && !trimmed.starts_with("at ")
                && !trimmed.starts_with("node:")
                && !trimmed.contains("Object.<anonymous>")
        })
        .unwrap_or(stderr.lines().next().unwrap_or(stderr))
}

// ClaudeEvent: compact enum (P3)

#[derive(Debug, Clone)]
pub enum ClaudeEvent {
    TextDelta(Arc<str>),
    ToolUse {
        tool: Arc<str>,
        input_preview: Arc<str>,
        input_json: Arc<str>,
    },
    ToolResult {
        tool: Arc<str>,
        is_error: bool,
        output_preview: Arc<str>,
    },
    /// Claude CLI `control_request` — permission prompt or AskUserQuestion.
    /// P3: Boxed to keep enum size compact (ControlRequest is rare vs TextDelta).
    ControlRequest(Box<ControlRequestData>),
    SessionId(ClaudeSessionId),
    /// Process exited with classified reason. Sent before Done.
    ExitError(ClaudeExitReason),
    Done,
    Error(Box<str>),
}
