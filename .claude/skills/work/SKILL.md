---
name: work
description: "Full-cycle team dispatch: clarify -> research -> plan -> execute (architect agent for hard tasks) -> verify (/test) -> review (/review) -> finalize. Orchestrates the entire agent team for complex multi-step tasks."
disable-model-invocation: true
allowed-tools: Read, Write, Edit, Grep, Glob, Bash, Agent, Skill, WebSearch, WebFetch, AskUserQuestion, TaskCreate, TaskUpdate, TaskList, TaskGet
argument-hint: "[what you want to build/fix/refactor]"
---

# Work — Full-Cycle Team Dispatch

End-to-end: clarify -> research -> plan -> execute -> verify -> review -> finalize.

> $ARGUMENTS

## CRITICAL RULES

1. **ASK QUESTIONS** — Use `AskUserQuestion` when scope, approach, or priorities are unclear. Clarify FIRST, act SECOND.
2. **USE TASKS** — Track phases via `TaskCreate`/`TaskUpdate`. This maintains rhythm and shows progress.
3. **DISPATCH AGENTS** — Hard implementation goes to `architect` agent (opus). Verification goes to `/test` skill. Review goes to `/review` skill.
4. **LOOP UNTIL DONE** — After each phase, check results and fix issues before moving on. Max 3 fix attempts per failure before asking the user.
5. **FOLLOW P1-P6** — All code must adhere to the 6 engineering principles in CLAUDE.md.

---

## Architecture: Agent & Skill Dispatch

```
YOU (work orchestrator)
|
+-- PHASE 1: CLARIFY (AskUserQuestion)
|
+-- PHASE 2: RESEARCH
|   +-- scout agent (haiku): fast targeted lookups first
|   +-- Explore agents: deep codebase scan if needed
|   +-- WebSearch: external docs/APIs if needed
|
+-- PHASE 3: PLAN (decompose tasks, present to user)
|
+-- PHASE 4: EXECUTE
|   +-- STANDARD tasks -> implement directly (main session)
|   +-- HARD tasks -> architect agent (opus, isolation: worktree)
|   +-- Independent HARD tasks -> parallel architects (each in own worktree)
|   +-- After each task:
|   |   +-- /test (verifier agent) — cargo check/clippy/test
|   |   +-- /review (reviewer agent) — code quality & security
|   |   +-- Fix loop (max 3x) if failures
|   +-- After parallel tasks: merge worktree branches -> /test all changes
|   +-- WebSearch on unfamiliar errors
|
+-- PHASE 5: FINALIZE
|   +-- Final /test
|   +-- Summary of changes
|   +-- Ask user about committing
|
+-- LOOP -> back to any phase as needed
```

---

## PHASE 0: Initialize

### 0.1 Quick Recon (scout agent — haiku, cheap)

Before anything else, dispatch the **scout** agent to cheaply gather project state:
```
Agent(subagent_type: "scout", prompt: "Quick project recon:
  1. git status and current branch
  2. Check if Cargo.toml exists and list key dependencies
  3. Count src/ files and test files
  4. Any uncommitted changes?
  Report in structured format.")
```

### 0.2 Parse Request

Extract from `$ARGUMENTS`:
- **What**: Feature, fix, refactor, optimization?
- **Where**: Which modules/files?
- **Why**: What problem does this solve?

### 0.3 Create Task List

**MANDATORY**: Set up tracking:

```
TaskCreate: "Clarify requirements"
TaskCreate: "Research codebase"
TaskCreate: "Plan implementation"
TaskCreate: "Execute tasks"
TaskCreate: "Final verification"
```

---

## PHASE 1: CLARIFY

Mark "Clarify requirements" as `in_progress`.

If `$ARGUMENTS` is clear and unambiguous, proceed directly. Otherwise:

Use `AskUserQuestion` to clarify:
- **Scope**: Which modules/files does this affect?
- **Approach**: Preferred approach if multiple options exist?
- **Priority**: Correctness first, minimal changes, or full refactor?

**DO NOT PROCEED until you know:**
- [ ] What exactly to build/fix
- [ ] Which parts of the codebase are affected
- [ ] Constraints and priorities

Mark task COMPLETE.

---

## PHASE 2: RESEARCH

Mark "Research codebase" as `in_progress`.

### 2.1 Quick Lookup (scout — haiku)

First, use the cheap **scout** agent for fast targeted lookups:
```
Agent(subagent_type: "scout", prompt: "Find files related to {topic}. Check: does {function} exist? Where is {module} defined? Report file paths and line numbers.")
```

Use scout results to decide if deeper Explore is needed.

### 2.2 Deep Codebase Research (Explore agents, parallel)

If scout results are insufficient, launch Explore agents in parallel:

```
Agent(subagent_type: "Explore", prompt: "Find all code related to {topic}...", run_in_background: true)
Agent(subagent_type: "Explore", prompt: "Find callers/dependents of {module}...", run_in_background: true)
```

### 2.3 External Research

If the request involves Discord API, Claude Code CLI, poise framework, or unfamiliar concepts:
```
WebSearch: "{concept} Rust implementation"
```

### 2.4 Synthesize

Collect results. Build understanding of:
- What code exists and how it's structured
- What needs to change
- What constraints exist (P1-P6 principles)

### 2.5 Research-Informed Questions

If research reveals options or ambiguities not covered in Phase 1:
```
AskUserQuestion: "I found that {module} uses {pattern}. Should we:
  A) Keep existing pattern and extend it
  B) Refactor to {better pattern} (affects N files)
  C) Something else"
```

Mark task COMPLETE.

---

## PHASE 3: PLAN

