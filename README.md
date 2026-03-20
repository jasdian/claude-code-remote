<div align="center">

# Claude Crew

**Collaborate with Claude Code as a team -- one Discord thread, multiple minds.**

*Your team's AI pair programmer, one Discord thread away.*

<p align="center">
<a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-stable-orange?logo=rust" alt="Rust"></a>
<a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
<a href="https://discord.com/"><img src="https://img.shields.io/badge/Discord-bot-5865F2?logo=discord&logoColor=white" alt="Discord"></a>
</p>

</div>

## Why?

Your team uses Claude Code, but each person runs their own session in their own terminal. **Nobody sees what Claude is doing for someone else.** Context gets lost, work gets duplicated, and junior devs are left out of the loop.

Claude Crew turns Claude Code into a **shared, real-time experience**. One person starts a session in Discord. Teammates jump in -- ask follow-up questions, steer Claude in a new direction, review its tool usage -- all in the same thread. Everyone sees the same streaming output, and every message is attributed to the person who sent it.

**Concrete scenario:** A backend engineer starts a Claude session to refactor the auth module. The security lead joins the thread to add constraints. A new hire watches the whole exchange and learns the codebase. The audit trail records who asked for what. The session runs on your machine with full file access -- nothing leaves your infrastructure.

```
Alice (laptop)  --+
                  |    Discord Thread     Claude Crew (Rust)             Claude Code CLI
Bob   (phone)   --+--> #refactor-auth --> session manager          --> claude subprocess
Carol (tablet)  --+                       (per-user attribution)       (your machine)
```

It also works great for **solo use** -- talk to Claude Code from your phone while away from your desk, check on long-running sessions, and approve tool permissions from anywhere.

## How It Works

1. Alice uses `/claude refactor the auth module` in a server channel
2. Bot spawns a `claude` subprocess on your machine (using `--input-format stream-json`)
3. A Discord thread is created; Claude's response streams in real-time
4. Bob opens the thread and types a follow-up -- he's **auto-joined** as a participant
5. Carol types in the thread too -- she's auto-joined; `/participants` shows everyone in the session
6. Every message is attributed: Claude knows who said what
7. Alice (the owner) can `/sessionkick` someone or `/end` the session

Works in both **DMs** (just message the bot directly) and **server channels** (creates a thread per session).

## Features

**Multi-User Collaboration**
- **Auto-join** -- authorized users typing in a session thread are automatically added as participants
- **Participant management** -- `/participants`, `/sessionkick`, `/sessionban` commands for session membership
- **Per-user message attribution** -- every message to Claude is tagged with the sender's identity
- **Owner/participant roles** -- session creator has owner privileges; controls who can `/end`, `/sessionkick`, or `/handoff`
- **Full audit trail** -- tool invocations logged to SQLite with user attribution, input JSON, result preview, error status, and duration
- **Admin override** -- server admins can manage any session regardless of ownership

