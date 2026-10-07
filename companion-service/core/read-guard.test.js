const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");

const GUARD_PATH = path.join(__dirname, "read-guard.js");

/** Runs the real guard script as a child process, exactly as Claude's
 * PreToolUse hook does: `<node> "<guard path>" "<root>"`, JSON on stdin. */
function runGuard(root, input) {
  return spawnSync(process.execPath, [GUARD_PATH, root], { input, encoding: "utf8" });
}

function makeRoot() {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "read-guard-test-")));
}

const DENY = (label) => ({
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: `${label} outside the worktree is blocked by the companion's read guard`,
  },
});

test("read-guard.js allows a Read inside root: exit 0, no output", () => {
  const root = makeRoot();
  const file = path.join(root, "a.txt");
  fs.writeFileSync(file, "hi");
  const r = runGuard(root, JSON.stringify({ tool_name: "Read", tool_input: { file_path: file } }));
  assert.equal(r.status, 0);
  assert.equal(r.stdout, "");
});

test("read-guard.js denies a Read outside root with the exact deny JSON", () => {
  const root = makeRoot();
  const r = runGuard(root, JSON.stringify({ tool_name: "Read", tool_input: { file_path: "/etc/hosts" } }));
  assert.equal(r.status, 0);
  assert.deepEqual(JSON.parse(r.stdout), DENY("Read"));
});

test("read-guard.js allows Glob/Grep with no path, denies one with a path outside root", () => {
  const root = makeRoot();
  for (const tool of ["Glob", "Grep"]) {
    const ok = runGuard(root, JSON.stringify({ tool_name: tool, tool_input: { pattern: "*.js" } }));
    assert.equal(ok.status, 0, tool);
    assert.equal(ok.stdout, "", tool);
    const bad = runGuard(root, JSON.stringify({ tool_name: tool, tool_input: { pattern: "*", path: os.homedir() } }));
    assert.equal(bad.status, 0, tool);
    assert.deepEqual(JSON.parse(bad.stdout), DENY(tool));
  }
});

test("read-guard.js denies a Glob whose pattern climbs out with ..", () => {
  const root = makeRoot();
  const r = runGuard(root, JSON.stringify({ tool_name: "Glob", tool_input: { pattern: "../../**/*" } }));
  assert.deepEqual(JSON.parse(r.stdout), DENY("Glob"));
});

test("read-guard.js denies malformed stdin with an (unknown tool) reason", () => {
  const root = makeRoot();
  const r = runGuard(root, "not json");
  assert.equal(r.status, 0);
  assert.deepEqual(JSON.parse(r.stdout), DENY("(unknown tool)"));
});

test("read-guard.js fails closed (exit 2) with no root argument", () => {
  const r = spawnSync(process.execPath, [GUARD_PATH], {
    input: JSON.stringify({ tool_name: "Read", tool_input: { file_path: "/tmp/x" } }),
    encoding: "utf8",
  });
  assert.equal(r.status, 2);
  assert.equal(JSON.parse(r.stdout).hookSpecificOutput.permissionDecision, "deny");
});
