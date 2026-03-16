# Implementation Plan (Redesigned)

## Principles

Every module in this project adheres to six rules. Violations are bugs.

### P1: Borrow/Consume Flow
- Pass `&Config`, `&str`, `&[T]` on every call path. Never clone a config struct.
- Use `Cow<'a, str>` where a function *usually* borrows but *sometimes* must own (e.g. tool name resolution with defaults).
- Use `Arc<str>` for strings shared across tasks (tokens, binary path, session IDs). Never `Arc<String>`.
- Use `&'a [T]` slices on hot paths. Allocate `Vec` only at construction time, pass slices thereafter.

### P2: Functional Patterns
- `fold`/`scan` for single-pass accumulation (stream event parsing, text chunking).
- `filter_map` chains to eliminate intermediate `Vec` allocations.
- `and_then()` / `ok_or_else()` for nested optionals with rich error context.
- Closures capture by reference on hot paths. No `move` unless sending across tasks.
- Early returns with `?` and contextual errors. No nested if-else pyramids.

### P3: Memory Efficiency
- `SmallVec<[T; N]>` where N is the typical case (tool lists: N=8, role lists: N=4).
- Fixed-size arrays on stack where sizes are compile-time known.
- Buffer reuse: `.clear()` retains capacity. The formatter pre-allocates once (2048 bytes) and reuses.
- `Arc<[T]>` thin slices instead of `Arc<Vec<T>>`.
- Compact enums: largest variant dictates size. Use `Box<str>` for error messages, `Arc<str>` for shared event data.

### P4: Async-Only IO
- ALL IO via tokio: `tokio::fs`, `tokio::process`, `tokio::net`, sqlx async.
- `spawn_blocking` for CPU-bound work (large JSON parsing).
- Never: `std::fs`, `std::thread::sleep`, synchronous `std::process::Command`.
- Channels: `mpsc` for producer-consumer (Claude events), `watch` for config/state broadcast, `oneshot` for request-response.
- `CancellationToken` for graceful shutdown. Every background task receives one.

### P5: CPU Locality
- Hot/cold field separation: `SessionHot` (status, last_active) separate from `SessionCold` (created_at, project name).
- `#[inline]` on small hot-path functions (parser match arms, event dispatch).
- Monomorphization via generic trait bounds. No `dyn Trait` on hot paths.
- Keep structs that are iterated together adjacent in memory (AoS for sessions).

### P6: Type-Driven Design
- Newtypes: `ThreadId(u64)`, `UserId(u64)`, `ClaudeSessionId(Arc<str>)` -- prevent mixing up bare `u64`s.
- Builder pattern for complex configs with `Cow` fields.
- Trait-based polymorphism with compile-time dispatch: `trait EventSink` for Discord output abstraction.
- `const fn` for compile-time constants (defaults, static config values).

---

## Phase 1: Scaffold

### 1.1 Initialize project

```bash
cargo init
```

### 1.2 Cargo.toml

```toml
[package]
name = "claude-remote-chat"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1", features = ["full"] }
tokio-util = { version = "0.7", features = ["rt"] }  # CancellationToken
poise = "0.6"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.9"
sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite"] }
uuid = { version = "1", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
thiserror = "2"
smallvec = { version = "1", features = ["serde"] }
dashmap = "6"

[profile.release]
lto = true
codegen-units = 1
strip = true
```

### 1.3 shell.nix

(Keep existing shell.nix as-is -- it already handles musl cross-compilation.)

### 1.4 config.example.toml

```toml
[discord]
token = "YOUR_BOT_TOKEN"
guild_id = 123456789012345678

[claude]
binary = "claude"
default_cwd = "/home/you/projects"
allowed_tools = ["Bash", "Read", "Write", "Edit", "Glob", "Grep"]
max_sessions = 3
session_timeout_minutes = 30
# system_prompt = "You are helping via Discord. Keep responses concise."

[claude.projects.myapp]
cwd = "/home/you/projects/myapp"
# allowed_tools = ["Read", "Grep"]

[auth]
allowed_users = [123456789012345678]
allowed_roles = []

[database]
url = "sqlite:data.db?mode=rwc"

[logging]
level = "info"
format = "pretty"
```

### 1.5 Database migration

Create `migrations/001_initial.sql`:

```sql
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    thread_id INTEGER NOT NULL UNIQUE,
    user_id INTEGER NOT NULL,
    claude_session_id TEXT,
    project TEXT,
    status TEXT NOT NULL DEFAULT 'idle',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_active_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_sessions_thread ON sessions(thread_id);
CREATE INDEX idx_sessions_status ON sessions(status);
```

### 1.6 Verify

```bash
cargo build
```

---

## Phase 2: Core Types & Config

### 2.1 `src/domain.rs` -- Newtypes, Compact Enums, Zero-Copy Events

```rust
use std::sync::Arc;
use chrono::{DateTime, Utc};
use uuid::Uuid;

// ── P6: Newtypes prevent mixing up bare u64 IDs ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClaudeSessionId(pub Arc<str>);  // P1: Arc<str> not Arc<String>

impl ThreadId {
    #[inline]
    pub const fn new(id: u64) -> Self { Self(id) }
    #[inline]
    pub const fn get(self) -> u64 { self.0 }
}

impl UserId {
    #[inline]
    pub const fn new(id: u64) -> Self { Self(id) }
    #[inline]
    pub const fn get(self) -> u64 { self.0 }
}

impl ClaudeSessionId {
    pub fn new(s: &str) -> Self { Self(Arc::from(s)) }
    #[inline]
    pub fn as_str(&self) -> &str { &self.0 }
}

// From conversions for ergonomic use with poise/serenity types
impl From<serenity::model::id::ChannelId> for ThreadId {
    fn from(id: serenity::model::id::ChannelId) -> Self { Self(id.get()) }
}

impl From<serenity::model::id::UserId> for UserId {
    fn from(id: serenity::model::id::UserId) -> Self { Self(id.get()) }
}

// ── Session: hot/cold field separation (P5) ──

/// Hot fields: accessed on every message dispatch. Kept compact.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: Uuid,
    pub thread_id: ThreadId,
    pub user_id: UserId,
    // Hot path fields first (P5: locality)
    pub status: SessionStatus,
    pub last_active_at: DateTime<Utc>,
    // Cold path fields (rarely accessed during message dispatch)
    pub claude_session_id: Option<ClaudeSessionId>,
    pub project: Option<Arc<str>>,  // P1: shared immutable string
    pub created_at: DateTime<Utc>,
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

    #[inline]
    pub fn from_str(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "idle" => Self::Idle,
            "stopped" => Self::Stopped,
            _ => Self::Expired,
        }
    }
}

// ── ClaudeEvent: compact enum, no large String variants (P3) ──
// Arc<str> for text that gets shared across formatter + session state.
// Box<str> for error messages (owned, not shared).

#[derive(Debug, Clone)]
pub enum ClaudeEvent {
    /// HOT PATH: most frequent event. Arc<str> allows zero-copy sharing
    /// with the formatter without cloning the underlying text.
    TextDelta(Arc<str>),
    ToolUse {
        tool: Arc<str>,       // Tool names are repeated often; Arc avoids duplication
        input_preview: Arc<str>,
    },
    ToolResult {
        tool: Arc<str>,
        is_error: bool,
    },
    SessionId(ClaudeSessionId),
    Done,
    Error(Box<str>),  // P3: Box<str> is 1 word smaller than String (no capacity field)
}
```