**Discord Integration**
- **DM mode** -- message the bot directly, no slash commands needed
- **Server mode** -- thread-per-session with `/claude` slash command
- **@mention support** -- mention the bot in a session thread to continue the conversation
- **Message queuing** -- messages sent while Claude is busy are queued and auto-processed
- **Interrupt** -- `!` prefix or `/interrupt` kills current task and sends the new message
- Natural follow-ups -- just type in the thread to continue
- Smart message chunking (handles Discord's 2000-char limit)
- Typing indicators and tool-use status
- **Interactive prompts** -- `AskUserQuestion` and permission requests are shown in Discord with @mention; reply to answer, auto-denied after 120s timeout
- **Expired session warnings** -- messages in ended/expired threads prompt confirmation before starting a new session (60s timeout)
- `/end` archives the thread after stopping the session

**Claude Code Management**
- Subprocess lifecycle via `tokio::process` with `--output-format stream-json`
- Streaming `stream-json` parser for real-time output
- `control_request` handling -- interactive permission prompts and user questions routed through Discord
- Multi-turn conversations via `--resume SESSION_ID`
- Smart project resolution -- named projects, sibling directory discovery, or default cwd
- Configurable tool permissions per project (auto-approved in headless mode)
- Optional `--dangerously-skip-permissions` for trusted environments
- **Git worktree isolation** -- optional per-project worktree per session, so concurrent sessions on the same repo don't conflict (`use_worktrees = true`)
- **Auto-PR on `/end`** -- when enabled (`auto_pr = true`), `/end` pushes the worktree branch and creates a PR via `gh` CLI if there are commits ahead of the default branch. **Note:** auto-PR currently uses `gh` (GitHub CLI) only. For GitLab repositories, keep `auto_pr = false` and create merge requests manually or let Claude do it via Bash with `glab mr create`
- **Co-authored commits** -- map Discord users to GitHub usernames/emails via config; collaborative sessions automatically add `Co-authored-by` trailers to every commit via a `prepare-commit-msg` git hook (worktree sessions) plus system prompt hints (all sessions)
- **Context-aware sessions** -- periodic background task summarizes each session's activity (files touched, tools used, recent messages) and injects sibling summaries into the system prompt. Sessions on the same project know what each other is doing -- preventing conflicting edits and enabling coordinated parallel work. Includes **file conflict detection** with warnings when sessions touch overlapping files. Opt-in via `[claude.context_sharing] enabled = true`
- Session timeout and automatic cleanup
- stderr capture -- Claude process errors are logged and surfaced to Discord

**Security**
- Discord user/role allowlist
- Per-project tool restrictions (`--allowedTools` auto-approves listed tools; unlisted tools prompt via `--permission-prompt-tool stdio`)
- No secrets in Discord -- Claude runs locally on your machine

**Operations**
- SQLite session persistence (survives bot restarts -- active sessions are reconciled to idle on startup so they can be resumed)
- TOML configuration
- Structured logging via `tracing` with custom poise error handler
- Graceful shutdown (SIGINT/SIGTERM with 5s timeout)

## Prerequisites

**Native:**
- **Rust** (stable, latest) -- or use `nix-shell` for the dev environment
- **Claude Code CLI** (`claude`) v2.1.78+ -- installed and authenticated on your machine

**Docker:**
- **Docker** with Compose (Rust and Claude CLI are bundled in the image)

**Both:**
- **Discord Bot** -- created via the Developer Portal (see setup below)
- **Anthropic API key** -- for the Claude CLI

## Discord Bot Setup

### 1. Create the Application

1. Go to **https://discord.com/developers/applications**
2. Click **"New Application"** -- give it a name (e.g. "Claude Remote")
3. Note the **Application ID** on the General Information page

### 2. Create the Bot

1. Click **"Bot"** in the left sidebar
2. Click **"Reset Token"** to generate a bot token
3. **Copy the token** -- you'll need it for `config.toml`. This is the only time you can see it.
4. Under **Privileged Gateway Intents**, enable:
   - **Message Content Intent** (required -- the bot reads message text)
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
     - View Channels
     - Send Messages
     - Send Messages in Threads
     - Create Public Threads
     - Manage Threads *(archiving threads on `/end`)*
     - Read Message History
     - Add Reactions *(emoji feedback on messages)*
     - Use Slash Commands
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

**Via DM**: Just open a DM with the bot and type your message. No slash commands needed -- every message starts or continues a Claude session.

**Via Server**: Use `/claude <prompt>` in any channel. The bot creates a thread and streams Claude's response. Reply in the thread to continue.

**Via @mention**: In an existing session thread, @mention the bot with your message to continue the conversation.

**Collaborate**: Any authorized user who types in an active session thread is automatically joined as a participant. Use `/participants` to see who's in a session.

## Configuration

```toml
[discord]
token = "YOUR_BOT_TOKEN"          # Bot token from Developer Portal
guild_id = 123456789012345678  # Your server ID

[claude]
binary = "claude"                                    # Path to claude CLI
default_cwd = "/home/you/projects"                   # Default working directory
allowed_tools = ["Bash", "Read", "Write", "Edit", "Glob", "Grep", "WebSearch", "WebFetch", "Agent", "ToolSearch", "Skill", "NotebookEdit"]
max_sessions = 3                                     # Max concurrent sessions
session_timeout_minutes = 30                         # Auto-kill after inactivity
# system_prompt = "Keep responses concise."          # Optional system prompt
# dangerously_skip_permissions = false               # Skip all permission prompts
# use_worktrees = false                              # Git worktree per session
# auto_pr = false                                    # Auto-create PR on /end

[claude.context_sharing]                             # Cross-session context awareness
# enabled = false                                    # Opt-in: summarize session activity
# interval_seconds = 120                             # How often to update summaries
# max_summary_chars = 1500                           # Budget for sibling context in prompt

[claude.projects.myapp]                              # Named project overrides
cwd = "/home/you/projects/myapp"
# allowed_tools = ["Read", "Grep"]                   # Restrict tools per project
# use_worktrees = true                               # Override per project
# auto_pr = true                                     # Override auto-PR per project

[auth]
admins = [123456789012345678]                        # Can /approve, /revoke, /pending
allowed_users = [123456789012345678]                  # Always authorized (config-managed)
allowed_roles = []                                   # Always authorized (by role)

[auth.user_identities.123456789012345678]            # Map Discord ID -> Git identity
github_username = "octocat"                           # For Co-authored-by trailers
# email = "octocat@example.com"                      # Preferred over github noreply

[database]
url = "sqlite:data.db?mode=rwc"                      # SQLite DB path

[logging]
level = "info"                                       # debug, info, warn, error
format = "pretty"                                    # pretty or json
```

### Tool Permissions

The bot uses `--permission-prompt-tool stdio --permission-mode default` to route all permission decisions through Discord:

- **Listed tools (`allowed_tools`) are auto-approved** -- no prompt needed
- **Unlisted tools trigger a `control_request`** -- the bot displays the permission request in Discord with an @mention, and the user can reply to approve or deny
- **`AskUserQuestion`** -- Claude's clarifying questions are forwarded to Discord; your freeform reply is sent back (not just yes/no)
- **Permission timeout** -- if no reply within 120 seconds, the request is auto-denied
- Set `dangerously_skip_permissions = true` to bypass all permission checks (use only in trusted environments)

## Commands

#### Session

| Command | Where | Description |
|---------|-------|-------------|
| `/claude <prompt> [project]` | Server | Start a new session in a thread |
| *(just type)* | DM | Start or continue a session |
| *@mention bot* | Session thread | Continue the conversation |
| `/interrupt [prompt]` | Session thread | Kill current task, optionally send new prompt |
| `!message` | Session thread | Interrupt current task and send message |
| `/compact` | Session thread | Summarize conversation to reduce context usage |
| `/context` | Session thread | Show current context window and token usage |
| `/end` | Session thread | Stop session and archive the thread (owner/admin only) |
| `/sessions` | Anywhere | List active sessions with thread links, project names, age, and status |

#### Collaboration

| Command | Where | Description |
|---------|-------|-------------|
| `/participants` | Session thread | List all participants and their roles (ephemeral) |
| `/handoff <user>` | Session thread | Transfer session ownership to another participant (owner/admin only, ephemeral) |
| `/sessionkick <user>` | Session thread | Remove a participant (owner/admin only, ephemeral) |
| `/sessionban <user>` | Session thread | Remove from session and revoke access (admin only, ephemeral) |

#### Access

| Command | Where | Description |
|---------|-------|-------------|
| `/optin` | Anywhere | Request access (ephemeral) |
| `/optout` | Anywhere | Remove your own access (ephemeral) |
| `/approve <user>` | Anywhere | Admin: approve a pending request (ephemeral) |
| `/revoke <user>` | Anywhere | Admin: revoke a user's access (ephemeral) |
| `/pending` | Anywhere | Admin: list pending requests (ephemeral) |
| `/audit [id] [count] [detail]` | Session thread | Admin: view tool usage audit log; `detail=true` shows full input JSON and result (ephemeral) |

After the initial `/claude` command in a server, just type messages in the thread -- the bot picks them up automatically.

If Claude is busy, your message is **queued** and sent automatically when the current task finishes. Prefix with `!` to **interrupt** -- kills the current task and sends your message immediately. If Claude is waiting for your reply to a question or permission prompt, your message is routed as the answer.

### Access Control

Users can be authorized in three ways:
1. **Config** -- `allowed_users` and `admins` in `config.toml` (permanent, requires restart)
2. **Roles** -- `allowed_roles` in config (Discord role-based)
3. **Dynamic** -- `/optin` request approved by an admin via `/approve` (stored in DB, instant)

## Build Commands

```bash
cargo build                    # Dev build
cargo run                      # Run the bot
cargo test                     # Run unit tests
cargo clippy --all-targets     # Lint
cargo build --release          # Release build (LTO, stripped)
```

## Docker Deployment

The bot can be containerized using the pre-built static binary pattern.

### 1. Build the Binary

```bash
cargo build --release --no-default-features --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/claude-crew .
```

### 2. Build the Image

```bash
docker build -t claude-crew .
```

The image uses `debian:bookworm-slim` as the base and installs Claude Code as a standalone binary (no Node.js required).

### 3. Configure

```bash
cp config.example.toml config.prod.toml
```

All `cwd` paths in the config must point inside the `/projects` mount so Claude can read/write project files. The host directory is bind-mounted read-write.

```toml
# config.prod.toml -- Docker paths
[database]
url = "sqlite:/data/data.db?mode=rwc"

[claude]
default_cwd = "/projects"

[claude.projects.myapp]
cwd = "/projects/myapp"
```

### 4. Run

```bash
# Set your API key
export ANTHROPIC_API_KEY="sk-ant-..."

# With compose
docker compose up -d

# Or standalone
docker run -d \
  --name claude-crew \
  --restart unless-stopped \
  -e ANTHROPIC_API_KEY \
  -v claude-data:/data \
  -v claude-state:/home/appuser/.claude \
  -v ./config.prod.toml:/app/config.toml:ro \
  -v /path/to/your/projects:/projects \
  claude-crew
```

### Volumes

| Mount | Purpose |
|-------|---------|
| `/data` | SQLite database (named volume) |
| `/home/appuser/.claude` | Claude CLI session state and settings (named volume) |
| `/app/config.toml` | Configuration file (bind mount, read-only) |
| `/projects` | Project directories Claude works on (bind mount) |

## Tech Stack

- **Rust** with Tokio async runtime + tokio-util (CancellationToken)
- **poise** -- Discord bot framework (wraps serenity)
- **sqlx** -- SQLite for session persistence
- **dashmap** -- Lock-free concurrent session registry
- **smallvec** -- Inline stack storage for small collections
- **tokio::process** -- Claude Code subprocess management
- **tracing** -- Structured logging
- **serde + toml** -- Two-phase config (Raw TOML -> validated Arc<str>-backed)

## Architecture

See [PLAN.md](PLAN.md) for the full implementation guide including module structure, key types, and design decisions.

## Troubleshooting

| Problem | Fix |
|---------|-----|
| "Missing Access" on `/claude` | The bot needs **Create Public Threads** permission. Check both the bot role AND channel-level permission overrides -- if the channel denies thread creation for `@everyone`, the bot needs an explicit allow override on that channel |
| "The application did not respond" on `/claude` | Ensure the bot has Send Messages permission |
| Bot connects then disconnects with "Disallowed intents" | Enable **Message Content Intent** in Bot settings on the Developer Portal |
| Slash commands don't appear | Wait 1-2 minutes after first bot startup for Discord to register them globally |
| Bot doesn't respond to DMs | Make sure your user ID is in `auth.allowed_users` in config.toml |
| "failed to spawn claude" error | Ensure `claude` CLI is in PATH and authenticated. On NixOS, use an FHS wrapper script as `binary` |
| Bot responds but Claude output is empty | Check stderr logs -- Claude errors are now logged. Verify `default_cwd` is valid |
| Claude can't use tools (permission denied) | Add the tools to `allowed_tools` in config, or set `dangerously_skip_permissions = true` |
| Follow-up messages start new conversations | The bot now warns you when a session is expired/stopped and asks to confirm before starting a new one. Check logs for `claude_session_id` if issues persist |
| Sessions stuck after bot restart | Fixed: on startup the bot reconciles all "active" sessions to "idle", so the next message in the thread resumes normally |
| Ctrl+C doesn't work | Fixed in v0.2.0: Claude subprocesses now run in their own process group, so SIGINT only reaches the bot which handles graceful shutdown |
| "Invalid Form Body (name)" error | Thread name exceeded 100 chars -- this is now fixed with proper truncation |

## Health Warning

> [!CAUTION]
> **Vibe-coding is addictive. Protect your mental health.**
>
> This tool can easily turn you into a x100 developer. That feeling is intoxicating -- and
> therein lies the danger. Extended AI-assisted coding sessions (8+ hours) are associated with
> measurable cognitive and psychological harm. Managing multiple concurrent threads amplifies the risk.
>
> **What the research shows:**
>
> - **Brain changes from extended screen time** -- Excessive sessions reduce gray matter volume in the
>   prefrontal cortex (executive function, attention, working memory) and alter white matter integrity.
>   These effects compound with duration and frequency.
>   ([Neophytou et al., 2021](https://link.springer.com/article/10.1007/s11469-019-00182-2);
>   [Small et al., 2020](https://www.tandfonline.com/doi/full/10.31887/DCNS.2020.22.2/gsmall))
>
> - **Flow state becomes addictive without recovery** -- Flow is protective *only* when followed by
>   adequate rest. Without it, flow accelerates burnout and develops into obsessive, compulsive
>   engagement patterns. 39-83% of developers already exhibit burnout symptoms.
>   ([Aust & Beneke, 2022](https://www.mdpi.com/1660-4601/19/7/3865);
>   [Almeida et al., 2022](https://www.sciencedirect.com/science/article/pii/S0950584922002257))
>
> - **Technostress is a validated clinical construct** -- Five empirically identified stressors
>   (overload, invasion, complexity, insecurity, uncertainty) decrease job satisfaction and
>   cognitive performance.
>   ([Tarafdar et al., 2007](https://pubsonline.informs.org/doi/10.1287/isre.1070.0165);
>   [Fischer & Riedl, 2022](https://www.tandfonline.com/doi/full/10.1080/0960085X.2022.2154712))
>
> **What to do about it:**
>
> - **Nature exposure restores directed attention** -- As little as 10-15 minutes outdoors measurably
>   improves executive attention and working memory. A 90-minute nature walk reduces rumination and
>   decreases activity in brain regions linked to depression.
>   ([Berman et al., 2008](https://journals.sagepub.com/doi/abs/10.1111/j.1467-9280.2008.02225.x);
>   [Bratman et al., 2015](https://www.pnas.org/doi/10.1073/pnas.1510459112))
>
> - **Walking boosts creativity by 60%** -- Even short walks (indoors or outdoors) significantly
>   improve divergent thinking, with residual benefits persisting after sitting back down.
>   ([Oppezzo & Schwartz, 2014](https://pubmed.ncbi.nlm.nih.gov/24749966/))
>
> - **Take a full no-tech day off** between marathon sessions. Digital detox is not optional -- it is
>   maintenance. Your brain needs time in environments that provide what Kaplan's Attention
>   Restoration Theory calls "soft fascination" -- nature, not screens.
>   ([Kaplan, 1995](https://www.sciencedirect.com/science/article/abs/pii/0272494495900012);
>   [Stevenson et al., 2018](https://www.tandfonline.com/doi/full/10.1080/10937404.2016.1196155))
>
> **The rule of thumb:** For every deep vibe-coding session, schedule equal recovery time away from
> all screens. Go outside. Touch grass. Your future self -- and your prefrontal cortex -- will thank you.

## Roadmap

Potential future features:

- **File attachment support** -- Send files/images via Discord attachments for Claude to read
- **Reaction-based approval UI** -- Discord button components instead of text replies for permission prompts and user questions; collaborative sessions could require quorum approval
- **`/health` endpoint** -- HTTP health check for monitoring (lightweight Axum or Hyper)
- **GitLab merge request support** -- Add `glab` CLI support alongside `gh` for auto-PR on GitLab repositories
- **PR review integration** -- Post PR review comments from Discord; let participants approve/request changes via slash commands
- **Comprehensive test suite for CI/CD** -- End-to-end tests exercising both the Claude Code CLI integration (stream-json parsing, control_request/control_response protocol, permission flows) and the Discord layer (slash commands, thread lifecycle, ephemeral messages, reply routing)

## Related Projects

- [claude-code-discord](https://github.com/zebbern/claude-code-discord) -- TypeScript/Deno, uses Claude Agent SDK directly
- [claude-code-discord-bridge](https://github.com/ebibibi/claude-code-discord-bridge) -- TypeScript, thread-per-session with git worktrees
- [discord-agent-bridge](https://github.com/DoBuDevel/discord-agent-bridge) -- tmux polling approach
- [Claude-Code-Remote](https://github.com/JessyTsui/Claude-Code-Remote) -- Email/Discord/Telegram control

Inspired by the discussion at [anthropics/claude-code#15922](https://github.com/anthropics/claude-code/issues/15922).

## License

MIT
