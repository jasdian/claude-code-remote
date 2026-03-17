<div align="center">

# claude-remote-chat

**Talk to Claude Code from your phone. Rust Discord bot that bridges mobile to running Claude Code terminal sessions.**

*Your AI pair-programmer, always in your pocket.*

<p align="center">
<a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-stable-orange?logo=rust" alt="Rust"></a>
<a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
<a href="https://discord.com/"><img src="https://img.shields.io/badge/Discord-bot-5865F2?logo=discord&logoColor=white" alt="Discord"></a>
</p>

</div>

## Why?

Long-running Claude Code sessions stall at permission prompts, need follow-up input, or produce results you want to check — but you're not at your desk. Existing solutions are mostly TypeScript. This one is Rust.

```
You (phone / Discord DM or server)
       |
       v
claude-remote-chat (Rust)
       |
       v
  Claude Code CLI
       |
       v
  Your Machine
```

## How It Works

1. You DM the bot or use `/claude fix the auth bug` in a server
2. Bot spawns `claude -p` as a subprocess on your machine
3. Claude Code runs locally with full file access
4. Response flows back to Discord in real-time
5. You reply — the bot resumes the same Claude session via `--resume`

Works in both **DMs** (just message the bot directly) and **server channels** (creates a thread per session).

## Features