### 2.2 `src/error.rs` -- Compact Variants

```rust
/// P3: All string-carrying variants use Box<str> to keep the enum compact.
/// Box<str> is 16 bytes (ptr + len). String is 24 bytes (ptr + len + capacity).
/// The enum's size is determined by its largest variant.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),

    #[error("config: {0}")]
    Config(Box<str>),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("discord: {0}")]
    Discord(#[from] serenity::Error),

    #[error("claude process: {0}")]
    Claude(Box<str>),

    #[error("session not found: thread {0}")]
    SessionNotFound(ThreadId),

    #[error("unauthorized: {0}")]
    Unauthorized(Box<str>),

    #[error("max sessions reached ({0})")]
    MaxSessions(usize),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Convenience: convert &str to Box<str> for error construction
impl AppError {
    #[inline]
    pub fn config(msg: &str) -> Self { Self::Config(msg.into()) }
    #[inline]
    pub fn claude(msg: &str) -> Self { Self::Claude(msg.into()) }
    #[inline]
    pub fn unauthorized(msg: &str) -> Self { Self::Unauthorized(msg.into()) }
}
```

### 2.3 `src/config.rs` -- Arc<str>, SmallVec, Builder

```rust
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use serde::Deserialize;
use smallvec::SmallVec;

// ── Raw TOML deserialization target (String-based for serde compat) ──

#[derive(Debug, Deserialize)]
struct RawAppConfig {
    discord: RawDiscordConfig,
    claude: RawClaudeConfig,
    database: RawDatabaseConfig,
    auth: RawAuthConfig,
    #[serde(default)]
    logging: RawLoggingConfig,
}

#[derive(Debug, Deserialize)]
struct RawDiscordConfig {
    token: String,
    guild_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawClaudeConfig {
    #[serde(default = "default_binary")]
    binary: String,
    default_cwd: String,
    #[serde(default)]
    projects: HashMap<String, RawProjectConfig>,
    #[serde(default = "default_allowed_tools")]
    allowed_tools: Vec<String>,
    #[serde(default = "default_max_sessions")]
    max_sessions: usize,
    #[serde(default = "default_timeout")]
    session_timeout_minutes: u64,
    system_prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawProjectConfig {
    cwd: String,
    allowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawAuthConfig {
    allowed_users: Vec<u64>,
    #[serde(default)]
    allowed_roles: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct RawDatabaseConfig {
    url: String,
}

#[derive(Debug, Deserialize)]
struct RawLoggingConfig {
    #[serde(default = "default_level")]
    level: String,
    #[serde(default = "default_format")]
    format: String,
}

impl Default for RawLoggingConfig {
    fn default() -> Self {
        Self { level: default_level(), format: default_format() }
    }
}

fn default_binary() -> String { "claude".into() }
fn default_allowed_tools() -> Vec<String> {
    ["Bash", "Read", "Write", "Edit", "Glob", "Grep"]
        .iter().map(|s| s.to_string()).collect()
}
const fn default_max_sessions() -> usize { 3 }
const fn default_timeout() -> u64 { 30 }
fn default_level() -> String { "info".into() }
fn default_format() -> String { "pretty".into() }

// ── Validated, Arc<str>-backed config (P1: shared immutable, never cloned) ──

#[derive(Debug)]
pub struct AppConfig {
    pub discord: DiscordConfig,
    pub claude: ClaudeConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug)]
pub struct DiscordConfig {
    pub token: Arc<str>,    // P1: shared across serenity client
    pub guild_id: Option<u64>,
}

#[derive(Debug)]
pub struct ClaudeConfig {
    pub binary: Arc<str>,   // P1: passed to every process spawn, never changes
    pub default_cwd: Arc<str>,
    pub projects: HashMap<Arc<str>, ProjectConfig>,
    pub allowed_tools: SmallVec<[Arc<str>; 8]>,  // P3: typically 6 tools, inline on stack
    pub max_sessions: usize,
    pub session_timeout_minutes: u64,
    pub system_prompt: Option<Arc<str>>,
}

#[derive(Debug)]
pub struct ProjectConfig {
    pub cwd: Arc<str>,
    pub allowed_tools: Option<SmallVec<[Arc<str>; 8]>>,
}

#[derive(Debug)]
pub struct AuthConfig {
    pub allowed_users: SmallVec<[u64; 4]>,    // P3: typically 1-3 users
    pub allowed_roles: SmallVec<[u64; 4]>,
}

#[derive(Debug)]
pub struct DatabaseConfig {
    pub url: Arc<str>,
}

#[derive(Debug)]
pub struct LoggingConfig {
    pub level: Arc<str>,
    pub format: Arc<str>,
}

impl ClaudeConfig {
    /// P1: Resolve tools for a project. Returns borrowed slice when using defaults,
    /// owned SmallVec only when project overrides exist.
    pub fn resolve_tools<'a>(&'a self, project: Option<&str>) -> Cow<'a, [Arc<str>]> {
        project
            .and_then(|p| self.projects.get(p))
            .and_then(|pc| pc.allowed_tools.as_ref())
            .map(|tools| Cow::Owned(tools.to_vec()))
            .unwrap_or(Cow::Borrowed(self.allowed_tools.as_slice()))
    }

    /// P1: Resolve cwd. Returns &str reference, never allocates.
    pub fn resolve_cwd<'a>(&'a self, project: Option<&str>) -> &'a str {
        project
            .and_then(|p| self.projects.get(p))
            .map(|pc| pc.cwd.as_ref())
            .unwrap_or(&self.default_cwd)
    }
}

// ── Builder: parse raw TOML -> validated config ──

impl AppConfig {
    /// P4: reads config file via tokio::fs
    pub async fn from_file(path: &str) -> Result<Self, crate::error::AppError> {
        let content = tokio::fs::read_to_string(path).await
            .map_err(|e| crate::error::AppError::config(
                &format!("reading {path}: {e}")
            ))?;
        Self::from_str(&content)
    }

    pub fn from_str(content: &str) -> Result<Self, crate::error::AppError> {
        let raw: RawAppConfig = toml::from_str(content)
            .map_err(|e| crate::error::AppError::config(&e.to_string()))?;

        Ok(AppConfig {
            discord: DiscordConfig {
                token: Arc::from(raw.discord.token.as_str()),
                guild_id: raw.discord.guild_id,
            },
            claude: ClaudeConfig {
                binary: Arc::from(raw.claude.binary.as_str()),
                default_cwd: Arc::from(raw.claude.default_cwd.as_str()),
                projects: raw.claude.projects.into_iter().map(|(k, v)| {
                    let pc = ProjectConfig {
                        cwd: Arc::from(v.cwd.as_str()),
                        allowed_tools: v.allowed_tools.map(|tools| {
                            tools.iter().map(|s| Arc::from(s.as_str())).collect()
                        }),
                    };
                    (Arc::from(k.as_str()), pc)
                }).collect(),
                allowed_tools: raw.claude.allowed_tools.iter()
                    .map(|s| Arc::from(s.as_str())).collect(),
                max_sessions: raw.claude.max_sessions,
                session_timeout_minutes: raw.claude.session_timeout_minutes,
                system_prompt: raw.claude.system_prompt.map(|s| Arc::from(s.as_str())),
            },
            database: DatabaseConfig {
                url: Arc::from(raw.database.url.as_str()),
            },
            auth: AuthConfig {
                allowed_users: raw.auth.allowed_users.into_iter().collect(),
                allowed_roles: raw.auth.allowed_roles.into_iter().collect(),
            },
            logging: LoggingConfig {
                level: Arc::from(raw.logging.level.as_str()),
                format: Arc::from(raw.logging.format.as_str()),
            },
        })
    }
}
```

