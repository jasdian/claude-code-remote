#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BINARY_NAME="claude-crew"
TARGET="x86_64-unknown-linux-musl"
RELEASE_BIN="$REPO_ROOT/target/$TARGET/release/$BINARY_NAME"
LOG_FILE="$REPO_ROOT/claude-crew.log"

SKIP_BUILD=false
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=true ;;
        *) echo "Unknown arg: $arg"; exit 1 ;;
    esac
done

if [ "$SKIP_BUILD" = false ]; then
    echo "==> Building release binary (MUSL static)..."
    nix-shell "$REPO_ROOT/shell.nix" --run \
        "cargo build --release --no-default-features --target $TARGET --manifest-path $REPO_ROOT/Cargo.toml"
    if [ ! -f "$RELEASE_BIN" ]; then
        echo "ERROR: binary not found at $RELEASE_BIN"
        exit 1
    fi
    echo "==> Build OK: $(ls -lh "$RELEASE_BIN" | awk '{print $5}')"
fi

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

echo "==> Starting $BINARY_NAME..."
cd "$REPO_ROOT"
# The bot spawns `claude` subprocesses — on NixOS this needs the FHS wrapper in PATH.
# Ensure scripts/claude-wrapper.sh is set as `binary` in config.toml, or run inside nix-shell.
export PATH="$REPO_ROOT/scripts:$HOME/.local/bin:$PATH"
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
