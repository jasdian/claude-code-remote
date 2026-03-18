#!/usr/bin/env bash
# Run claude-crew on a fresh Ubuntu server (22.04+).
# Usage: ./scripts/run-ubuntu.sh [--skip-deps] [--skip-build]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BINARY_NAME="claude-crew"
RELEASE_BIN="$REPO_ROOT/target/release/$BINARY_NAME"
LOG_FILE="$REPO_ROOT/$BINARY_NAME.log"
CONFIG="$REPO_ROOT/config.toml"

SKIP_DEPS=false
SKIP_BUILD=false
for arg in "$@"; do
    case "$arg" in
        --skip-deps)  SKIP_DEPS=true ;;
        --skip-build) SKIP_BUILD=true ;;
        *) echo "Unknown arg: $arg"; exit 1 ;;
    esac
done

# ── 1. System dependencies ──────────────────────────────────────────
if [ "$SKIP_DEPS" = false ]; then
    echo "==> Installing system dependencies..."
    sudo apt-get update -qq
    sudo apt-get install -y --no-install-recommends \
        build-essential pkg-config libssl-dev git curl ca-certificates

    # Rust toolchain (skip if already present)
    if ! command -v cargo &>/dev/null; then
        echo "==> Installing Rust..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        # shellcheck disable=SC1091
        source "$HOME/.cargo/env"
    fi
    echo "    rustc $(rustc --version | awk '{print $2}')"

    # Claude Code CLI (skip if already present)
    if ! command -v claude &>/dev/null; then
        echo "==> Installing Claude Code CLI..."
        curl -fsSL https://claude.ai/install.sh | bash
        export PATH="$HOME/.local/bin:$PATH"
    fi
    echo "    claude $(claude --version 2>/dev/null || echo '(installed)')"

    # gh CLI for auto-PR feature (optional)
    if ! command -v gh &>/dev/null; then
        echo "==> Installing GitHub CLI (optional, for auto-PR)..."
        curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
            | sudo dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg
        echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
            | sudo tee /etc/apt/sources.list.d/github-cli.list > /dev/null
        sudo apt-get update -qq && sudo apt-get install -y gh
    fi
fi

# ── 2. Claude Code credentials check ────────────────────────────────
# Claude Code stores OAuth tokens in ~/.claude/.credentials.json
# and API keys can be set via ANTHROPIC_API_KEY env var.
#
# Authentication precedence:
#   1. ANTHROPIC_API_KEY env var          (API key from console.anthropic.com)
#   2. ~/.claude/.credentials.json        (OAuth — Claude Pro/Max/Teams subscription)
#   3. ANTHROPIC_AUTH_TOKEN env var        (proxy/gateway token)
#
# If using OAuth (Pro/Max subscription), run `claude login` first.
# If using an API key, export ANTHROPIC_API_KEY before running this script.
CREDS_FILE="$HOME/.claude/.credentials.json"
if [ -z "${ANTHROPIC_API_KEY:-}" ] && [ ! -f "$CREDS_FILE" ]; then
    echo ""
    echo "WARNING: No Claude credentials found."
    echo "  Option A: export ANTHROPIC_API_KEY='sk-ant-...'   (API key)"
    echo "  Option B: claude login                            (OAuth / Pro subscription)"
    echo ""
    read -rp "Continue anyway? [y/N] " yn
    [[ "$yn" =~ ^[Yy]$ ]] || exit 1
fi

# ── 3. Config check ─────────────────────────────────────────────────
if [ ! -f "$CONFIG" ]; then
    echo ""
    echo "ERROR: config.toml not found at $CONFIG"
    echo "  cp config.example.toml config.toml   # then edit with your values"
    exit 1
fi

# ── 4. Build ─────────────────────────────────────────────────────────
if [ "$SKIP_BUILD" = false ]; then
    echo "==> Building release binary..."
    cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
    if [ ! -f "$RELEASE_BIN" ]; then
        echo "ERROR: binary not found at $RELEASE_BIN"
        exit 1
    fi
    echo "==> Build OK: $(ls -lh "$RELEASE_BIN" | awk '{print $5}')"
fi

# ── 5. Stop existing instance ───────────────────────────────────────
echo "==> Stopping running $BINARY_NAME..."
if pkill -TERM -x "$BINARY_NAME" 2>/dev/null; then
    for i in $(seq 1 10); do
        if ! pgrep -x "$BINARY_NAME" >/dev/null 2>&1; then
            echo "    Stopped (${i}s)"
            break
        fi
        sleep 1
    done
    if pgrep -x "$BINARY_NAME" >/dev/null 2>&1; then
        echo "    Force killing..."
        pkill -9 -x "$BINARY_NAME" 2>/dev/null || true
        sleep 1
    fi
else
    echo "    No running process found"
fi

# ── 6. Start ─────────────────────────────────────────────────────────
echo "==> Starting $BINARY_NAME..."
cd "$REPO_ROOT"
export PATH="$HOME/.local/bin:$PATH"
nohup "$RELEASE_BIN" >> "$LOG_FILE" 2>&1 &
NEW_PID=$!
sleep 2
if kill -0 "$NEW_PID" 2>/dev/null; then
    echo "==> $BINARY_NAME started (PID $NEW_PID)"
    echo "    Log: $LOG_FILE"
    echo "    Tail: tail -f $LOG_FILE"
else
    echo "ERROR: process exited immediately. Last 20 lines:"
    tail -20 "$LOG_FILE"
    exit 1
fi