---

## Phase 3: Claude Process Management

### 3.1 `src/claude/parser.rs` -- Zero-Copy Hot Path

```rust
use std::sync::Arc;
use crate::domain::{ClaudeEvent, ClaudeSessionId};

/// HOT PATH: called for every line of Claude stdout.
///
/// P1: Works on &str borrowing. serde_json::from_str with Value borrows
/// string data from the input when accessing via .as_str().
/// We convert to Arc<str> only for TextDelta (which must outlive the line).
///
/// P5: #[inline] for the dispatch function — it's small and called per-line.
#[inline]
pub fn parse_stream_line(line: &str) -> Option<ClaudeEvent> {
    // P2: Early return chain, no nested if-else
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = v.get("type")?.as_str()?;

    match event_type {
        "system" => parse_system(&v),
        "content_block_delta" => parse_delta(&v),
        "content_block_start" => parse_block_start(&v),
        "result" => parse_result(&v),
        _ => {
            tracing::debug!(event_type, "unknown stream event");
            None
        }
    }
}

#[inline]
fn parse_system(v: &serde_json::Value) -> Option<ClaudeEvent> {
    // P2: and_then chain for nested optional access
    v.get("session_id")
        .and_then(|s| s.as_str())
        .map(|sid| ClaudeEvent::SessionId(ClaudeSessionId::new(sid)))
}

/// HOT PATH: TextDelta is the most frequent event during streaming.
#[inline]
fn parse_delta(v: &serde_json::Value) -> Option<ClaudeEvent> {
    let delta = v.get("delta")?;
    // P2: filter via and_then — no intermediate allocation
    let delta_type = delta.get("type")?.as_str()?;
    if delta_type != "text_delta" {
        return None;
    }
    delta.get("text")
        .and_then(|t| t.as_str())
        .map(|text| ClaudeEvent::TextDelta(Arc::from(text)))  // P1: Arc<str> from borrowed &str
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
        "tool_result" => {
            let tool = block.get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("unknown");
            let is_error = block.get("is_error")
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            Some(ClaudeEvent::ToolResult {
                tool: Arc::from(tool),
                is_error,
            })
        }
        _ => None,
    }
}

#[inline]
fn parse_result(v: &serde_json::Value) -> Option<ClaudeEvent> {
    // Capture session_id from result if present, then emit Done
    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
        // The caller should handle this: SessionId followed by Done.
        // For simplicity, we emit Done here; session_id from "system" init is primary.
        // If we need both, the process.rs reader can check for session_id in result events.
    }
    Some(ClaudeEvent::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_delta() {
        let line = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}"#;
        match parse_stream_line(line) {
            Some(ClaudeEvent::TextDelta(t)) => assert_eq!(&*t, "hello"),
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
    fn parse_tool_use() {
        let line = r#"{"type":"content_block_start","content_block":{"type":"tool_use","name":"Bash"}}"#;
        match parse_stream_line(line) {
            Some(ClaudeEvent::ToolUse { tool, .. }) => assert_eq!(&*tool, "Bash"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_result_done() {
        let line = r#"{"type":"result","result":"done","session_id":"abc-123"}"#;
        assert!(matches!(parse_stream_line(line), Some(ClaudeEvent::Done)));
    }

    #[test]
    fn parse_unknown_skipped() {
        let line = r#"{"type":"unknown_future_event"}"#;
        assert!(parse_stream_line(line).is_none());
    }

    #[test]
    fn parse_garbage_skipped() {
        assert!(parse_stream_line("not json at all").is_none());
        assert!(parse_stream_line("").is_none());
    }
}
```

### 3.2 `src/claude/process.rs` -- CancellationToken, Pre-allocated Buffer, Bounded Channel

