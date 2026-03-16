---
name: refactor
description: "Focused refactoring: research existing code, implement changes, verify."
disable-model-invocation: true
---

# Refactor — Focused Code Restructuring

Targeted refactoring with research, planning, and verification.

> $ARGUMENTS

## Steps

### 1. Research

Launch Explore agents to understand:
- Current implementation (read all affected files)
- All callers and dependents (grep for usage)
- Invariants that must be preserved (tests, API contracts)

### 2. Plan

Identify:
- What changes and why
- What must be preserved (public API, behavior, existing tests)
- Risks (breaking changes, performance impact)
- Difficulty: STANDARD (single module) or HARD (cross-module, complex types)

Present plan to user. **Wait for approval.**

### 3. Implement

- **STANDARD**: Implement directly
- **HARD**: Delegate to architect agent with full context (runs in isolated git worktree via `isolation: "worktree"`)

Ensure all changes follow P1-P6 engineering principles:
- P1: Borrow/consume flow (Arc<str>, &str, Cow)
- P2: Functional patterns (filter_map, and_then, early returns)
- P3: Memory efficiency (SmallVec, buffer reuse, Box<str>)
- P4: Async-only IO (tokio, CancellationToken, select!)
- P5: CPU locality (#[inline], hot/cold separation)
- P6: Type-driven design (newtypes, const fn)

### 4. Verify

Run `/test` to ensure nothing broke.

### 5. Review

Run `/review` to check quality.

### 6. Report

Output:
- Before/after comparison (what changed structurally)
- Principles applied (which P1-P6 rules guided the refactor)
- Risks or follow-up work needed
