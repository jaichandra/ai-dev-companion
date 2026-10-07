const test = require("node:test");
const assert = require("node:assert/strict");
const path = require("path");
const { spawnSync } = require("child_process");

const GUARD_PATH = path.join(__dirname, "bash-guard.js");
const PREFIXES = JSON.stringify(["git diff", "git status", "npm test"]);

/** Runs the real guard as a child process, exactly how Claude's PreToolUse
 * hook invokes it: `<node> '<guard>' '<JSON prefixes>'`, hook JSON on stdin. */
function runGuard(args, input) {
  return spawnSync(process.execPath, [GUARD_PATH, ...args], { input, encoding: "utf8" });
}
const bash = (command) => JSON.stringify({ tool_name: "Bash", tool_input: { command } });

const DENY = {
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason:
      "Bash is limited to git diff, git status and the configured check commands, run as one plain command " +
      "(no ; & | ` $ ( ) < > or line breaks) — blocked by the companion's bash guard",
  },
};

test("bash-guard.js allows an allowed command: exit 0, no output", () => {
  for (const cmd of ["git diff --stat", "git status", "npm test"]) {
    const r = runGuard([PREFIXES], bash(cmd));
    assert.equal(r.status, 0, cmd);
    assert.equal(r.stdout, "", cmd);
  }
});

test("bash-guard.js denies a command outside the allowlist or chained onto it: exit 0, exact deny JSON", () => {
  for (const cmd of ["touch probe-made.txt && echo MADE", "git diff; rm x", "git status | sh", "git diffx", "rm -rf ."]) {
    const r = runGuard([PREFIXES], bash(cmd));
    assert.equal(r.status, 0, cmd);
    assert.deepEqual(JSON.parse(r.stdout), DENY, cmd);
  }
});

test("bash-guard.js denies malformed stdin, empty stdin, and a missing command", () => {
  for (const input of ["{not json", "", JSON.stringify({ tool_name: "Bash", tool_input: {} }), JSON.stringify({ tool_name: "Bash" })]) {
    const r = runGuard([PREFIXES], input);
    assert.equal(r.status, 0, input);
    assert.deepEqual(JSON.parse(r.stdout), DENY, input);
  }
});

test("bash-guard.js fails closed (deny JSON, exit 2) on a missing or bad prefixes argument", () => {
  for (const args of [[], [""], ["not json"], ["{}"], ["[]"], ['["git diff", 3]'], ['[""]']]) {
    const r = runGuard(args, bash("git diff"));
    assert.equal(r.status, 2, JSON.stringify(args));
    assert.deepEqual(JSON.parse(r.stdout), DENY, JSON.stringify(args));
  }
});
