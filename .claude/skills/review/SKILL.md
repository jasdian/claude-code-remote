---
name: review
description: "Review recent code changes for quality, security, and conventions."
disable-model-invocation: false
---

# Review — Code Quality & Security Review

Launch the **reviewer** agent to analyze recent code changes.

## Instructions

1. Run `git diff` and `git diff --cached` to identify all changes
2. If `$ARGUMENTS` specifies files or a scope, focus the review on that
3. Delegate to the **reviewer** agent with the identified scope
4. Present the reviewer's findings to the user

The reviewer agent checks for:
- Code quality: readability, naming, duplication, error handling
- Engineering principles: P1-P6 compliance (borrow flow, functional patterns, memory, async IO, locality, type design)
- Security: OWASP top 10, secrets exposure, input validation, command injection
- Project conventions: CLAUDE.md compliance, poise/serenity/sqlx patterns
- Dead code, unused imports, missing error handling

Output is prioritized as: critical / warning / suggestion.
