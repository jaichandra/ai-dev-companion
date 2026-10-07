#!/usr/bin/env node
// PreToolUse hook script for Claude's `--settings` (see core/claude-args.js's
// buildClaudeArgs's `editRoot` option), wired in for `worktreeWrite` runs.
// Fail-closed confinement of the Edit/Write/MultiEdit/NotebookEdit tools to
// one allowed root — the counterpart to the Bash sandbox
// (core/claude-args.js's isSandboxedPolicy), which only ever mediates Bash:
// live-verified that with `sandbox.enabled` and `filesystem.denyWrite` set,
// Claude's own Write tool still wrote a file outside the sandboxed cwd with
// no denial at all. An earlier attempt confined these tools with plain
// `--disallowedTools` permission-rule strings instead (the now-removed
// `worktreeGitEditDenyRules`) — those are glob-like string patterns with no
// filesystem resolution, and a broad-enough one (`Edit(//<home>/**)`) also
// matched paths INSIDE the worktree once the worktree lived under $HOME (as
// in production), denying Claude's own legitimate edits. This hook does
// real filesystem resolution instead (see core/edit-path-policy.js).
//
// Invoked as `<node> "<this file>" "<allowed root>"` (both shell-quoted —
// see buildClaudeArgs's shellQuoteSingle) for every tool call matching the
// `Edit|Write|MultiEdit|NotebookEdit` matcher, piping the hook's JSON
// payload on stdin, and reading a JSON decision back from stdout — same
// wire protocol as core/mcp-guard.js (see that file's header for exactly
// why only hook exit code 2 blocks a call in Claude Code, and why every
// unexpected failure here must reach the same deny-and-exit-2 path as a
// legitimate deny).
//
// Plain .js file (not .ts), same reason as mcp-guard.js: it has to run
// standalone with plain `node`, no build step and no ts-node.
const fs = require("fs");

function readStdin() {
  try {
    return fs.readFileSync(0, "utf8");
  } catch {
    return "";
  }
}

/** Prints the exact PreToolUse deny JSON. `toolName` falls back to a
 * placeholder label so the reason string is never just "" — malformed
 * stdin, a missing candidate path, and this hook's own internal-error
 * path can all reach here with no reliable tool name to report. */
function writeDenyJson(toolName) {
  const label = toolName || "(unknown tool)";
  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: `${label} outside the worktree (or touching .git) is blocked by the companion's edit guard`,
      },
    }),
  );
}

// Read outside the inner try's JSON.parse so the outer catch can still
// report a tool name if parsing succeeded but something later failed.
let toolName = "";

try {
  // Required inside the outer try: a missing/empty root is a
  // configuration error on the CALLER's side (buildClaudeArgs always
  // passes one when this hook is wired in) — fail closed via the
  // deny-and-exit-2 path, not a crash before anything has printed.
  const root = process.argv[2];
  if (typeof root !== "string" || root.length === 0) {
    throw new Error("edit-guard.js requires the allowed root as its first argument");
  }

  const raw = readStdin();
  let payload = {};
  try {
    payload = JSON.parse(raw);
    if (typeof payload.tool_name === "string" && payload.tool_name) {
      toolName = payload.tool_name;
    }
  } catch {
    // Malformed stdin — payload stays {}, so the candidate-path lookup
    // below finds nothing and denies, exactly like an unrecognized tool
    // call. Not re-thrown: this is an expected input shape to fail
    // closed on, not an internal error.
  }

  // Required inside the try, not at module top level, so a broken/missing
  // policy module also fails closed through the same deny-and-exit-2
  // path below, rather than crashing before anything has printed.
  const { isEditAllowed } = require("./edit-path-policy.js");

  const input = (payload && typeof payload.tool_input === "object" && payload.tool_input) || {};
  // Write/Edit/MultiEdit use `file_path`; NotebookEdit uses `notebook_path`.
  const candidate =
    typeof input.file_path === "string"
      ? input.file_path
      : typeof input.notebook_path === "string"
        ? input.notebook_path
        : null;

  if (candidate === null || !isEditAllowed(root, candidate)) {
    writeDenyJson(toolName);
  }
  // Exit 0 on either decision: on "allow" this hook has nothing to add and
  // leaves it to Claude's normal permission handling; on "deny" it already
  // printed the deny JSON above.
  process.exit(0);
} catch {
  writeDenyJson(toolName);
  process.exit(2);
}