```rust
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::ClaudeConfig;
use crate::domain::ClaudeEvent;
use crate::error::AppError;

/// Pre-allocated BufReader capacity for stdout line reading.
/// Claude output lines (JSON events) are typically 100-500 bytes,
/// occasionally up to ~4KB for large text deltas.
const STDOUT_BUF_CAPACITY: usize = 8 * 1024;

/// Bounded channel capacity. Provides backpressure if Discord sending
/// falls behind Claude output rate.
const EVENT_CHANNEL_CAPACITY: usize = 256;

pub struct ClaudeProcessHandle {
    child: Child,
    reader_task: JoinHandle<()>,
    cancel: CancellationToken,  // P4: clean shutdown
}

impl ClaudeProcessHandle {
    /// Graceful kill: signal cancellation, then force-kill the child process.
    pub async fn kill(mut self) -> Result<(), AppError> {
        self.cancel.cancel();
        self.child.kill().await?;
        // reader_task will exit on its own via cancellation or stdout EOF,
        // but abort as safety net
        self.reader_task.abort();
        Ok(())
    }

    /// Check if the process is still running.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// Create a bounded event channel with backpressure.
pub fn event_channel() -> (mpsc::Sender<ClaudeEvent>, mpsc::Receiver<ClaudeEvent>) {
    mpsc::channel(EVENT_CHANNEL_CAPACITY)
}

/// P4: All IO is async. P1: borrows config and prompt, never clones.
/// P5: #[inline] omitted here — function is large, not a hot-path per-call.
pub async fn run_claude(
    config: &ClaudeConfig,       // P1: borrow, never clone
    prompt: &str,                // P1: borrow
    session_id: Option<&str>,    // P1: borrow
    cwd: &Path,                  // P1: borrow
    allowed_tools: &[Arc<str>],  // P1: slice reference
    event_tx: mpsc::Sender<ClaudeEvent>,
    cancel: CancellationToken,   // P4: propagated from parent
) -> Result<ClaudeProcessHandle, AppError> {
    let mut cmd = Command::new(config.binary.as_ref());
    cmd.arg("-p").arg(prompt)
       .arg("--output-format").arg("stream-json")
       .arg("--verbose");

    // P2: filter_map to build tool string without intermediate Vec
    if !allowed_tools.is_empty() {
        let tools_str: String = allowed_tools.iter()
            .map(|t| t.as_ref())
            .collect::<Vec<_>>()
            .join(",");
        cmd.arg("--allowedTools").arg(tools_str);
    }

    if let Some(sid) = session_id {
        cmd.arg("--resume").arg(sid);
    }

    if let Some(ref sys_prompt) = config.system_prompt {
        cmd.arg("--append-system-prompt").arg(sys_prompt.as_ref());
    }

    cmd.current_dir(cwd)
       .stdout(Stdio::piped())
       .stderr(Stdio::piped())
       .kill_on_drop(true);  // Safety: kill child if handle is dropped

    let mut child = cmd.spawn().map_err(|e| {
        AppError::claude(&format!("failed to spawn claude: {e}"))
    })?;

    let stdout = child.stdout.take()
        .ok_or_else(|| AppError::claude("no stdout from claude process"))?;

    let reader_cancel = cancel.clone();
    let reader_task = tokio::spawn(async move {
        // P3: Pre-allocated buffer with known capacity
        let reader = BufReader::with_capacity(STDOUT_BUF_CAPACITY, stdout);
        let mut lines = reader.lines();

        // P4: select! on cancellation vs. next line
        loop {
            tokio::select! {
                _ = reader_cancel.cancelled() => {
                    tracing::debug!("claude reader cancelled");
                    break;
                }
                line_result = lines.next_line() => {
                    match line_result {
                        Ok(Some(line)) => {
                            // HOT PATH: parse each line, send if meaningful
                            if let Some(event) = super::parser::parse_stream_line(&line) {
                                if event_tx.send(event).await.is_err() {
                                    break; // receiver dropped
                                }
                            }
                        }
                        Ok(None) => break, // EOF
                        Err(e) => {
                            tracing::warn!(error = %e, "claude stdout read error");
                            let _ = event_tx.send(ClaudeEvent::Error(
                                format!("stdout read: {e}").into_boxed_str()
                            )).await;
                            break;
                        }
                    }
                }
            }
        }
        // Always signal completion
        let _ = event_tx.send(ClaudeEvent::Done).await;
    });

    Ok(ClaudeProcessHandle { child, reader_task, cancel })
}
```

### 3.3 `src/claude/session.rs` -- DashMap for Lock-Free Access

```rust
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::Instant;
use dashmap::DashMap;
use tokio_util::sync::CancellationToken;

use crate::config::ClaudeConfig;
use crate::domain::{ClaudeSessionId, ThreadId};
use crate::error::AppError;

use super::process::ClaudeProcessHandle;

/// Active session entry. Fields ordered by access frequency (P5: locality).
struct ActiveSession {
    handle: ClaudeProcessHandle,
    started_at: Instant,                      // Hot: checked by reaper
    claude_session_id: Option<ClaudeSessionId>, // Hot: updated on first event
    project_cwd: PathBuf,                     // Cold: only used at spawn time
}

/// P4: DashMap provides lock-free concurrent reads and fine-grained write locking.
/// Justification: multiple Discord event handler tasks may check has_session()
/// concurrently while the reaper task removes expired sessions. DashMap avoids
/// a single RwLock bottleneck. For 3 max sessions the perf difference is negligible,
/// but DashMap is also simpler to use (no .read().await / .write().await ceremony).
pub struct SessionManager {
    active: DashMap<ThreadId, ActiveSession>,
    config: Arc<ClaudeConfig>,  // P1: borrowed via Arc, never cloned
}

impl SessionManager {
    pub fn new(config: Arc<ClaudeConfig>) -> Self {
        Self {
            active: DashMap::with_capacity(config.max_sessions),
            config,
        }
    }

    #[inline]
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    #[inline]
    pub fn has_session(&self, thread_id: ThreadId) -> bool {
        self.active.contains_key(&thread_id)
    }

    pub fn register(
        &self,
        thread_id: ThreadId,
        handle: ClaudeProcessHandle,
        cwd: PathBuf,
    ) -> Result<(), AppError> {
        // Check count before inserting (slight TOCTOU race is acceptable:
        // worst case we get max_sessions+1 briefly)
        if self.active.len() >= self.config.max_sessions {
            // Cannot register; spawn a task to kill the handle
            tokio::spawn(async move {
                let _ = handle.kill().await;
            });
            return Err(AppError::MaxSessions(self.config.max_sessions));
        }
        self.active.insert(thread_id, ActiveSession {
            handle,
            started_at: Instant::now(),
            claude_session_id: None,
            project_cwd: cwd,
        });
        Ok(())
    }

    pub fn set_session_id(&self, thread_id: ThreadId, sid: ClaudeSessionId) {
        if let Some(mut entry) = self.active.get_mut(&thread_id) {
            entry.claude_session_id = Some(sid);
        }
    }

    pub fn get_session_id(&self, thread_id: ThreadId) -> Option<ClaudeSessionId> {
        self.active.get(&thread_id)
            .and_then(|entry| entry.claude_session_id.clone())
    }

    /// Remove and return the handle for explicit killing.
    pub fn remove(&self, thread_id: ThreadId) -> Option<ClaudeProcessHandle> {
        self.active.remove(&thread_id).map(|(_, s)| s.handle)
    }

    /// Kill sessions older than timeout. Returns number killed.
    /// P2: functional filter_map + fold pattern for single-pass collection.
    pub async fn reap_expired(&self) -> usize {
        let timeout = std::time::Duration::from_secs(
            self.config.session_timeout_minutes * 60
        );

        // Collect expired keys first (cannot hold DashMap ref across await)
        let expired: SmallVec<[ThreadId; 4]> = self.active.iter()
            .filter(|entry| entry.started_at.elapsed() > timeout)
            .map(|entry| *entry.key())
            .collect();

        let count = expired.len();
        for tid in expired {
            if let Some((_, session)) = self.active.remove(&tid) {
                let _ = session.handle.kill().await;
                tracing::info!(?tid, "reaped expired session");
            }
        }
        count
    }
}
```

