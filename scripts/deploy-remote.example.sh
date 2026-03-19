#!/usr/bin/env bash
set -euo pipefail

# Deploy claude-crew to a remote server (full lifecycle)
# Usage: ./scripts/deploy-remote.sh
#        ./scripts/deploy-remote.sh --skip-build   (reuse last binary)
#        ./scripts/deploy-remote.sh --setup-only   (prepare host only)

# ── Connection ────────────────────────────────────────────────────────
SERVER="root@YOUR_SERVER_IP"
SSH_PORT="22"
REMOTE_USER="claude-crew"
REMOTE_DIR="/home/${REMOTE_USER}"
BINARY_NAME="claude-crew"
SERVICE_NAME="claude-crew"
TARGET="x86_64-unknown-linux-musl"

SSH_CMD="ssh -p ${SSH_PORT} ${SERVER}"
SCP_CMD="scp -P ${SSH_PORT}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RELEASE_BIN="${REPO_ROOT}/target/${TARGET}/release/${BINARY_NAME}"

SKIP_BUILD=false
SETUP_ONLY=false
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=true ;;
        --setup-only) SETUP_ONLY=true ;;
        *) echo "Unknown arg: $arg"; exit 1 ;;
    esac
done

# ── 1. Setup remote host (idempotent) ────────────────────────────────
echo "==> Setting up remote host..."
$SSH_CMD "
# Create dedicated user (no login shell)
if ! id -u ${REMOTE_USER} &>/dev/null; then
    useradd -r -m -s /usr/sbin/nologin ${REMOTE_USER}
    echo '    Created user ${REMOTE_USER}'
else
    echo '    User ${REMOTE_USER} already exists'
fi

# Working directories
mkdir -p ${REMOTE_DIR}/projects ${REMOTE_DIR}/data
chown -R ${REMOTE_USER}:${REMOTE_USER} ${REMOTE_DIR}

# Claude credentials directory
mkdir -p ${REMOTE_DIR}/.claude
chown ${REMOTE_USER}:${REMOTE_USER} ${REMOTE_DIR}/.claude
chmod 700 ${REMOTE_DIR}/.claude

# Install Claude Code CLI for the user (if not present)
if [ ! -f ${REMOTE_DIR}/.local/bin/claude ]; then
    echo '    Installing Claude Code CLI...'
    su -s /bin/bash ${REMOTE_USER} -c 'curl -fsSL https://claude.ai/install.sh | bash'
else
    echo '    Claude CLI already installed'
fi
"

if [ "$SETUP_ONLY" = true ]; then
    echo "==> Setup-only mode: done."
    exit 0
fi

# ── 2. Build MUSL release binary ─────────────────────────────────────
if [ "$SKIP_BUILD" = false ]; then
    echo "==> Building release binary (MUSL static)..."
    nix-shell "${REPO_ROOT}/shell.nix" --run \
        "cargo build --release --no-default-features --target ${TARGET} --manifest-path ${REPO_ROOT}/Cargo.toml"

    if [ ! -f "$RELEASE_BIN" ]; then
        echo "ERROR: binary not found at $RELEASE_BIN"
        exit 1
    fi
    echo "==> Build OK: $(du -h "$RELEASE_BIN" | cut -f1)"
fi

# ── 3. Copy files to remote ──────────────────────────────────────────
echo "==> Copying binary..."
$SCP_CMD "$RELEASE_BIN" "${SERVER}:${REMOTE_DIR}/${BINARY_NAME}"
$SSH_CMD "chmod 755 ${REMOTE_DIR}/${BINARY_NAME} && chown ${REMOTE_USER}:${REMOTE_USER} ${REMOTE_DIR}/${BINARY_NAME}"

echo "==> Copying production config..."
PROD_CONFIG="${REPO_ROOT}/config.toml"
if [ ! -f "$PROD_CONFIG" ]; then
    echo "ERROR: config.toml not found."
    exit 1
fi
$SCP_CMD "$PROD_CONFIG" "${SERVER}:${REMOTE_DIR}/config.toml"
$SSH_CMD "chmod 600 ${REMOTE_DIR}/config.toml && chown ${REMOTE_USER}:${REMOTE_USER} ${REMOTE_DIR}/config.toml"

echo "==> Copying Claude credentials..."
LOCAL_CREDS="$HOME/.claude/.credentials.json"
if [ -f "$LOCAL_CREDS" ]; then
    $SCP_CMD "$LOCAL_CREDS" "${SERVER}:${REMOTE_DIR}/.claude/.credentials.json"
    $SSH_CMD "chmod 600 ${REMOTE_DIR}/.claude/.credentials.json && chown ${REMOTE_USER}:${REMOTE_USER} ${REMOTE_DIR}/.claude/.credentials.json"
    echo "    Credentials copied."
else
    echo "    WARNING: No local credentials at $LOCAL_CREDS"
    echo "    Set ANTHROPIC_API_KEY in the service environment instead."
fi

# ── 4. Create systemd service ────────────────────────────────────────
echo "==> Writing ${SERVICE_NAME}.service..."

UNIT_FILE="/etc/systemd/system/${SERVICE_NAME}.service"

$SSH_CMD "[ -f '${UNIT_FILE}' ] && cp '${UNIT_FILE}' '${UNIT_FILE}.bak'; cat > '${UNIT_FILE}'" <<EOF
[Unit]
Description=Claude Crew Discord Bot
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${REMOTE_USER}
WorkingDirectory=${REMOTE_DIR}
ExecStart=${REMOTE_DIR}/${BINARY_NAME}
Restart=on-failure
RestartSec=5

# Claude CLI needs to be in PATH
Environment=PATH=${REMOTE_DIR}/.local/bin:/usr/local/bin:/usr/bin:/bin
# Uncomment and set if using API key instead of OAuth credentials:
# Environment=ANTHROPIC_API_KEY=sk-ant-...

StandardOutput=journal
StandardError=journal
SyslogIdentifier=${SERVICE_NAME}

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=${REMOTE_DIR}
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

# ── 5. Enable & restart ──────────────────────────────────────────────
echo "==> Enabling and restarting service..."
$SSH_CMD "
systemctl daemon-reload
systemctl enable ${SERVICE_NAME}.service
systemctl restart ${SERVICE_NAME}.service
sleep 3
systemctl status ${SERVICE_NAME}.service --no-pager || true
echo ''
echo '==> Last 20 journal lines:'
journalctl -u ${SERVICE_NAME}.service --no-pager -n 20
"

echo "==> Deploy complete."
echo "    Logs:   ssh -p ${SSH_PORT} ${SERVER##*@} journalctl -fu ${SERVICE_NAME}"
echo "    Status: ssh -p ${SSH_PORT} ${SERVER##*@} systemctl status ${SERVICE_NAME}"
