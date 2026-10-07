const test = require("node:test");
const assert = require("node:assert/strict");
const path = require("path");
const { spawnSync } = require("child_process");

const GUARD_PATH = path.join(__dirname, "mcp-guard.js");

/** Runs the real guard script as a real child process (not a `require` of
 * its internals) — this is exactly how Claude's PreToolUse hook invokes
 * it: `<node> "<guard path>"` with the hook JSON piped on stdin. */
function runGuard(input) {
  return spawnSync(process.execPath, [GUARD_PATH], { input, encoding: "utf8" });
}

test("mcp-guard.js allows a read-only MCP tool: exit 0, no output", () => {
  const result = runGuard(JSON.stringify({ tool_name: "mcp__acme-jira-confluence__jira_get_issue" }));
  assert.equal(result.status, 0);
  assert.equal(result.stdout, "");
});

test("mcp-guard.js denies a mutating MCP tool: exit 0, exact deny JSON", () => {
  const result = runGuard(JSON.stringify({ tool_name: "mcp__acme-jenkins-dii__stop_build" }));
  assert.equal(result.status, 0);
  assert.deepEqual(JSON.parse(result.stdout), {
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason:
        "mcp__acme-jenkins-dii__stop_build is blocked by the companion's read-only policy",
    },
  });
});

test("mcp-guard.js denies malformed stdin, with an (unknown tool) reason, exit 0", () => {
  const result = runGuard("not json");
  assert.equal(result.status, 0);
  assert.deepEqual(JSON.parse(result.stdout), {
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason: "(unknown tool) is blocked by the companion's read-only policy",
    },
  });
});

test("mcp-guard.js denies empty stdin the same way", () => {
  const result = runGuard("");
  assert.equal(result.status, 0);
  const parsed = JSON.parse(result.stdout);
  assert.equal(parsed.hookSpecificOutput.permissionDecision, "deny");
  assert.equal(
    parsed.hookSpecificOutput.permissionDecisionReason,
    "(unknown tool) is blocked by the companion's read-only policy",
  );
});

test("mcp-guard.js denies a tool_name that parses but isn't a string", () => {
  const result = runGuard(JSON.stringify({ tool_name: 12345 }));
  assert.equal(result.status, 0);
  assert.deepEqual(JSON.parse(result.stdout), {
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason: "(unknown tool) is blocked by the companion's read-only policy",
    },
  });
});

// A forced internal-error path (require failure, a thrown error mid-parse)
// would need to reach into the guard's own module resolution — e.g.
// temporarily hiding mcp-tool-classifier.js from disk, or adding an
// env-var-triggered throw that only exists for tests. Both are exactly the
// kind of production-code contortion the review explicitly said to skip;
// the fail-closed catch (deny + exit 2) is covered by code review of
// core/mcp-guard.js's top-level try/catch instead.
