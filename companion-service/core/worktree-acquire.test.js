// acquireWorktree reuses what exists and refuses what it can't safely use.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const git = (cwd, ...args) => execFileSync("git", args, { cwd, encoding: "utf8" }).trim();

function makeRepos() {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "wt-acquire-")));
  process.env.HOME = path.join(root, "home"); // before dist/ reads the state dir
  const seed = path.join(root, "seed");
  fs.mkdirSync(seed);
  git(seed, "init", "-q", "-b", "master");
  git(seed, "config", "user.email", "t@example.com");
  git(seed, "config", "user.name", "T");
  fs.writeFileSync(path.join(seed, "README.md"), "hi\n");
  git(seed, "add", "README.md");
  git(seed, "commit", "-q", "-m", "init");
  git(seed, "branch", "feature/PROJ-7-login");
  const origin = path.join(root, "origin.git");
  execFileSync("git", ["clone", "-q", "--bare", seed, origin]);
  const clone = path.join(root, "sample-app");
  execFileSync("git", ["clone", "-q", origin, clone]);
  git(clone, "config", "user.email", "t@example.com");
  git(clone, "config", "user.name", "T");
  return { root, origin, clone, cleanup: () => fs.rmSync(root, { recursive: true, force: true }) };
}

/** Pushes a new commit to feature/PROJ-7-login from a second clone. */
function pushCommit(r, name) {
  const other = path.join(r.root, `other-${name}`);
  execFileSync("git", ["clone", "-q", r.origin, other]);
  git(other, "config", "user.email", "t@example.com");
  git(other, "config", "user.name", "T");
  git(other, "checkout", "-q", "feature/PROJ-7-login");
  fs.writeFileSync(path.join(other, `${name}.txt`), name);
  git(other, "add", ".");
  git(other, "commit", "-q", "-m", name);
  git(other, "push", "-q", "origin", "feature/PROJ-7-login");
}

const B = "feature/PROJ-7-login";

test("a clean existing worktree is reused, brought up to date, and is not the job's to delete", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const wt = require("../dist/core/worktree.js");
  const mine = path.join(r.root, "my-tree");
  git(r.clone, "worktree", "add", "-q", "--track", "-b", B, mine, `origin/${B}`);
  pushCommit(r, "newer");

  const { handle, reused } = await wt.acquireWorktree(r.clone, B, "job-1");
  assert.equal(reused, true);
  assert.equal(handle.adopted, true);
  assert.equal(handle.dir, mine);
  assert.equal(git(mine, "rev-parse", "HEAD"), git(r.clone, "rev-parse", `origin/${B}`), "fast-forwarded to origin");
  assert.equal(git(mine, "rev-parse", "--abbrev-ref", "HEAD"), B, "still on the real branch, not detached");

  await wt.removeWorktree(handle);
  assert.ok(fs.existsSync(mine), "removeWorktree never deletes an adopted worktree");
});

test("with no checkout of the branch, a disposable job worktree is made as before", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const wt = require("../dist/core/worktree.js");
  const { handle, reused } = await wt.acquireWorktree(r.clone, B, "job-2");
  assert.equal(reused, false);
  assert.equal(handle.adopted, undefined);
  assert.ok(handle.dir.startsWith(wt.WORKTREE_ROOT));
  await wt.removeWorktree(handle);
  assert.equal(fs.existsSync(handle.dir), false);
});

test("a dirty checkout, one with unpushed commits and the main clone are refused", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const wt = require("../dist/core/worktree.js");
  const mine = path.join(r.root, "my-tree");
  git(r.clone, "worktree", "add", "-q", "--track", "-b", B, mine, `origin/${B}`);

  fs.writeFileSync(path.join(mine, "README.md"), "changed\n");
  await assert.rejects(wt.acquireWorktree(r.clone, B, "j"), /uncommitted/);
  git(mine, "checkout", "--", "README.md");

  fs.writeFileSync(path.join(mine, "local.txt"), "x");
  git(mine, "add", ".");
  git(mine, "commit", "-q", "-m", "local only");
  await assert.rejects(wt.acquireWorktree(r.clone, B, "j"), /1 commit.*aren't on origin/);
  git(mine, "reset", "-q", "--hard", `origin/${B}`);

  git(r.clone, "worktree", "remove", "--force", mine);
  git(r.clone, "checkout", "-q", B);
  await assert.rejects(wt.acquireWorktree(r.clone, B, "j"), /main clone/);
});

