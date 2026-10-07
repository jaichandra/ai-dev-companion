#!/usr/bin/env node
// PreToolUse hook script for Claude's `--settings` (see core/claude-args.js's
// buildClaudeArgs), wired in with matcher `Bash` for policies with
// `bashGuard: true` (worktreeWriteNarrowBash). It enforces the Bash
// allowlist that `--allowedTools` alone does not: under
// `--permission-mode bypassPermissions`, a `Bash(git diff:*)` rule only
// pre-approves matching calls and restricts nothing — live-verified that
// `touch probe-made.txt && echo MADE` ran with zero denials. The decision
// itself is core/bash-command-policy.js.
//
// Invoked as `<node> '<this file>' '<JSON array of allowed prefixes>'`
// (shell-quoted — see buildClaudeArgs's shellQuoteSingle), with the hook's
// JSON payload on stdin; same wire protocol and fail-closed contract as
// core/edit-guard.js: deny JSON + exit 0 for an ordinary deny, deny JSON +
// exit 2 for anything unexpected (a bad argument, a broken policy module),
// since only exit 2 blocks a call if the JSON itself isn't honored.
//
// Plain .js file (not .ts): it has to run standalone with plain `node`.
const fs = require("fs");

function readStdin() {
  try {
    return fs.readFileSync(0, "utf8");
  } catch {
    return "";
  }
}

function writeDenyJson() {
  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason:
          "Bash is limited to git diff, git status and the configured check commands, run as one plain command " +
          "(no ; & | ` $ ( ) < > or line breaks) — blocked by the companion's bash guard",
      },
    }),
  );
}

try {
  // A missing/malformed prefix list is the CALLER's configuration error
  // (buildClaudeArgs always passes one) — fail closed through exit 2.
  const prefixes = JSON.parse(process.argv[2] || "");
  if (
    !Array.isArray(prefixes) ||
    prefixes.length === 0 ||
    !prefixes.every((p) => typeof p === "string" && p.trim().length > 0)
  ) {
    throw new Error("bash-guard.js requires a non-empty JSON array of allowed command prefixes as its argument");
  }

  let payload = {};
  try {
    payload = JSON.parse(readStdin());
  } catch {
    // Malformed stdin: no command to find below, so it denies (exit 0),
    // exactly like a call with no command.
  }

  const { isBashCommandAllowed } = require("./bash-command-policy.js");
  const input = (payload && typeof payload.tool_input === "object" && payload.tool_input) || {};
  if (!isBashCommandAllowed(input.command, prefixes)) {
    writeDenyJson();
  }
  process.exit(0);
} catch {
  writeDenyJson();
  process.exit(2);
}