### 3.4 `src/claude/mod.rs`

```rust
pub mod parser;
pub mod process;
pub mod session;
```

### 3.5 Verify Phase 2-3

```rust
// Temporary test in main.rs — validates process spawning and event parsing
#[tokio::main]
async fn main() {
    let config = AppConfig::from_file("config.toml").await.unwrap();
    let config = Arc::new(config);

    let (tx, mut rx) = claude::process::event_channel();
    let cancel = CancellationToken::new();

    let _handle = claude::process::run_claude(
        &config.claude,
        "say hello",
        None,
        Path::new(&*config.claude.default_cwd),
        &config.claude.allowed_tools,
        tx,
        cancel,
    ).await.unwrap();

    while let Some(event) = rx.recv().await {
        println!("{event:?}");
        if matches!(event, ClaudeEvent::Done) { break; }
    }
}
```

---

## Phase 4: Discord Bot

### 4.1 `src/lib.rs` -- AppState with CancellationToken

```rust
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub mod config;
pub mod error;
pub mod domain;
pub mod db;
pub mod claude;
pub mod discord;

/// Application state shared across all tasks.
/// P1: Config behind Arc — never cloned, always borrowed via &.
/// P4: CancellationToken for graceful shutdown propagation.
///
/// Note: SessionManager is internally concurrent (DashMap), so it does NOT
/// need Arc wrapping. AppState itself is wrapped in Arc for poise.
pub struct AppState {
    pub config: Arc<config::AppConfig>,
    pub db: sqlx::SqlitePool,
    pub session_manager: claude::session::SessionManager,
    pub shutdown: CancellationToken,
}

/// poise context type alias
pub type Context<'a> = poise::Context<'a, Arc<AppState>, error::AppError>;
```

### 4.2 `src/discord/commands.rs`

```rust
use std::path::Path;
use std::sync::Arc;
use crate::{Context, error::AppError, domain::{ThreadId, UserId}};

/// Auth check using SmallVec-backed allowed_users.
/// P2: early return pattern.
#[inline]
fn check_auth(ctx: &Context<'_>) -> Result<(), AppError> {
    let user_id = ctx.author().id.get();
    let auth = &ctx.data().config.auth;

    // P2: any() short-circuits on first match
    if auth.allowed_users.iter().any(|&id| id == user_id) {
        return Ok(());
    }

    // Check roles if member info available
    if let Some(member) = ctx.author_member() {
        let has_role = member.roles.iter().any(|role| {
            auth.allowed_roles.iter().any(|&allowed| allowed == role.get())
        });
        if has_role {
            return Ok(());
        }
    }

    Err(AppError::unauthorized("not in allowed_users or allowed_roles"))
}

/// Start a new Claude Code conversation.
#[poise::command(slash_command)]
pub async fn claude(
    ctx: Context<'_>,
    #[description = "Your prompt for Claude"] prompt: String,
    #[description = "Project name or path"] project: Option<String>,
) -> Result<(), AppError> {
    check_auth(&ctx)?;

    let state = ctx.data();
    let config = &state.config.claude;

    // P1: borrow resolution, no allocation
    let cwd_str = config.resolve_cwd(project.as_deref());
    let cwd = Path::new(cwd_str);
    let tools = config.resolve_tools(project.as_deref());

    // Create thread from the command
    let reply = ctx.say(format!("Starting Claude session...")).await?;
    let thread = ctx.channel_id()
        .create_thread_from_message(
            ctx.http(),
            reply.message().await?.id,
            serenity::builder::CreateThread::new(
                truncate_thread_name(&prompt)
            ).auto_archive_duration(serenity::model::channel::AutoArchiveDuration::OneDay),
        )
        .await?;

    let thread_id = ThreadId::from(thread.id);

    // P4: bounded channel with backpressure
    let (tx, rx) = crate::claude::process::event_channel();
    let cancel = state.shutdown.child_token();  // P4: child of global shutdown

    let handle = crate::claude::process::run_claude(
        config,
        &prompt,
        None,
        cwd,
        &tools,  // P1: &[Arc<str>] slice from Cow
        tx,
        cancel,
    ).await?;

    // Register session (DashMap, no await needed)
    state.session_manager.register(thread_id, handle, cwd.to_path_buf())?;

    // Persist to DB
    let user_id = UserId::from(ctx.author().id);
    crate::db::create_session(
        &state.db,
        thread_id,
        user_id,
        project.as_deref(),
    ).await?;

    // Spawn response streamer — receives its own child cancellation token
    let stream_cancel = state.shutdown.child_token();
    tokio::spawn(super::formatter::stream_to_discord(
        ctx.http().clone(),
        thread.id,
        rx,
        state.clone(),
        stream_cancel,
    ));

    ctx.say(format!("Session started in <#{}>", thread.id)).await?;
    Ok(())
}

/// Stop the active Claude process in the current thread.
#[poise::command(slash_command)]
pub async fn stop(ctx: Context<'_>) -> Result<(), AppError> {
    check_auth(&ctx)?;
    let thread_id = ThreadId::from(ctx.channel_id());

    if let Some(handle) = ctx.data().session_manager.remove(thread_id) {
        handle.kill().await?;
        crate::db::update_session_status(&ctx.data().db, thread_id, "stopped").await?;
        ctx.say("Session stopped.").await?;
    } else {
        ctx.say("No active session in this thread.").await?;
    }
    Ok(())
}

/// List all active sessions.
#[poise::command(slash_command)]
pub async fn sessions(ctx: Context<'_>) -> Result<(), AppError> {
    check_auth(&ctx)?;
    let count = ctx.data().session_manager.active_count();
    let max = ctx.data().config.claude.max_sessions;
    ctx.say(format!("Active sessions: {count}/{max}")).await?;
    Ok(())
}

/// P3: Truncate prompt to fit Discord thread name limit (100 chars).
#[inline]
fn truncate_thread_name(prompt: &str) -> String {
    if prompt.len() <= 97 {
        format!("CC: {prompt}")
    } else {
        format!("CC: {}...", &prompt[..94])
    }
}
```

