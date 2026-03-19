#!/usr/bin/env bash
set -euo pipefail

# Deploy claude-crew to a remote server (full lifecycle)
# Usage: ./scripts/deploy-remote.sh                 (full deploy: setup + build + copy + systemd)
#        ./scripts/deploy-remote.sh --skip-build     (reuse last binary)
#        ./scripts/deploy-remote.sh --setup-only     (prepare host only)
#        ./scripts/deploy-remote.sh --restart         (quick: stop → build → copy binary+config → start)
#        ./scripts/deploy-remote.sh --restart --skip-build  (quick: reuse last binary)

# ── Connection ────────────────────────────────────────────────────────
SERVER="root@YOUR_SERVER_IP"
SSH_PORT="22"
REMOTE_USER="claude-crew"
REMOTE_DIR="/home/${REMOTE_USER}"
BINARY_NAME="claude-crew"
SERVICE_NAME="claude-crew"
TARGET="x86_64-unknown-linux-musl"

# Production config to deploy (gitignored — copy from config.example.toml)
PROD_CONFIG_NAME="config.toml"

SSH_CMD="ssh -p ${SSH_PORT} ${SERVER}"
SCP_CMD="scp -P ${SSH_PORT}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RELEASE_BIN="${REPO_ROOT}/target/${TARGET}/release/${BINARY_NAME}"
PROD_CONFIG="${REPO_ROOT}/${PROD_CONFIG_NAME}"

SKIP_BUILD=false
SETUP_ONLY=false
RESTART_ONLY=false
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=true ;;
        --setup-only) SETUP_ONLY=true ;;
        --restart)    RESTART_ONLY=true ;;
        *) echo "Unknown arg: $arg"; exit 1 ;;
    esac
done

# ── Quick restart mode ────────────────────────────────────────────────
if [ "$RESTART_ONLY" = true ]; then
    if [ "$SKIP_BUILD" = false ]; then
        echo "==> Building release binary (MUSL static)..."
        nix-shell "${REPO_ROOT}/shell.nix" --run \
            "cargo build --release --no-default-features --target ${TARGET} --manifest-path ${REPO_ROOT}/Cargo.toml"
        echo "==> Build OK: $(du -h "$RELEASE_BIN" | cut -f1)"
    fi

    echo "==> Stopping ${SERVICE_NAME}..."
    $SSH_CMD "systemctl stop ${SERVICE_NAME}.service"

    echo "==> Copying binary..."
    $SCP_CMD "$RELEASE_BIN" "${SERVER}:${REMOTE_DIR}/${BINARY_NAME}"
    $SSH_CMD "chmod 755 ${REMOTE_DIR}/${BINARY_NAME} && chown ${REMOTE_USER}:${REMOTE_USER} ${REMOTE_DIR}/${BINARY_NAME}"

    echo "==> Copying config..."
    $SCP_CMD "$PROD_CONFIG" "${SERVER}:${REMOTE_DIR}/config.toml"
    $SSH_CMD "chmod 600 ${REMOTE_DIR}/config.toml && chown ${REMOTE_USER}:${REMOTE_USER} ${REMOTE_DIR}/config.toml"

    echo "==> Starting ${SERVICE_NAME}..."
    $SSH_CMD "systemctl start ${SERVICE_NAME}.service && sleep 2 && journalctl -u ${SERVICE_NAME} --no-pager -n 15"

    echo "==> Restart complete."
    echo "    Logs:   ssh -p ${SSH_PORT} ${SERVER##*@} journalctl -fu ${SERVICE_NAME}"
    exit 0
fi

# ── 1. Setup remote host (idempotent) ────────────────────────────────
echo "==> Setting up remote host..."
$SSH_CMD "
if ! id -u ${REMOTE_USER} &>/dev/null; then
    useradd -r -m -s /usr/sbin/nologin ${REMOTE_USER}
    echo '    Created user ${REMOTE_USER}'
else
    echo '    User ${REMOTE_USER} already exists'
fi
mkdir -p ${REMOTE_DIR}/projects ${REMOTE_DIR}/data
chown -R ${REMOTE_USER}:${REMOTE_USER} ${REMOTE_DIR}
mkdir -p ${REMOTE_DIR}/.claude
chown ${REMOTE_USER}:${REMOTE_USER} ${REMOTE_DIR}/.claude
chmod 700 ${REMOTE_DIR}/.claude
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

# ── 3. Stop & copy files to remote ───────────────────────────────────
echo "==> Stopping ${SERVICE_NAME}..."
$SSH_CMD "systemctl stop ${SERVICE_NAME}.service || true"

echo "==> Copying binary..."
$SCP_CMD "$RELEASE_BIN" "${SERVER}:${REMOTE_DIR}/${BINARY_NAME}"
$SSH_CMD "chmod 755 ${REMOTE_DIR}/${BINARY_NAME} && chown ${REMOTE_USER}:${REMOTE_USER} ${REMOTE_DIR}/${BINARY_NAME}"

echo "==> Copying production config..."
if [ ! -f "$PROD_CONFIG" ]; then
    echo "ERROR: ${PROD_CONFIG_NAME} not found."
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
Environment=PATH=${REMOTE_DIR}/.local/bin:/usr/local/bin:/usr/bin:/bin
StandardOutput=journal
StandardError=journal
SyslogIdentifier=${SERVICE_NAME}
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=${REMOTE_DIR}
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

# ── 5. Enable & start ────────────────────────────────────────────────
echo "==> Enabling and starting service..."
$SSH_CMD "
systemctl daemon-reload
systemctl enable ${SERVICE_NAME}.service
systemctl start ${SERVICE_NAME}.service
sleep 3
systemctl status ${SERVICE_NAME}.service --no-pager || true
echo ''
echo '==> Last 20 journal lines:'
journalctl -u ${SERVICE_NAME}.service --no-pager -n 20
"

echo "==> Deploy complete."
echo "    Logs:   ssh -p ${SSH_PORT} ${SERVER##*@} journalctl -fu ${SERVICE_NAME}"
echo "    Status: ssh -p ${SSH_PORT} ${SERVER##*@} systemctl status ${SERVICE_NAME}"
