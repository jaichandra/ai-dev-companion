const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const report = require("./worktree-report.js");
const { defaultGit } = require("./ticket-git.js");
const { repoWorktreesRoot } = require("./paths.js");
const { openWorkUnits } = require("./work-units.js");

const git = (cwd, ...args) => execFileSync("git", args, { cwd, encoding: "utf8" }).trim();

function makeRepos() {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "wt-report-")));
  process.env.HOME = path.join(root, "home");
  const seed = path.join(root, "seed");
  fs.mkdirSync(seed);
  git(seed, "init", "-q", "-b", "master");
  git(seed, "config", "user.email", "t@example.com");
  git(seed, "config", "user.name", "T");
  fs.writeFileSync(path.join(seed, "README.md"), "hi\n");
  git(seed, "add", "README.md");
  git(seed, "commit", "-q", "-m", "init");
  const origin = path.join(root, "origin.git");
  execFileSync("git", ["clone", "-q", "--bare", seed, origin]);
  const clone = path.join(root, "app");
  execFileSync("git", ["clone", "-q", origin, clone]);
  git(clone, "config", "user.email", "t@example.com");
  git(clone, "config", "user.name", "T");
  return { root, clone, repos: { "ACME/app": clone }, cleanup: () => fs.rmSync(root, { recursive: true, force: true }) };
}

/** A tool-owned worktree on a new branch (made from origin/master). */
function toolTree(r, name) {
  const dir = path.join(repoWorktreesRoot(r.clone), name);
  fs.mkdirSync(path.dirname(dir), { recursive: true });
  git(r.clone, "worktree", "add", "-q", "-b", name, dir, "origin/master");
  return dir;
}

const byBranch = (rows, b) => rows.find((x) => x.branch === b);

test("it lists the clone, yours and the tool's, and says which can be pruned", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const merged = toolTree(r, "PROJ-1"); // nothing new on it: merged into origin/master
  const dirty = toolTree(r, "PROJ-2");
  fs.writeFileSync(path.join(dirty, "wip.txt"), "x");
  const unpushed = toolTree(r, "PROJ-3");
  fs.writeFileSync(path.join(unpushed, "f.txt"), "x");
  git(unpushed, "add", ".");
  git(unpushed, "commit", "-q", "-m", "local");
  const mine = path.join(r.root, "my-tree");
  git(r.clone, "worktree", "add", "-q", "-b", "mine", mine, "origin/master");

  const rows = await report.listWorktrees({ repos: r.repos, git: defaultGit });
  assert.equal(rows.length, 5);
  assert.equal(byBranch(rows, "master").isMainClone, true);
  assert.equal(byBranch(rows, "master").prunable, false);
  assert.equal(byBranch(rows, "mine").owned, false);
  assert.equal(byBranch(rows, "mine").why, "yours, not made by the tool");
  assert.equal(byBranch(rows, "PROJ-1").prunable, true);
  assert.equal(byBranch(rows, "PROJ-1").merged, true);
  assert.equal(byBranch(rows, "PROJ-2").why, "1 uncommitted change");
  assert.equal(byBranch(rows, "PROJ-3").why, "1 commit not pushed");
  assert.deepEqual(rows.filter((x) => x.prunable).map((x) => x.dir), [merged]);
});

test("a branch that was pushed and merged away upstream (gone) is prunable; one not merged is kept", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const work = toolTree(r, "PROJ-4");
  fs.writeFileSync(path.join(work, "f.txt"), "x");
  git(work, "add", ".");
  git(work, "commit", "-q", "-m", "change");
  git(work, "push", "-q", "-u", "origin", "PROJ-4");
  let rows = await report.listWorktrees({ repos: r.repos, git: defaultGit });
  assert.equal(byBranch(rows, "PROJ-4").why, "branch not merged", "pushed but not merged and not deleted");

  git(r.clone, "push", "-q", "origin", "--delete", "PROJ-4"); // as after merging the PR
  git(r.clone, "fetch", "-q", "--prune", "origin");
  rows = await report.listWorktrees({ repos: r.repos, git: defaultGit });
  assert.equal(byBranch(rows, "PROJ-4").upstreamGone, true);
  assert.equal(byBranch(rows, "PROJ-4").prunable, true);
});

test("the registry supplies ticket, PR, sessions and last use", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const dir = toolTree(r, "PROJ-5");
  const reg = openWorkUnits({ file: path.join(r.root, "units.json"), now: () => 5000 });
  const unit = reg.upsert({ repoPath: r.clone, branch: "PROJ-5", dir, issueKey: "PROJ-5", prKey: "bitbucket:ACME/app#9" });
  reg.addSession(unit.key, { id: "s-1", cwd: dir });
  const rows = await report.listWorktrees({ repos: r.repos, units: reg.list(), git: defaultGit });
  const row = byBranch(rows, "PROJ-5");
  assert.deepEqual([row.issueKey, row.prKey, row.sessions, row.lastUsed], ["PROJ-5", "bitbucket:ACME/app#9", 1, 5000]);
});

test("pruning removes only the prunable worktrees, keeps the branches and clears stale units", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  const gone = toolTree(r, "PROJ-6");
  const kept = toolTree(r, "PROJ-7");
  fs.writeFileSync(path.join(kept, "wip.txt"), "x");
  const reg = openWorkUnits({ file: path.join(r.root, "units.json") });
  reg.upsert({ repoPath: r.clone, branch: "PROJ-6", dir: gone });
  reg.upsert({ repoPath: r.clone, branch: "PROJ-7", dir: kept });

  const rows = await report.listWorktrees({ repos: r.repos, units: reg.list(), git: defaultGit });
  const result = await report.pruneWorktrees(rows, { git: defaultGit, workUnits: reg });
  assert.deepEqual(result.removed.map((x) => x.dir), [gone]);
  assert.equal(fs.existsSync(gone), false);
  assert.equal(fs.existsSync(kept), true);
  assert.match(git(r.clone, "branch", "--list", "PROJ-6"), /PROJ-6/, "the branch stays");
  assert.deepEqual(reg.list().map((u) => u.branch), ["PROJ-7"]);
});

test("countOwned counts only the tool's worktrees", async (t) => {
  const r = makeRepos();
  t.after(r.cleanup);
  toolTree(r, "PROJ-8");
  toolTree(r, "PROJ-9");
  git(r.clone, "worktree", "add", "-q", "-b", "mine", path.join(r.root, "mine"), "origin/master");
  assert.equal(await report.countOwned({ repoPath: r.clone, git: defaultGit }), 2);
});

test("a repo that isn't a clone is skipped", async () => {
  const rows = await report.listWorktrees({ repos: { "ACME/none": "/nonexistent/path" }, git: defaultGit });
  assert.deepEqual(rows, []);
});
