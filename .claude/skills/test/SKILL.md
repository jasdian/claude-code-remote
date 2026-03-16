---
name: test
description: "Run cargo check, clippy, and test suite with structured reporting."
disable-model-invocation: false
---

# Test — Build & Test Pipeline

Launch the **verifier** agent to run the full build verification pipeline.

## Instructions

1. Delegate to the **verifier** agent
2. The verifier runs in order, stopping on first failure:
   - `cargo check --workspace`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `cargo test --workspace`
3. Present the structured results table to the user

If `$ARGUMENTS` contains specific test names or flags, pass them to the verifier.

The verifier NEVER modifies code — it reports only. If failures are found, the user or calling workflow decides what to do.
