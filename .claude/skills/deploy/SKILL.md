---
name: deploy
description: "Build release binary, deploy to server, restart service."
disable-model-invocation: true
---

# Deploy — Build, Ship, Restart

Deploy the claude-crew bot to production.

## Steps

### 1. Pre-flight Check
Run **scout** agent first for cheap state check (git status, uncommitted changes, current branch).
Then run `/test` to verify everything passes. Do NOT proceed if tests fail.

### 2. Build Release Binary
```bash
cargo build --release --no-default-features --target x86_64-unknown-linux-musl
```

### 3. Confirm with User
Before deploying, show:
- Binary size and path
- Target server (from config or ask user)
- Current service status on remote

**Wait for explicit user confirmation before proceeding.**

### 4. Deploy via SSH
```bash
# Copy binary
scp target/x86_64-unknown-linux-musl/release/claude-crew user@server:/path/

# Copy config if changed
scp config.toml user@server:/path/config.toml
```

Adjust paths based on user input or existing deployment config.

### 5. Restart Service
```bash
ssh user@server "sudo systemctl restart claude-crew"
```

### 6. Verify
```bash
ssh user@server "systemctl status claude-crew"
ssh user@server "journalctl -u claude-crew -n 20 --no-pager"
```

Check that the bot connects to Discord (look for "bot ready" in logs).

### 7. Report
- Version/commit deployed
- Service status (running/failed)
- Bot online status (connected to Discord gateway)
- Any warnings from startup logs