Mark "Plan implementation" as `in_progress`.

### 3.1 Decompose into Tasks

For each task, define:
- **Title**: What to do
- **Files**: Which files to modify/create
- **Difficulty**: STANDARD or HARD
- **Dependencies**: Which tasks must complete first
- **Success criteria**: How to verify it works

### 3.2 Difficulty Classification

| Difficulty | Handler | When |
|-----------|---------|------|
| STANDARD | Main session (you) | Single-file changes, config updates, straightforward additions |
| HARD | `architect` agent (opus) | Cross-module refactors, complex types/lifetimes, tricky bugs, async patterns, DashMap/CancellationToken flows |

### 3.3 Present Plan

Show the user:
- Task list with difficulty and file scope
- Execution order
- Any risks or trade-offs

**MANDATORY**: Wait for user approval before executing.

```
AskUserQuestion: "Plan has {N} tasks ({X} standard, {Y} hard). Ready to execute?"
  A) "Execute as-is"
  B) "Adjust plan first"
  C) "Show more detail"
  D) "Abort"
```

Mark task COMPLETE.

---

## PHASE 4: EXECUTE

Mark "Execute tasks" as `in_progress`.

### 4.1 Create Per-Task Tracking

```
TaskCreate per plan task: "{task_id}: {title}"
```

### 4.2 Execute Each Task

For each task in dependency order:

#### Step 1: IMPLEMENT

**STANDARD tasks** — implement directly in the main session.

**HARD tasks** — delegate to architect agent (runs in its own git worktree):
```
Agent(
  subagent_type: "architect",
  isolation: "worktree",
  description: "{task title}",
  prompt: "
    Task: {description}
    Files: {file list}
    Success criteria: {criteria}
    Context: {relevant findings from Phase 2}

    Read CLAUDE.md for project conventions and engineering principles P1-P6.
    Implement the minimal change that solves this correctly.
    Verify your work with cargo check/clippy/test before reporting back.
  "
)
```

The `isolation: "worktree"` flag gives each architect agent its own git worktree — an isolated copy of the repo. Changes are made on a separate branch. If the agent makes no changes, the worktree is auto-cleaned. If changes are made, the worktree path and branch are returned for merging.

#### Step 2: VERIFY — `/test`

After implementation, run verification:
```
Skill(skill: "test")
```

**If FAIL**: Fix the issue and re-verify. Max 3 attempts.
- Attempt 1-2: Fix directly (STANDARD) or re-dispatch architect (HARD)
- Attempt 3: Ask user for guidance

#### Step 3: REVIEW — `/review`

After tests pass:
```
Skill(skill: "review")
```

**Handle findings:**
- **Critical**: Fix immediately, re-run `/test`
- **Warning**: Note for user, continue unless it's a security issue
- **Suggestion**: Note for user, continue

### 4.3 Parallel Execution with Git Worktrees

Independent tasks (no shared files) run in parallel, each in its own worktree:
```
Agent(subagent_type: "architect", isolation: "worktree", prompt: "Task A...", run_in_background: true)
Agent(subagent_type: "architect", isolation: "worktree", prompt: "Task B...", run_in_background: true)
```

Each agent gets an isolated copy of the repo via `git worktree`. They can edit files simultaneously without conflicts. After both complete:

1. Review the returned worktree branches
2. Merge each branch into the working branch (resolve conflicts if any)
3. Run `/test` once for all merged changes

**Worktree lifecycle:**
- Created automatically when agent starts
- Agent works on its own branch in `.claude/worktrees/`
- Auto-cleaned if agent makes no changes
- Persists with branch name if changes were made
- Merge via `git merge` or cherry-pick from the returned branch

### 4.4 Error Recovery

| Error | Recovery |
|-------|----------|
| Compile error | Fix directly or re-dispatch architect |
| Test failure | Analyze failure, fix, re-verify |
| Architect agent stuck | Try different approach or ask user |
| Unfamiliar error | WebSearch for solution |

After 3 failed fix attempts on any task, escalate:
```
AskUserQuestion: "Task {id} failed verification 3 times. Last error: {error}. How to proceed?"
  A) "Let me look at it"
  B) "Skip this task"
  C) "Try different approach: {describe}"
  D) "Abort"
```

Mark task COMPLETE when all tasks done.

---

## PHASE 5: FINALIZE

Mark "Final verification" as `in_progress`.

### 5.1 Final Verification

Run `/test` one final time to confirm everything passes together.

### 5.2 Summary

Output:

```
---

**Work complete.**

**Goal**: {from $ARGUMENTS}

**Tasks**: {completed}/{total}
**Files modified**: {list with 1-line description each}
**Tests**: PASS / FAIL
**Review**: {clean / N findings noted}

**Key decisions**:
- {decision 1}
- {decision 2}

**Risks / TODOs**:
- {any remaining items}

---
```

### 5.3 Commit

```
AskUserQuestion: "Ready to commit these changes?"
  A) "Yes — commit"
  B) "Let me review first"
  C) "Make adjustments: {describe}"
```

- **Commit**: Create a commit with a descriptive message
- **Review**: Wait for user
- **Adjustments**: Loop back to Phase 4 for the specific changes

Mark task COMPLETE.

---

## Loop Control: When to Return to User

| Trigger | Action |
|---------|--------|
| Scope unclear | AskUserQuestion |
| Research reveals options | Present options |
| Plan ready | Get approval before executing |
| Task BLOCKED | Surface blocker |
| 3x fix failures | Escalate to user |
| All tasks done | Present summary |

**NEVER silently:**
- Skip a phase
- Make architectural decisions without presenting the plan
- Continue past repeated failures without reporting
- Commit without asking
