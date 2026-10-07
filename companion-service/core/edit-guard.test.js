const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");

const GUARD_PATH = path.join(__dirname, "edit-guard.js");

/** Runs the real guard script as a real child process (not a `require` of
 * its internals) with `root` as its argv[2] — exactly how Claude's
 * PreToolUse hook invokes it: `<node> "<guard path>" "<root>"` with the
 * hook JSON piped on stdin. */
function runGuard(root, input) {
  return spawnSync(process.execPath, [GUARD_PATH, root], { input, encoding: "utf8" });
}

function makeRoot() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "edit-guard-test-"));
  return fs.realpathSync(dir);
}

test("edit-guard.js allows an Edit inside root: exit 0, no output", () => {
  const root = makeRoot();
  const file = path.join(root, "a.txt");
  fs.writeFileSync(file, "hi");
  const result = runGuard(root, JSON.stringify({ tool_name: "Edit", tool_input: { file_path: file } }));
  assert.equal(result.status, 0);
  assert.equal(result.stdout, "");
});

test("edit-guard.js allows a Write to a brand-new file inside root: exit 0, no output", () => {
  const root = makeRoot();
  const file = path.join(root, "new.txt");
  const result = runGuard(root, JSON.stringify({ tool_name: "Write", tool_input: { file_path: file } }));
  assert.equal(result.status, 0);
  assert.equal(result.stdout, "");
});

test("edit-guard.js denies a Write outside root: exit 0, exact deny JSON", () => {
  const root = makeRoot();
  const outside = path.join(os.tmpdir(), "edit-guard-test-outside.txt");
  const result = runGuard(root, JSON.stringify({ tool_name: "Write", tool_input: { file_path: outside } }));
  assert.equal(result.status, 0);
  assert.deepEqual(JSON.parse(result.stdout), {
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason: "Write outside the worktree (or touching .git) is blocked by the companion's edit guard",
    },
  });
});

test("edit-guard.js denies an Edit touching a .git path inside root", () => {
  const root = makeRoot();
  fs.mkdirSync(path.join(root, ".git"));
  const hookFile = path.join(root, ".git", "hooks");
  fs.mkdirSync(hookFile);
  const target = path.join(hookFile, "pre-commit");
  const result = runGuard(root, JSON.stringify({ tool_name: "Edit", tool_input: { file_path: target } }));
  assert.equal(result.status, 0);
  assert.equal(JSON.parse(result.stdout).hookSpecificOutput.permissionDecision, "deny");
});

test("edit-guard.js resolves NotebookEdit's notebook_path field", () => {
  const root = makeRoot();
  const outside = path.join(os.tmpdir(), "edit-guard-test-outside.ipynb");
  const result = runGuard(
    root,
    JSON.stringify({ tool_name: "NotebookEdit", tool_input: { notebook_path: outside } }),
  );
  assert.equal(result.status, 0);
  assert.equal(JSON.parse(result.stdout).hookSpecificOutput.permissionDecision, "deny");
});

test("edit-guard.js denies malformed stdin, with an (unknown tool) reason, exit 0", () => {
  const root = makeRoot();
  const result = runGuard(root, "not json");
  assert.equal(result.status, 0);
  assert.deepEqual(JSON.parse(result.stdout), {
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason: "(unknown tool) outside the worktree (or touching .git) is blocked by the companion's edit guard",
    },
  });
});

test("edit-guard.js denies empty stdin the same way", () => {
  const root = makeRoot();
  const result = runGuard(root, "");
  assert.equal(result.status, 0);
  const parsed = JSON.parse(result.stdout);
  assert.equal(parsed.hookSpecificOutput.permissionDecision, "deny");
});

test("edit-guard.js fails closed (exit 2) when no allowed root is given", () => {
  const result = spawnSync(process.execPath, [GUARD_PATH], {
    input: JSON.stringify({ tool_name: "Edit", tool_input: { file_path: "/tmp/x" } }),
    encoding: "utf8",
  });
  assert.equal(result.status, 2);
  assert.equal(JSON.parse(result.stdout).hookSpecificOutput.permissionDecision, "deny");
});

test("edit-guard.js denies a tool_input with no file_path or notebook_path", () => {
  const root = makeRoot();
  const result = runGuard(root, JSON.stringify({ tool_name: "Edit", tool_input: {} }));
  assert.equal(result.status, 0);
  assert.equal(JSON.parse(result.stdout).hookSpecificOutput.permissionDecision, "deny");
});

test("edit-guard.js denies a Write through a dangling symlink that points outside root", () => {
  const root = makeRoot();
  const link = path.join(root, "link");
  fs.symlinkSync(path.join("..", "edit-guard-outside-new.txt"), link);
  const result = runGuard(root, JSON.stringify({ tool_name: "Write", tool_input: { file_path: link } }));
  assert.equal(result.status, 0);
  assert.equal(JSON.parse(result.stdout).hookSpecificOutput.permissionDecision, "deny");
});
