const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const { isLinkedWorktreeOf } = require("./worktree-registry.js");

const git = (cwd, ...args) => execFileSync("git", args, { cwd, encoding: "utf8" }).trim();

test("only a linked worktree git registered for the repo counts", (t) => {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "wt-registry-")));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const repo = path.join(root, "app");
  fs.mkdirSync(repo);
  git(repo, "init", "-q", "-b", "master");
  git(repo, "config", "user.email", "t@example.com");
  git(repo, "config", "user.name", "T");
  fs.writeFileSync(path.join(repo, "a"), "a");
  git(repo, "add", "a");
  git(repo, "commit", "-q", "-m", "i");
  const tree = path.join(root, "elsewhere", "tree");
  git(repo, "worktree", "add", "-q", "-b", "feat", tree);

  assert.equal(isLinkedWorktreeOf(repo, tree), true);
  assert.equal(isLinkedWorktreeOf(repo, repo), false, "the clone itself is not a linked worktree");
  assert.equal(isLinkedWorktreeOf(repo, path.join(root, "elsewhere")), false);
  assert.equal(isLinkedWorktreeOf(repo, "relative/tree"), false);
  assert.equal(isLinkedWorktreeOf(path.join(root, "no-repo"), tree), false);
  assert.equal(isLinkedWorktreeOf(undefined, tree), false);
  fs.rmSync(tree, { recursive: true, force: true });
  // Removed outside git: the registration is still there, but the folder is gone.
  assert.equal(isLinkedWorktreeOf(repo, tree), true);
});
