# CLAUDE.md

## Project Overview

Rust Discord bot that bridges Claude Code CLI to Discord, enabling mobile interaction with running Claude Code sessions. Single binary: Discord bot + Claude subprocess manager + SQLite persistence.

## Tech Stack

- **Language:** Rust (latest stable)
- **Discord framework:** poise 0.6 (wraps serenity)
- **Async runtime:** Tokio + tokio-util (CancellationToken)
- **Database:** SQLite via sqlx (async, compile-time checked queries)
- **Concurrency:** DashMap (lock-free concurrent map for session registry)
- **Config:** TOML via serde (two-phase: Raw -> validated Arc<str>-backed)
- **Collections:** SmallVec (inline stack storage for small lists)
- **Logging:** `tracing` + `tracing-subscriber`
- **Dev environment:** Nix (`nix-shell` to enter)

## Build Commands

```bash
nix-shell
cargo build
cargo run
cargo build --release --no-default-features --target x86_64-unknown-linux-musl
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

## Engineering Principles

Every module adheres to these rules. Violations are bugs.

### P1: Borrow/Consume Flow
- Pass `&Config`, `&str`, `&[T]` on every call path. Never clone a config struct.
- `Arc<str>` for cross-task shared strings. Never `Arc<String>`.
- `Cow<'a, T>` where a function usually borrows but sometimes must own.

### P2: Functional Patterns
- `fold`/`filter_map` chains for single-pass accumulation without intermediate Vecs.
- `and_then()` / `ok_or_else()` for nested optionals with rich error context.
- Early returns with `?` and contextual errors. No nested if-else pyramids.

### P3: Memory Efficiency
- `SmallVec<[T; N]>` for typically-small collections (tool lists N=8, role lists N=4).
- Buffer reuse: `.clear()` retains capacity. Formatter pre-allocates once (2048 bytes).
- `Box<str>` for error messages (16 bytes vs String's 24). Keeps enum size compact.

### P4: Async-Only IO
- ALL IO via tokio: `tokio::fs`, `tokio::process`, sqlx async, serenity async.
- `CancellationToken` for graceful shutdown. Every background task receives one.
- Bounded `mpsc` channels with backpressure. `select!` for multi-signal awaiting.
- Never: `std::fs`, `std::thread::sleep`, synchronous `std::process::Command`.

### P5: CPU Locality
- Hot/cold field separation in structs (status/last_active first, metadata last).
- `#[inline]` on small hot-path functions (parser match arms, event dispatch).
- Monomorphization via generic trait bounds. No `dyn Trait` on hot paths.

### P6: Type-Driven Design
- Newtypes: `ThreadId(u64)`, `UserId(u64)`, `ClaudeSessionId(Arc<str>)`.
- `const fn` for compile-time constants (defaults, static config values).

## Architecture

### Module Structure

```
src/
  main.rs           -- entry point, signal handler, graceful shutdown
  lib.rs            -- AppState (with CancellationToken), re-exports
  config.rs         -- Raw TOML -> validated Arc<str>-backed config
  error.rs          -- thiserror AppError with Box<str> messages
  db.rs             -- sqlx SQLite session repository
  domain.rs         -- Newtypes, Session, ClaudeEvent (compact enum)
  discord/
    mod.rs          -- poise framework setup with shutdown-aware select!
    commands.rs     -- /claude, /stop, /sessions slash commands
    handler.rs      -- thread message handler for follow-up replies
    formatter.rs    -- pre-allocated buffer, slice-based chunking
  claude/
    mod.rs          -- re-exports
    process.rs      -- CancellationToken-aware subprocess management
    parser.rs       -- #[inline] zero-copy stream-json parser
    session.rs      -- DashMap-backed SessionManager
```

### Background Tasks

All receive `CancellationToken` and use `tokio::select!` for clean shutdown.

1. **Session reaper** — kills sessions exceeding `session_timeout_minutes`, updates DB status to "expired"
2. **Typing indicator** — re-sends every 8s while Claude is processing
3. **Signal handler** — SIGINT/SIGTERM -> cancels root token

### Message Flow

```
Discord message (or @mention) -> poise handler -> strip bot mention if present
  -> DB session lookup -> SessionManager check (DashMap, no await)
  -> spawn/resume claude -p subprocess (tokio::process, CancellationToken)
  -> stream stdout line-by-line -> parse ClaudeEvent (#[inline] hot path)
  -> bounded mpsc channel (backpressure) -> formatter
  -> accumulate in pre-allocated buffer -> chunk at ~1800 chars -> send to Discord
  -> on Done: set DB status to "idle", remove from DashMap
```

### Database

Single table: `sessions` (id UUID, thread_id, user_id, claude_session_id, project, status, timestamps).

### Configuration

Two-phase TOML loading: `RawAppConfig` (serde Strings) -> `AppConfig` (validated `Arc<str>`). See `config.example.toml`.

### Permission Model

- `allowed_tools` in config → passed as `--allowedTools` (auto-approved in headless `-p` mode)
- `dangerously_skip_permissions` in config → `--dangerously-skip-permissions` flag (bypasses all checks)
- Tools not in `allowed_tools` are denied (fail-closed in `-p` mode)

### Session Lifecycle

Status transitions: `idle` → `active` (SessionId received) → `idle` (process completes) → `expired` (reaped) or `stopped` (user /stop).

### Slash Command Interaction

All slash commands call `ctx.defer()` first to avoid Discord's 3-second interaction timeout.

## Deployment

- Runs as systemd service
- Config in TOML format
- Needs `claude` CLI accessible in PATH
- SQLite DB file in working directory
- Graceful shutdown on SIGTERM/SIGINT (kills all Claude subprocesses)