**Discord Integration**
- **DM mode** — message the bot directly, no slash commands needed
- **Server mode** — thread-per-session with `/claude` slash command
- **@mention support** — mention the bot in a session thread to continue the conversation
- Slash commands: `/claude`, `/end`, `/interrupt`, `/sessions`
- **Message queuing** — messages sent while Claude is busy are queued (📨) and auto-processed
- **Interrupt** — `!` prefix or `/interrupt` kills current task and sends the new message (⏭️)
- Natural follow-ups — just type in the thread to continue
- Smart message chunking (handles Discord's 2000-char limit)
- Typing indicators and tool-use status with spoiler previews (click to see details)
- `/end` archives the thread after stopping the session

**Claude Code Management**
- Subprocess lifecycle via `tokio::process`
- Streaming `stream-json` parser for real-time output
- Multi-turn conversations via `--resume SESSION_ID`
- Smart project resolution — named projects, sibling directory discovery, or default cwd
- Configurable tool permissions per project (auto-approved in headless mode)
- Optional `--dangerously-skip-permissions` for trusted environments
- Session timeout and automatic cleanup
- stderr capture — Claude process errors are logged and surfaced to Discord

**Security**
- Discord user/role allowlist
- Per-project tool restrictions (`--allowedTools` auto-approves listed tools, denies others)
- No secrets in Discord — Claude runs locally on your machine

**Operations**
- SQLite session persistence (survives bot restarts)
- TOML configuration
- Structured logging via `tracing` with custom poise error handler
- Graceful shutdown (SIGINT/SIGTERM with 5s timeout)

## Prerequisites

- **Rust** (stable, latest) — or use `nix-shell` for the dev environment
- **Claude Code CLI** (`claude`) — installed and authenticated on your machine
- **Discord Bot** — created via the Developer Portal (see setup below)

## Discord Bot Setup

### 1. Create the Application

1. Go to **https://discord.com/developers/applications**
2. Click **"New Application"** — give it a name (e.g. "Claude Remote")
3. Note the **Application ID** on the General Information page

### 2. Create the Bot

1. Click **"Bot"** in the left sidebar
2. Click **"Reset Token"** to generate a bot token
3. **Copy the token** — you'll need it for `config.toml`. This is the only time you can see it.
4. Under **Privileged Gateway Intents**, enable:
   - **Message Content Intent** (required — the bot reads message text)
5. Save changes

### 3. Get Your IDs

You need two IDs for the config. Enable **Developer Mode** in Discord first:
- Discord Settings → App Settings → Advanced → **Developer Mode** → ON

Then:
- **Guild (Server) ID**: Right-click your server name → **Copy Server ID**
- **Your User ID**: Right-click your username → **Copy User ID**

### 4. Invite the Bot to Your Server

1. Go to **"OAuth2"** in the left sidebar
2. Under **OAuth2 URL Generator**:
   - Scopes: `bot`, `applications.commands`
   - Bot Permissions:
     - Send Messages
     - Create Public Threads
     - Send Messages in Threads
     - Use Slash Commands
     - Read Message History
3. Copy the generated URL and open it in your browser
4. Select your server and authorize

### 5. Configure and Run

```bash
# Enter dev environment (optional, if using Nix)
nix-shell

# Create config from example
cp config.example.toml config.toml

# Edit config.toml with your values:
#   - discord.token = "your bot token from step 2"
#   - discord.guild_id = your server ID from step 3
#   - auth.allowed_users = [your user ID from step 3]
#   - claude.default_cwd = path to your project directory

# Run the bot
cargo run
```

### 6. Use It

**Via DM**: Just open a DM with the bot and type your message. No slash commands needed — every message starts or continues a Claude session.

**Via Server**: Use `/claude <prompt>` in any channel. The bot creates a thread and streams Claude's response. Reply in the thread to continue.

**Via @mention**: In an existing session thread, @mention the bot with your message to continue the conversation.

## Configuration

```toml
[discord]
token = "MTIxNzU1..."          # Bot token from Developer Portal
guild_id = 1233628554378477589  # Your server ID

[claude]
binary = "claude"                                    # Path to claude CLI
default_cwd = "/home/you/projects"                   # Default working directory
allowed_tools = ["Bash", "Read", "Write", "Edit", "Glob", "Grep"]
max_sessions = 3                                     # Max concurrent sessions
session_timeout_minutes = 30                         # Auto-kill after inactivity
# system_prompt = "Keep responses concise."          # Optional system prompt
# dangerously_skip_permissions = false               # Skip all permission prompts

[claude.projects.myapp]                              # Named project overrides
cwd = "/home/you/projects/myapp"
# allowed_tools = ["Read", "Grep"]                   # Restrict tools per project

[auth]
allowed_users = [594857943015358487]                  # Discord user IDs allowed
allowed_roles = []                                   # Discord role IDs allowed

[database]
url = "sqlite:data.db?mode=rwc"                      # SQLite DB path

[logging]
level = "info"                                       # debug, info, warn, error
format = "pretty"                                    # pretty or json
```

### Tool Permissions

The `allowed_tools` list controls which tools Claude can use. In headless (`-p`) mode:
- **Listed tools are auto-approved** — no permission prompt needed
- **Unlisted tools are denied** — Claude cannot use them (fail-closed)
- Set `dangerously_skip_permissions = true` to bypass all permission checks (use only in trusted environments)

## Commands

| Command | Where | Description |
|---------|-------|-------------|
| `/claude <prompt> [project]` | Server | Start a new Claude session in a thread |
| `/end` | Session thread | Stop session and archive the thread |
| `/interrupt [prompt]` | Session thread | Kill current task, optionally send new prompt |
| `/sessions` | Anywhere | Show active session count |
| *(just type)* | DM | Start or continue a Claude session |
| *@mention bot* | Session thread | Continue the conversation |
| `!message` | Session thread | Interrupt current task and send message |

After the initial `/claude` command in a server, just type messages in the thread — the bot picks them up automatically.

If Claude is busy, your message is **queued** (📨 reaction) and sent automatically when the current task finishes. Prefix with `!` to **interrupt** (⏭️ reaction) — kills the current task and sends your message immediately.

## Build Commands

```bash
cargo build                    # Dev build
cargo run                      # Run the bot
cargo test                     # Run unit tests
cargo clippy --all-targets     # Lint
cargo build --release          # Release build (LTO, stripped)
```

## Tech Stack

- **Rust** with Tokio async runtime + tokio-util (CancellationToken)
- **poise** — Discord bot framework (wraps serenity)
- **sqlx** — SQLite for session persistence
- **dashmap** — Lock-free concurrent session registry
- **smallvec** — Inline stack storage for small collections
- **tokio::process** — Claude Code subprocess management
- **tracing** — Structured logging
- **serde + toml** — Two-phase config (Raw TOML -> validated Arc<str>-backed)

## Architecture

See [PLAN.md](PLAN.md) for the full implementation guide including module structure, key types, and design decisions.

## Troubleshooting

| Problem | Fix |
|---------|-----|
| "The application did not respond" on `/claude` | Ensure the bot has Send Messages permission |
| Bot connects then disconnects with "Disallowed intents" | Enable **Message Content Intent** in Bot settings on the Developer Portal |
| Slash commands don't appear | Wait 1-2 minutes after first bot startup for Discord to register them globally |
| Bot doesn't respond to DMs | Make sure your user ID is in `auth.allowed_users` in config.toml |
| "failed to spawn claude" error | Ensure `claude` CLI is in PATH and authenticated. On NixOS, use an FHS wrapper script as `binary` |
| Bot responds but Claude output is empty | Check stderr logs — Claude errors are now logged. Verify `default_cwd` is valid |
| Claude can't use tools (permission denied) | Add the tools to `allowed_tools` in config, or set `dangerously_skip_permissions = true` |
| Follow-up messages start new conversations | Check logs for `claude_session_id` — the session may have expired |
| Ctrl+C doesn't work | Run the binary directly (`./target/debug/claude-remote-chat`), not via `cargo run` |
| "Invalid Form Body (name)" error | Thread name exceeded 100 chars — this is now fixed with proper truncation |

## Roadmap

Potential future features:

- **Interactive permission prompts** — Use `--permission-prompt-tool` to route Claude's permission requests to Discord, letting users approve/deny tool use from their phone via reaction buttons
- **Session list with details** — Enhance `/sessions` to show thread links, project names, and session age
- **Multi-user session sharing** — Allow other authorized users to interact with a session in the same thread
- **File attachment support** — Send files via Discord attachments for Claude to read
- **Git worktree per session** — Isolate concurrent sessions working on the same project

## Related Projects

- [claude-code-discord](https://github.com/zebbern/claude-code-discord) — TypeScript/Deno, uses Claude Agent SDK directly
- [claude-code-discord-bridge](https://github.com/ebibibi/claude-code-discord-bridge) — TypeScript, thread-per-session with git worktrees
- [discord-agent-bridge](https://github.com/DoBuDevel/discord-agent-bridge) — tmux polling approach
- [Claude-Code-Remote](https://github.com/JessyTsui/Claude-Code-Remote) — Email/Discord/Telegram control

Inspired by the discussion at [anthropics/claude-code#15922](https://github.com/anthropics/claude-code/issues/15922).

## License

MIT