### 4.3 `src/discord/handler.rs` -- Thread Reply Handler

```rust
use std::path::Path;
use std::sync::Arc;
use crate::domain::{ThreadId, UserId};
use crate::error::AppError;
use crate::AppState;

/// P4: async event handler. P2: early returns.
pub async fn handle_message(
    ctx: &serenity::client::Context,
    msg: &serenity::model::channel::Message,
    state: &Arc<AppState>,
) -> Result<(), AppError> {
    // P2: early return chain
    if msg.author.bot { return Ok(()); }

    let thread_id = ThreadId::from(msg.channel_id);

    // Check if this message is in a tracked thread
    let session = match crate::db::get_session_by_thread(&state.db, thread_id).await? {
        Some(s) => s,
        None => return Ok(()),
    };

    // Auth: only the session owner can send follow-ups
    let user_id = UserId::from(msg.author.id);
    if session.user_id != user_id {
        return Ok(());  // Silent ignore, not an error
    }

    // Don't spawn if already active
    if state.session_manager.has_session(thread_id) {
        // Could queue the message or reject; for now, inform the user
        msg.reply(ctx, "Claude is still processing the previous message. Please wait.")
            .await?;
        return Ok(());
    }

    let config = &state.config.claude;
    let project = session.project.as_deref();
    let cwd_str = config.resolve_cwd(project);
    let cwd = Path::new(cwd_str);
    let tools = config.resolve_tools(project);

    let (tx, rx) = crate::claude::process::event_channel();
    let cancel = state.shutdown.child_token();

    // P1: borrow session_id via as_deref
    let resume_id = session.claude_session_id.as_ref().map(|s| s.as_str());

    let handle = crate::claude::process::run_claude(
        config,
        &msg.content,
        resume_id,
        cwd,
        &tools,
        tx,
        cancel,
    ).await?;

    state.session_manager.register(thread_id, handle, cwd.to_path_buf())?;
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
```

### 4.4 `src/discord/formatter.rs` -- Buffer Reuse, Slice-Based Splitting

