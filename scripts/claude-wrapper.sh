#!/usr/bin/env bash
# Wrapper to run claude inside NixOS FHS environment.
# claude-fhs's init script uses `exec "$@"`, so the first arg must be a binary path,
# not a flag like -p (which bash exec interprets as its own option).

# Resolve claude-fhs from PATH, nix profile, or known nix-shell location
CLAUDE_FHS="${CLAUDE_FHS:-$(command -v claude-fhs 2>/dev/null)}"
if [[ -z "$CLAUDE_FHS" ]]; then
    # Fallback: search nix store for claude-fhs binary
    CLAUDE_FHS="$(ls -d /nix/store/*-claude-fhs/bin/claude-fhs 2>/dev/null | head -1)"
fi

if [[ -z "$CLAUDE_FHS" || ! -x "$CLAUDE_FHS" ]]; then
    echo "claude-fhs not found" >&2
    exit 1
fi

exec "$CLAUDE_FHS" "$HOME/.local/bin/claude" "$@"
