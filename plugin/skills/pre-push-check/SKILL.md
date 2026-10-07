---
name: pre-push-check
description: Before a commit or push, check the changed files against the team's shared test history with the local AI Dev Companion companion — low, medium or high risk, each line citing the builds and tests behind it. Use when the user asks whether a change is risky or before pushing.
allowed-tools: Bash(companion status:*), Bash(companion precheck:*), Bash(git status:*), Bash(git diff --name-only:*)
---

# Pre-push check

1. Run `companion status`. If the service **isn't running** (or `companion` is not found),
   tell the user **"companion service not running"**, suggest `companion doctor` (if `companion` is not found, `node ~/ai-dev-companion/companion-service/bin/companion.js doctor`), and stop.
2. Run `companion precheck` for the uncommitted changes, or `companion precheck <file>…`
   for the files the user names (repo-relative paths).
3. Report the level and every cited line as printed. "Skipped" means the shared test history
   isn't set up or can't be read — say so; it is not a pass. "Not enough data" is not a pass either.

The check is advisory and never blocks a push. It uses no AI: it only compares file names
with the shared risk facts.