```rust
use std::sync::Arc;
use serenity::http::Http;
use serenity::model::id::ChannelId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::{ClaudeEvent, ThreadId};
use crate::AppState;

/// Pre-allocated buffer capacity. Sized for typical Claude responses.
/// P3: allocated once, reused via .clear() which retains capacity.
const BUFFER_INITIAL_CAPACITY: usize = 2048;

/// Discord message limit minus safety margin for code fence closure.
const FLUSH_THRESHOLD: usize = 1800;

/// Stream Claude events to a Discord channel.
/// P3: Pre-allocates buffer once, reuses with .clear().
/// P4: Respects CancellationToken for graceful shutdown.
pub async fn stream_to_discord(
    http: Arc<Http>,
    channel_id: ChannelId,
    mut rx: mpsc::Receiver<ClaudeEvent>,
    state: Arc<AppState>,
    cancel: CancellationToken,
) {
    // P3: Pre-allocate buffer, reuse across flushes
    let mut buffer = String::with_capacity(BUFFER_INITIAL_CAPACITY);
    let mut in_code_fence = false;
    let thread_id = ThreadId::from(channel_id);

    // Typing indicator task
    let typing_cancel = cancel.child_token();
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

    // HOT PATH: event consumption loop
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!(?thread_id, "stream_to_discord cancelled");
                break;
            }
            event = rx.recv() => {
                match event {
                    Some(ClaudeEvent::TextDelta(text)) => {
                        // P1: text is Arc<str>, we borrow via &*text
                        buffer.push_str(&text);

                        // P2: track code fence state with simple toggle
                        // Count ``` occurrences in this delta
                        update_fence_state(&text, &mut in_code_fence);

                        // Flush if buffer exceeds threshold
                        if buffer.len() >= FLUSH_THRESHOLD {
                            let chunk = take_chunk(&mut buffer, in_code_fence);
                            send_message(&http, channel_id, &chunk).await;
                        }
                    }
                    Some(ClaudeEvent::ToolUse { tool, .. }) => {
                        // Flush any pending text first
                        if !buffer.is_empty() {
                            let chunk = take_all(&mut buffer, in_code_fence);
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

    // Flush remaining buffer
    if !buffer.is_empty() {
        let chunk = take_all(&mut buffer, in_code_fence);
        send_message(&http, channel_id, &chunk).await;
    }

    // Cleanup
    typing_cancel.cancel();
    typing_task.abort();
    state.session_manager.remove(thread_id);
}

/// P2: Track code fence state by counting ``` in the delta.
/// An odd count toggles the state.
#[inline]
fn update_fence_state(text: &str, in_fence: &mut bool) {
    let count = text.matches("```").count();
    if count % 2 == 1 {
        *in_fence = !*in_fence;
    }
}

/// P1: Split works on &str, returns owned String only for the chunk to send.
/// The remaining data stays in `buffer` (which retains its capacity via truncation).
///
/// Strategy:
/// 1. Find last good break point before FLUSH_THRESHOLD
/// 2. Priority: "\n\n" > "\n" > " "
/// 3. If inside code fence at split, close with ``` and reopen in remainder
fn take_chunk(buffer: &mut String, in_code_fence: bool) -> String {
    let split_at = find_split_point(buffer, FLUSH_THRESHOLD);

    let mut chunk = String::with_capacity(split_at + 8); // +8 for possible fence closure
    chunk.push_str(&buffer[..split_at]);

    // If we're splitting inside a code fence, close it in this chunk
    // and reopen in the next
    if in_code_fence {
        chunk.push_str("\n```");
    }

    // Remove the sent portion, keeping the remainder
    // P3: drain preserves capacity
    let remainder_start = if buffer[split_at..].starts_with('\n') {
        split_at + 1
    } else {
        split_at
    };

    let remainder = buffer[remainder_start..].to_string();
    buffer.clear();  // P3: retains capacity
    if in_code_fence {
        buffer.push_str("```\n");
    }
    buffer.push_str(&remainder);

    chunk
}

/// Take everything from buffer as a final flush.
fn take_all(buffer: &mut String, _in_code_fence: bool) -> String {
    let chunk = buffer.clone();
    buffer.clear();  // P3: retains capacity
    chunk
}

/// P2: find_split_point uses rfind chain with fallback.
#[inline]
fn find_split_point(text: &str, max: usize) -> usize {
    let search_range = &text[..max.min(text.len())];

    // Priority: double newline, single newline, space
    search_range.rfind("\n\n")
        .map(|i| i + 1)  // Include one newline in first chunk
        .or_else(|| search_range.rfind('\n'))
        .or_else(|| search_range.rfind(' '))
        .unwrap_or(max.min(text.len()))
}

async fn send_message(http: &Http, channel_id: ChannelId, content: &str) {
    if content.is_empty() { return; }

    // Discord hard limit is 2000
    if content.len() > 2000 {
        // Emergency split: should rarely happen due to flush threshold
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
        // Two fences in one delta: open and close
        update_fence_state("```code```", &mut in_fence);
        assert!(!in_fence);  // Even count = no change
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
```

### 4.5 `src/discord/mod.rs` -- Framework Setup

```rust
pub mod commands;
pub mod handler;
pub mod formatter;

use std::sync::Arc;
use crate::error::AppError;
use crate::AppState;

/// P4: start_bot is the main async entry for the Discord gateway connection.
pub async fn start_bot(state: Arc<AppState>) -> Result<(), AppError> {
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                commands::claude(),
                commands::stop(),
                commands::sessions(),
            ],
            event_handler: |ctx, event, _fw_ctx, state| {
                Box::pin(async move {
                    if let poise::FullEvent::Message { new_message } = event {
                        if let Err(e) = handler::handle_message(ctx, new_message, state).await {
                            tracing::error!(error = %e, "message handler error");
                        }
                    }
                    Ok(())
                })
            },
            ..Default::default()
        })
        .setup(|ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                tracing::info!("bot ready, commands registered");
                Ok(state)
            })
        })
        .build();

    let intents = serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::MESSAGE_CONTENT;

    // P1: borrow token from Arc<str>
    let mut client = serenity::ClientBuilder::new(
        state.config.discord.token.as_ref(),
        intents,
    )
    .framework(framework)
    .await?;

    // P4: Run client with shutdown awareness
    let shutdown = state.shutdown.clone();
    tokio::select! {
        result = client.start() => {
            result?;
        }
        _ = shutdown.cancelled() => {
            tracing::info!("discord bot shutting down");
            client.shard_manager.shutdown_all().await;
        }
    }

    Ok(())
}
```

---

## Phase 5: Session Persistence

### 5.1 `src/db.rs`

```rust
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::domain::{Session, SessionStatus, ThreadId, UserId, ClaudeSessionId};
use crate::error::AppError;

/// P4: All DB operations are async via sqlx.
/// P1: ThreadId/UserId newtypes used throughout; &str borrows for string params.

pub async fn create_session(
    pool: &SqlitePool,
    thread_id: ThreadId,
    user_id: UserId,
    project: Option<&str>,  // P1: borrow, don't take String
) -> Result<(), AppError> {
    let id = Uuid::new_v4().to_string();
    let tid = thread_id.get() as i64;
    let uid = user_id.get() as i64;
    sqlx::query!(
        "INSERT INTO sessions (id, thread_id, user_id, project, status)
         VALUES (?, ?, ?, ?, 'idle')",
        id, tid, uid, project
    )
    .execute(pool).await?;
    Ok(())
}

pub async fn get_session_by_thread(
    pool: &SqlitePool,
    thread_id: ThreadId,
) -> Result<Option<Session>, AppError> {
    let tid = thread_id.get() as i64;
    let row = sqlx::query!(
        "SELECT id, thread_id, user_id, claude_session_id, project, status,
                created_at, last_active_at
         FROM sessions WHERE thread_id = ? AND status IN ('active', 'idle')",
        tid
    )
    .fetch_optional(pool).await?;

    // P2: map with and_then chain
    Ok(row.map(|r| Session {
        id: Uuid::parse_str(&r.id).unwrap_or_default(),
        thread_id: ThreadId::new(r.thread_id as u64),
        user_id: UserId::new(r.user_id as u64),
        status: SessionStatus::from_str(&r.status),
        last_active_at: r.last_active_at.parse().unwrap_or_default(),
        claude_session_id: r.claude_session_id.map(|s| ClaudeSessionId::new(&s)),
        project: r.project.map(|s| std::sync::Arc::from(s.as_str())),
        created_at: r.created_at.parse().unwrap_or_default(),
    }))
}

pub async fn update_session_id(
    pool: &SqlitePool,
    thread_id: ThreadId,
    claude_session_id: &str,  // P1: borrow
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query!(
        "UPDATE sessions SET claude_session_id = ?, status = 'active',
         last_active_at = datetime('now') WHERE thread_id = ?",
        claude_session_id, tid
    )
    .execute(pool).await?;
    Ok(())
}

pub async fn update_session_status(
    pool: &SqlitePool,
    thread_id: ThreadId,
    status: &str,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query!(
        "UPDATE sessions SET status = ?, last_active_at = datetime('now')
         WHERE thread_id = ?",
        status, tid
    )
    .execute(pool).await?;
    Ok(())
}

