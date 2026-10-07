#!/usr/bin/env node
// PreToolUse hook script for Claude's `--settings` (see core/claude-args.js's
// buildClaudeArgs, `readGuard` policies): fail-closed confinement of the
// Read/Glob/Grep tools to the job's worktree, wired in for both worktree
// policies (worktreeWrite and worktreeWriteNarrowBash) with the same root as
// core/edit-guard.js. The Bash sandbox's `denyRead` only mediates Bash, and
// `Read(...)` deny rules only cover the specific files they name — without
// this hook Claude's own Read tool could open any file the user can (a
// sibling repo, a dotfile nobody thought to list), and the PR content it's
// working from is untrusted input that may ask it to.
//
// Invoked as `<node> "<this file>" "<allowed root>"` (both shell-quoted —
// see buildClaudeArgs's shellQuoteSingle) for every call matching
// `Read|Glob|Grep`, with the hook's JSON payload on stdin and a JSON
// decision on stdout — the same wire protocol, and the same
// every-failure-denies rule, as core/edit-guard.js and core/mcp-guard.js
// (see mcp-guard.js's header for why only exit code 2 blocks on an
// internal error).
//
// Plain .js file (not .ts), same reason as edit-guard.js: it has to run
// standalone with plain `node`, no build step and no ts-node.
const fs = require("fs");

function readStdin() {
  try {
    return fs.readFileSync(0, "utf8");
  } catch {
    return "";
  }
}

function writeDenyJson(toolName) {
  const label = toolName || "(unknown tool)";
  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: `${label} outside the worktree is blocked by the companion's read guard`,
      },
    }),
  );
}

let toolName = "";

try {
  const root = process.argv[2];
  if (typeof root !== "string" || root.length === 0) {
    throw new Error("read-guard.js requires the allowed root as its first argument");
  }

  let payload = {};
  try {
    payload = JSON.parse(readStdin());
    if (payload && typeof payload.tool_name === "string" && payload.tool_name) {
      toolName = payload.tool_name;
    }
  } catch {
    // Malformed stdin: payload stays {} and the decision below denies.
  }

  // Inside the try so a missing policy module fails closed too.
  const { isReadToolCallAllowed } = require("./edit-path-policy.js");
  const input = payload && typeof payload.tool_input === "object" ? payload.tool_input : null;

  if (!isReadToolCallAllowed(root, toolName, input)) {
    writeDenyJson(toolName);
  }
  process.exit(0);
} catch {
  writeDenyJson(toolName);
  process.exit(2);
}
