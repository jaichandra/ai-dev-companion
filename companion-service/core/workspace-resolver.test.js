const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const wr = require("./workspace-resolver.js");
const { APP_SLUG } = require("./app-slug.js");

const git = (cwd, ...args) => execFileSync("git", args, { cwd, encoding: "utf8" }).trim();

/** A bare origin with master plus a clone, in a temp dir; HOME points inside it. */
function makeRepos() {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "workspace-resolver-test-")));
  const seed = path.join(root, "seed");
  fs.mkdirSync(seed);
  git(seed, "init", "-q", "-b", "master");
  git(seed, "config", "user.email", "t@example.com");
  git(seed, "config", "user.name", "T");
  fs.writeFileSync(path.join(seed, "README.md"), "hi\n");
  git(seed, "add", "README.md");
  git(seed, "commit", "-q", "-m", "init");
  git(seed, "branch", "feature/PROJ-7-login");
  git(seed, "branch", "feature/PROJ-7-logout");
  const origin = path.join(root, "origin.git");
  execFileSync("git", ["clone", "-q", "--bare", seed, origin]);
  const clone = path.join(root, "sample-app");
  execFileSync("git", ["clone", "-q", origin, clone]);
  git(clone, "config", "user.email", "t@example.com");
  git(clone, "config", "user.name", "T");
  process.env.HOME = path.join(root, "home");
  const wtRoot = path.join(root, "home", `.${APP_SLUG}`, "sample-app.worktrees");
  return { root, origin, clone, wtRoot, cleanup: () => fs.rmSync(root, { recursive: true, force: true }) };
}

const worktreeCount = (clone) => git(clone, "worktree", "list", "--porcelain").split("\n").filter((l) => l.startsWith("worktree ")).length;

test("a branch checked out in the clone itself is the first choice", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  git(r.clone, "checkout", "-q", "-b", "feature/PROJ-7-login", "--track", "origin/feature/PROJ-7-login");
  const res = await wr.resolveWorkspace(r.clone, { branch: "feature/PROJ-7-login" });
  assert.equal(res.kind, "worktree");
  assert.equal(res.isMainClone, true);
  assert.equal(res.dir, r.clone);
  assert.equal(res.usable, true);
});

test("a branch checked out in a worktree at any path is found", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const elsewhere = path.join(r.root, "my-own-tree");
  git(r.clone, "worktree", "add", "-q", "--track", "-b", "feature/PROJ-7-login", elsewhere, "origin/feature/PROJ-7-login");
  const res = await wr.resolveWorkspace(r.clone, { branch: "feature/PROJ-7-login" });
  assert.equal(res.kind, "worktree");
  assert.equal(res.isMainClone, false);
  assert.equal(res.dir, elsewhere);
  assert.equal(res.usable, true);
});

test("a dirty or mid-merge checkout is reported but not usable", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  git(r.clone, "checkout", "-q", "-b", "feature/PROJ-7-login", "--track", "origin/feature/PROJ-7-login");
  fs.writeFileSync(path.join(r.clone, "README.md"), "changed\n");
  let res = await wr.resolveWorkspace(r.clone, { branch: "feature/PROJ-7-login" });
  assert.equal(res.kind, "worktree");
  assert.equal(res.usable, false);
  assert.equal(res.dirty, true);
  assert.match(res.reason, /1 uncommitted change/);

  git(r.clone, "checkout", "--", "README.md");
  fs.writeFileSync(path.join(r.clone, ".git", "MERGE_HEAD"), git(r.clone, "rev-parse", "HEAD") + "\n");
  res = await wr.resolveWorkspace(r.clone, { branch: "feature/PROJ-7-login" });
  assert.equal(res.usable, false);
  assert.equal(res.inProgress, "merge");
  assert.match(res.reason, /merge is in progress/);
});

test("a branch only on origin is offered, and nothing is created", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const before = worktreeCount(r.clone);
  const res = await wr.resolveWorkspace(r.clone, { branch: "feature/PROJ-7-login" });
  assert.equal(res.kind, "branch");
  assert.equal(res.remote, true);
  assert.equal(res.local, false);
  assert.equal(worktreeCount(r.clone), before);
  assert.equal(fs.existsSync(r.wtRoot), false);
});

test("an unknown branch is none", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const res = await wr.resolveWorkspace(r.clone, { branch: "feature/nope" });
  assert.deepEqual({ kind: res.kind, branch: res.branch }, { kind: "none", branch: "feature/nope" });
});

test("with only a ticket key it prefers the branch that has a worktree and lists the others", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const tree = path.join(r.root, "logout-tree");
  git(r.clone, "worktree", "add", "-q", "--track", "-b", "feature/PROJ-7-logout", tree, "origin/feature/PROJ-7-logout");
  const res = await wr.resolveWorkspace(r.clone, { issueKey: "PROJ-7" });
  assert.equal(res.kind, "worktree");
  assert.equal(res.branch, "feature/PROJ-7-logout");
  assert.deepEqual(res.alternatives, ["feature/PROJ-7-login"]);
});

test("with a key and no worktree it offers a branch; with no matching branch, none", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const res = await wr.resolveWorkspace(r.clone, { issueKey: "PROJ-7" });
  assert.equal(res.kind, "branch");
  assert.equal(res.alternatives.length, 1);
  assert.equal((await wr.resolveWorkspace(r.clone, { issueKey: "PROJ-99" })).kind, "none");
});

test("resolveWorkspace refuses an unsafe branch, a bad key and no input", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  await assert.rejects(wr.resolveWorkspace(r.clone, { branch: "--upload-pack=x" }), /safe branch/);
  await assert.rejects(wr.resolveWorkspace(r.clone, { issueKey: "nope" }), /issue key/);
  await assert.rejects(wr.resolveWorkspace(r.clone, {}), /branch or an issue key/);
});

test("createBranchWorktree makes a tracking worktree for an origin branch, once", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const made = await wr.createBranchWorktree(r.clone, "feature/PROJ-7-login");
  assert.equal(made.dir, path.join(r.wtRoot, "feature-PROJ-7-login"));
  assert.equal(git(made.dir, "rev-parse", "--abbrev-ref", "HEAD"), "feature/PROJ-7-login");
  assert.equal(git(made.dir, "rev-parse", "--abbrev-ref", "@{upstream}"), "origin/feature/PROJ-7-login");
  const again = await wr.resolveWorkspace(r.clone, { branch: "feature/PROJ-7-login" });
  assert.equal(again.kind, "worktree");
  assert.equal(again.dir, made.dir);
  await assert.rejects(wr.createBranchWorktree(r.clone, "feature/PROJ-7-login"), /already exists/);
});

test("createBranchWorktree never invents a branch", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  await assert.rejects(wr.createBranchWorktree(r.clone, "feature/new-thing"), /fetch|doesn't exist/);
  assert.equal(worktreeCount(r.clone), 1);
});
