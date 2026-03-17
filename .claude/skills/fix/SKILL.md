---
name: fix
description: "Run cargo fmt and cargo clippy --fix to auto-fix all formatting and lint warnings."
disable-model-invocation: false
---

# Fix — Auto-fix Formatting & Lint Warnings

Automatically fix all formatting issues and clippy warnings in the workspace.

## Instructions

1. Run `cargo fmt --all` to fix all formatting issues
2. Run `cargo clippy --workspace --all-targets --all-features --fix --allow-dirty --allow-staged -- -D warnings` to auto-fix clippy warnings
3. Run `cargo check --workspace` to verify the fixes compile
4. Report a summary of what was fixed (files changed, any remaining warnings that couldn't be auto-fixed)
5. If there are changes, commit them with a message like: `fix: auto-fix fmt and clippy warnings`
6. If the previous commit was also a fix commit from this skill, amend it instead of creating a new commit

If any clippy warnings cannot be auto-fixed, list them so the user can address them manually.

If `$ARGUMENTS` contains specific flags or targets, incorporate them into the commands.
