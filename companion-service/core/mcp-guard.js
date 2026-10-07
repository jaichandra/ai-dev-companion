#!/usr/bin/env node
// PreToolUse hook script for Claude's `--settings` (see core/claude-args.js's
// buildClaudeArgs), wired in only for policies with `mcpGuard: true`
// (CLAUDE_POLICIES.readOnly). Claude invokes it as `<node> "<this file>"`
// (both shell-quoted — see buildClaudeArgs's shellQuoteSingle) for every
// tool call matching the `mcp__.*` matcher, piping the hook's JSON payload
// (which includes `tool_name`) on stdin, and reads a JSON decision back
// from stdout.
//
// Deliberately trivial and synchronous: this runs once per MCP tool call in
// the hot path of a headless Claude session, so it reads stdin, classifies,
// prints at most one line, and exits — no async, no logging, nothing that
// could hang the CLI waiting on this process.
//
// It is a plain .js file (not .ts) specifically so it can run standalone
// with plain `node`, with no build step and no ts-node — core/claude.ts
// resolves its path in dist/ with `path.join(__dirname, "mcp-guard.js")`
// (tsc's `allowJs` copies it there next to the compiled claude.js).
//
// FAIL CLOSED, not open: Claude Code treats ONLY hook exit code 2 as
// blocking a tool call — every other non-zero exit is non-blocking and the
// call proceeds exactly as if this hook hadn't run at all. That means any
// unexpected failure here (the classifier module failing to load, a thrown
// error mid-parse, anything) is exactly as dangerous as returning "allow"
// would be, so the whole body below runs inside one top-level try whose
// catch still denies (prints the same JSON) and exits 2 — never some other
// code that would silently let a mutating MCP call through.
const fs = require("fs");

/** Reads the hook's JSON payload from stdin, synchronously. No stdin
 * available (e.g. run by hand with none piped in) is treated the same as
 * malformed JSON below: an empty string, which the JSON.parse in the try
 * block below fails on. */
function readStdin() {
  try {
    return fs.readFileSync(0, "utf8");
  } catch {
    return "";
  }
}

/** Prints the exact PreToolUse deny JSON. `toolName` falls back to a
 * placeholder label so the reason string is never just "" — malformed
 * stdin, empty stdin, and this hook's own internal-error path all reach
 * here with no reliable tool name to report. */
function writeDenyJson(toolName) {
  const label = toolName || "(unknown tool)";
  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: `${label} is blocked by the companion's read-only policy`,
      },
    }),
  );
}

// Read outside the try's inner JSON.parse so the outer catch can still
// report a tool name if parsing succeeded but something later failed.
let toolName = "";

try {
  const raw = readStdin();
  try {
    const payload = JSON.parse(raw);
    if (typeof payload.tool_name === "string" && payload.tool_name) {
      toolName = payload.tool_name;
    }
  } catch {
    // Malformed stdin — toolName stays "", which classifyMcpTool denies
    // below exactly like an unrecognized tool name. Not re-thrown: this is
    // an expected input shape to fail closed on, not an internal error.
  }

  // Required inside the try, not at module top level, so a broken/missing
  // classifier module also fails closed through the same deny-and-exit-2
  // path below, rather than crashing before anything has printed.
  const { classifyMcpTool } = require("./mcp-tool-classifier.js");

  if (classifyMcpTool(toolName) === "deny") {
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