test("restoreAdoptedWorktree puts the worktree back as the job found it", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const wt = require("../dist/core/worktree.js");
  const mine = path.join(r.root, "my-tree");
  git(r.clone, "worktree", "add", "-q", "--track", "-b", B, mine, `origin/${B}`);
  const { handle } = await wt.acquireWorktree(r.clone, B, "j");
  const start = git(mine, "rev-parse", "HEAD");
  fs.writeFileSync(path.join(mine, "README.md"), "edited by claude\n");
  fs.writeFileSync(path.join(mine, "new.txt"), "added by claude\n");
  git(mine, "add", ".");
  git(mine, "commit", "-q", "-m", "claude");
  fs.writeFileSync(path.join(mine, "stray.txt"), "left over\n");

  await wt.restoreAdoptedWorktree(handle, start);
  assert.equal(git(mine, "rev-parse", "HEAD"), start);
  assert.equal(git(mine, "status", "--porcelain"), "");
  assert.equal(fs.existsSync(path.join(mine, "new.txt")), false);
  // A job's own worktree is not this function's business.
  await wt.restoreAdoptedWorktree({ id: "x", dir: mine, repoPath: r.clone }, "0".repeat(40));
  assert.equal(git(mine, "rev-parse", "HEAD"), start);
});

test("main clone on the branch: the person's choice decides, and nothing changes before it", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const wt = require("../dist/core/worktree.js");
  git(r.clone, "checkout", "-q", B);

  // Asked with what each option would do.
  let seen;
  const { handle, reused } = await wt.acquireWorktree(r.clone, B, "d1", {
    onMainClone: async (info) => ((seen = info), "disposable"),
  });
  assert.equal(seen.branch, B);
  assert.equal(seen.switchTo, "master");
  assert.equal(seen.ahead, 0);
  assert.equal(reused, false);
  assert.ok(handle.dir.startsWith(wt.WORKTREE_ROOT), "a disposable worktree");
  assert.equal(git(r.clone, "rev-parse", "--abbrev-ref", "HEAD"), B, "the clone was left alone");
  await wt.removeWorktree(handle);

  // Throwing from the callback (Cancel) changes nothing.
  await assert.rejects(
    wt.acquireWorktree(r.clone, B, "d2", { onMainClone: async () => { throw new Error("Cancelled"); } }),
    /Cancelled/,
  );
  assert.equal(git(r.clone, "rev-parse", "--abbrev-ref", "HEAD"), B);

  // "worktree": the clone moves to master, the branch gets a kept worktree that is reused.
  const made = await wt.acquireWorktree(r.clone, B, "d3", { onMainClone: async () => "worktree" });
  assert.equal(git(r.clone, "rev-parse", "--abbrev-ref", "HEAD"), "master");
  assert.equal(made.reused, true);
  assert.equal(made.handle.adopted, true);
  assert.equal(git(made.handle.dir, "rev-parse", "--abbrev-ref", "HEAD"), B);
});

test("main clone with uncommitted changes can't be moved, but a temporary worktree still works", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const wt = require("../dist/core/worktree.js");
  git(r.clone, "checkout", "-q", B);
  fs.writeFileSync(path.join(r.clone, "README.md"), "mine\n");

  let seen;
  await assert.rejects(
    wt.acquireWorktree(r.clone, B, "d4", { onMainClone: async (info) => ((seen = info), "worktree") }),
    /Can't move the clone/,
  );
  assert.equal(seen.switchTo, null);
  assert.match(seen.cannotSwitch, /uncommitted/);
  assert.equal(git(r.clone, "rev-parse", "--abbrev-ref", "HEAD"), B);

  const { handle } = await wt.acquireWorktree(r.clone, B, "d5", { onMainClone: async () => "disposable" });
  assert.equal(fs.readFileSync(path.join(r.clone, "README.md"), "utf8"), "mine\n", "uncommitted work untouched");
  await wt.removeWorktree(handle);
});