pub async fn touch_session(
    pool: &SqlitePool,
    thread_id: ThreadId,
) -> Result<(), AppError> {
    let tid = thread_id.get() as i64;
    sqlx::query!(
        "UPDATE sessions SET last_active_at = datetime('now') WHERE thread_id = ?",
        tid
    )
    .execute(pool).await?;
    Ok(())
}
```

### 5.2 Session resume on bot restart

In `main.rs` startup, after DB connection:
1. Query all sessions with status = 'idle' or 'active'
2. Log them -- they are available for `--resume` when the user sends a message in that thread
3. No need to proactively spawn processes; the thread message handler picks them up via `get_session_by_thread`

### 5.3 Background reaper

See Phase 6 (main.rs) -- spawned as a background task with CancellationToken.

---

## Phase 6: `src/main.rs` -- Graceful Shutdown

```rust
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use claude_remote_chat::{
    AppState,
    config::AppConfig,
    claude::session::SessionManager,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. Load config (P4: async file read) ──
    let config = AppConfig::from_file("config.toml").await?;

    // ── 2. Init tracing ──
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(config.logging.level.as_ref()));

    match config.logging.format.as_ref() {
        "json" => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        _ => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .init();
        }
    }

    // ── 3. Connect DB, run migrations (P4: async) ──
    let pool = sqlx::SqlitePool::connect(config.database.url.as_ref()).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;

    // ── 4. Build shared state ──
    let config = Arc::new(config);
    let shutdown = CancellationToken::new();  // P4: root cancellation token

    let state = Arc::new(AppState {
        session_manager: SessionManager::new(Arc::clone(&config)),
        config: Arc::clone(&config),
        db: pool,
        shutdown: shutdown.clone(),
    });

    // ── 5. Spawn background reaper (receives child token) ──
    let reaper_state = Arc::clone(&state);
    let reaper_cancel = shutdown.child_token();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = reaper_cancel.cancelled() => {
                    tracing::info!("reaper shutting down");
                    break;
                }
                _ = interval.tick() => {
                    let killed = reaper_state.session_manager.reap_expired().await;
                    if killed > 0 {
                        tracing::info!(killed, "reaped expired sessions");
                    }
                }
            }
        }
    });

    // ── 6. Spawn signal handler (P4: tokio::signal, not std) ──
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate()
        ).expect("failed to register SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => tracing::info!("received SIGINT"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        }

        tracing::info!("initiating graceful shutdown");
        signal_shutdown.cancel();
    });

    // ── 7. Start Discord bot (blocks until shutdown) ──
    tracing::info!("starting discord bot");
    claude_remote_chat::discord::start_bot(state).await?;

    tracing::info!("shutdown complete");
    Ok(())
}
```

---

## Design Decisions

### Why subprocess over Agent SDK?

The Agent SDK (TypeScript/Python) couples to a specific SDK version. The CLI subprocess approach:
- Works with any Claude Code version automatically
- Gets new features as the CLI updates
- Simpler to reason about (it is just stdin/stdout)

### Why DashMap over RwLock<HashMap>?

With max 3 sessions, contention is negligible either way. DashMap is chosen for:
- Simpler API (no `.read().await` / `.write().await` ceremony)
- `register()` and `has_session()` do not need to be async, simplifying the Discord handler
- Fine-grained per-key locking if max_sessions is ever increased

### Why Arc<str> everywhere instead of String?

- `Arc<str>` is 16 bytes (ptr + len). `Arc<String>` is 16 bytes pointing to a String that is 24 bytes (ptr + len + capacity). Total: 40 bytes vs 16 bytes.
- Config strings never change after startup. `Arc<str>` makes sharing across tasks free (just increment refcount).
- `ClaudeEvent::TextDelta(Arc<str>)` allows the parser to produce events that the formatter consumes without copying the text data.

### Why Box<str> for error messages?

- `Box<str>` is 16 bytes (ptr + len). `String` is 24 bytes (ptr + len + capacity).
- Error messages are write-once, read-once. They never need to grow, so capacity is wasted.
- Keeps the `AppError` enum smaller (size = largest variant).

### Why CancellationToken over ad-hoc shutdown?

- Hierarchical: `child_token()` creates scoped cancellation. Cancelling the root propagates to all children.
- Composable with `tokio::select!` -- no special handling needed.
- Each background task (reaper, typing indicator, stream reader) receives its own token.

### Why SmallVec for tool/role lists?

- `SmallVec<[Arc<str>; 8]>` stores up to 8 elements inline on the stack (no heap allocation).
- Default tool list has 6 entries. Most configs will never heap-allocate.
- `SmallVec` implements `Deref<Target=[T]>` so all slice operations work transparently.

### Concurrent sessions on same project

Two users editing the same project simultaneously could conflict. Solution: enforce one active session per project in SessionManager. The second request gets a message: "Project X already has an active session in <#thread>".

### Discord rate limits

serenity handles rate limiting internally. The accumulator/chunking approach batches text to reduce message count. Tool-use status messages use italics to keep them minimal.

---

## Testing Checklist

1. `cargo build` -- compiles
2. `cargo clippy --all-targets --all-features -- -D warnings` -- no warnings
3. `cargo test` -- unit tests for parser, formatter split logic, fence tracking
4. Manual: run with a test Discord server
   - `/claude say hello` -- creates thread, gets response
   - Reply in thread -- resumes conversation
   - `/stop` -- kills process
   - `/sessions` -- lists active sessions
5. Restart bot, reply in old thread -- session resumes via `--resume`
6. Let session exceed timeout -- reaper kills it
7. Hit max sessions -- get error message
8. Unauthorized user -- gets rejected
9. Ctrl+C -- graceful shutdown kills all Claude processes

---

## Future Enhancements (not in scope now)

- **Voice input**: Discord voice-to-text integration
- **File attachments**: Upload files to Claude's working directory via Discord
- **Approval mode**: Bot asks for permission before Claude runs dangerous tools, user approves via Discord reactions
- **Web dashboard**: Optional Axum health/status endpoint
- **Multi-user**: Per-user session isolation with separate working directories
- **trait EventSink**: Abstract Discord output behind a trait for testing and alternative frontends (Telegram, Slack). Use monomorphization (generics, not dyn) for zero-cost dispatch on the hot path.

---

### Critical Files for Implementation
- `/home/john/dump/git-repos/git-moje/claude-remote-chat/PLAN.md` - The file to be replaced with this redesigned plan
- `/home/john/dump/git-repos/git-moje/claude-remote-chat/CLAUDE.md` - Project instructions that define the architecture and module structure (keep in sync)
- `/home/john/dump/git-repos/git-moje/claude-remote-chat/shell.nix` - Dev environment; already correct, no changes needed
- `/home/john/dump/git-repos/git-moje/claude-remote-chat/README.md` - Public docs referencing PLAN.md; may need minor updates to mention new deps (dashmap, smallvec, tokio-util)