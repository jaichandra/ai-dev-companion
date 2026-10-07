const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const tg = require("./ticket-git.js");
const { APP_SLUG } = require("./app-slug.js");

const git = (cwd, ...args) => execFileSync("git", args, { cwd, encoding: "utf8" }).trim();

/** A bare "origin" with one commit on master, and a clone of it — all in a
 * temp dir, no network. `root` is realpath'd (macOS /var -> /private/var)
 * so paths compare equal to what git prints. */
function makeRepos() {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "ticket-git-test-")));
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
  const clone = path.join(root, "sample-app");
  execFileSync("git", ["clone", "-q", origin, clone]);
  git(clone, "config", "user.email", "t@example.com");
  git(clone, "config", "user.name", "T");
  // Worktrees live under the state dir in $HOME — point it into the temp dir.
  process.env.HOME = path.join(root, "home");
  const wtRoot = path.join(root, "home", `.${APP_SLUG}`, "sample-app.worktrees");
  return { root, origin, clone, wtRoot, cleanup: () => fs.rmSync(root, { recursive: true, force: true }) };
}

test("keyPattern matches the key at a boundary, case-insensitively, and not a longer number", () => {
  const re = tg.keyPattern("PROJ-12");
  assert.ok(re.test("bugfix/PROJ-12-login"));
  assert.ok(re.test("proj-12"));
  assert.ok(re.test("feature/x_PROJ-12"));
  assert.ok(!re.test("bugfix/PROJ-123"));
  assert.ok(!re.test("XPROJ-12"));
  assert.throws(() => tg.keyPattern("proj-12"));
  assert.throws(() => tg.keyPattern("PROJ-12)|(.*"));
});

test("ticketWorktreePath is <stateDir>/<repo>.worktrees/<KEY>", () => {
  assert.equal(tg.ticketWorktreePath("/Users/x/gitviews/sample-app", "PROJ-7"), path.join(os.homedir(), `.${APP_SLUG}`, "sample-app.worktrees", "PROJ-7"));
  assert.throws(() => tg.ticketWorktreePath("/Users/x/sample-app", "../../etc"));
});

test("parseForEachRef merges local and origin branches that carry the key", () => {
  const out = [
    "refs/heads/bugfix/PROJ-7-login\taaa\torigin/bugfix/PROJ-7-login\t[ahead 2, behind 1]",
    "refs/heads/master\tbbb\torigin/master\t",
    "refs/remotes/origin/bugfix/PROJ-7-login\tccc\t\t",
    "refs/remotes/origin/PROJ-7-spike\tddd\t\t",
    "refs/remotes/origin/HEAD\teee\t\t",
    "refs/heads/PROJ-70\tfff\t\t",
  ].join("\n");
  assert.deepEqual(tg.parseForEachRef(out, "PROJ-7"), [
    { name: "bugfix/PROJ-7-login", local: true, remote: true, sha: "aaa", upstream: "origin/bugfix/PROJ-7-login", ahead: 2, behind: 1, gone: false },
    { name: "PROJ-7-spike", local: false, remote: true, sha: "ddd", upstream: null, ahead: 0, behind: 0, gone: false },
  ]);
  assert.deepEqual(tg.parseTrack("[gone]"), { ahead: 0, behind: 0, gone: true });
});

test("parseWorktreePorcelain reads dirs, heads, branches and detached entries", () => {
  const out = "worktree /r/sample-app\nHEAD 111\nbranch refs/heads/master\n\nworktree /r/sample-app.worktrees/pr-review\nHEAD 222\ndetached\n\n";
  assert.deepEqual(tg.parseWorktreePorcelain(out), [
    { dir: "/r/sample-app", head: "111", branch: "master", detached: false },
    { dir: "/r/sample-app.worktrees/pr-review", head: "222", branch: null, detached: true },
  ]);
});

test("ensureTicketWorktree creates the worktree on a new branch from origin/<base>, then reuses it", async () => {
  const r = makeRepos();
  try {
    const first = await tg.ensureTicketWorktree(r.clone, { issueKey: "PROJ-7", branch: "bugfix/PROJ-7-login", base: "master" });
    assert.equal(first.created, true);
    assert.equal(first.dir, path.join(r.wtRoot, "PROJ-7"));
    assert.equal(git(first.dir, "rev-parse", "--abbrev-ref", "HEAD"), "bugfix/PROJ-7-login");
    // --no-track: no upstream until the first push.
    assert.throws(() => git(first.dir, "rev-parse", "--abbrev-ref", "@{upstream}"));

    const again = await tg.ensureTicketWorktree(r.clone, { issueKey: "PROJ-7", branch: "other-name", base: "master" });
    assert.deepEqual(again, { dir: first.dir, branch: "bugfix/PROJ-7-login", created: false });
  } finally {
    r.cleanup();
  }
});

test("ensureTicketWorktree checks out an existing origin branch, and refuses a stray folder or a bad name", async () => {
  const r = makeRepos();
  try {
    git(r.clone, "push", "-q", "origin", "master:refs/heads/bugfix/PROJ-8-x");
    git(r.clone, "fetch", "-q", "origin");
    const wt = await tg.ensureTicketWorktree(r.clone, { issueKey: "PROJ-8", branch: "bugfix/PROJ-8-x", base: "master" });
    assert.equal(git(wt.dir, "rev-parse", "--abbrev-ref", "@{upstream}"), "origin/bugfix/PROJ-8-x");

    fs.mkdirSync(path.join(r.wtRoot, "PROJ-9"), { recursive: true });
    fs.writeFileSync(path.join(r.wtRoot, "PROJ-9", "keep.txt"), "mine");
    await assert.rejects(
      tg.ensureTicketWorktree(r.clone, { issueKey: "PROJ-9", branch: "bugfix/PROJ-9", base: "master" }),
      /isn't a worktree/,
    );
    assert.equal(fs.readFileSync(path.join(r.wtRoot, "PROJ-9", "keep.txt"), "utf8"), "mine");

    await assert.rejects(tg.ensureTicketWorktree(r.clone, { issueKey: "PROJ-10", branch: "-rf", base: "master" }), /safe branch/);
    await assert.rejects(tg.ensureTicketWorktree(r.clone, { issueKey: "PROJ-10", branch: "ok", base: "a..b" }), /safe base/);
  } finally {
    r.cleanup();
  }
});

test("scanRepoForTicket lists the ticket's branches and worktrees with dirty and ahead counts", async () => {
  const r = makeRepos();
  try {
    const wt = await tg.ensureTicketWorktree(r.clone, { issueKey: "PROJ-7", branch: "bugfix/PROJ-7-login", base: "master" });
    fs.writeFileSync(path.join(wt.dir, "a.txt"), "a\n");
    git(wt.dir, "add", "a.txt");
    git(wt.dir, "commit", "-q", "-m", "PROJ-7: add a");
    await tg.pushBranch(wt.dir, "bugfix/PROJ-7-login");
    fs.writeFileSync(path.join(wt.dir, "b.txt"), "b\n");
    git(wt.dir, "add", "b.txt");
    git(wt.dir, "commit", "-q", "-m", "PROJ-7: add b");
    fs.writeFileSync(path.join(wt.dir, "c.txt"), "dirty\n");

    const scan = await tg.scanRepoForTicket(r.clone, "PROJ-7");
    assert.equal(scan.branches.length, 1);
    assert.equal(scan.branches[0].name, "bugfix/PROJ-7-login");
    assert.equal(scan.branches[0].local, true);
    assert.equal(scan.branches[0].remote, true);
    assert.equal(scan.branches[0].ahead, 1);
    assert.equal(scan.worktrees.length, 1);
    assert.equal(scan.worktrees[0].dir, wt.dir);
    assert.equal(scan.worktrees[0].dirty, true);
    assert.equal(scan.worktrees[0].changed, 1);
    assert.equal(scan.worktrees[0].isTicketWorktree, true);

    assert.equal(await tg.commitsAhead(wt.dir, "master"), 2);
    assert.match(await tg.commitLog(wt.dir, "master"), /- PROJ-7: add b[\s\S]*- PROJ-7: add a/);
    assert.match(await tg.diffStat(wt.dir, "master"), /a\.txt[\s\S]*b\.txt/);

    const none = await tg.scanRepoForTicket(r.clone, "PROJ-99");
    assert.deepEqual(none, { branches: [], worktrees: [] });
  } finally {
    r.cleanup();
  }
});

test("defaultBranchFromGit uses origin/HEAD, then master or main", async () => {
  const r = makeRepos();
  try {
    assert.equal(await tg.defaultBranchFromGit(r.clone), "master");
    git(r.clone, "remote", "set-head", "origin", "--delete");
    assert.equal(await tg.defaultBranchFromGit(r.clone), "master");
  } finally {
    r.cleanup();
  }
});

test("fetchBase brings origin/<base> up to date so the ahead count isn't measured against a stale ref", async () => {
  const r = makeRepos();
  try {
    const wt = await tg.ensureTicketWorktree(r.clone, { issueKey: "PROJ-5", branch: "bugfix/PROJ-5-x", base: "master" });
    // Someone else lands a commit on origin/master; our origin/master ref is now stale.
    const other = path.join(r.root, "other");
    execFileSync("git", ["clone", "-q", r.origin, other]);
    git(other, "config", "user.email", "t@example.com");
    git(other, "config", "user.name", "T");
    fs.writeFileSync(path.join(other, "n.txt"), "n\n");
    git(other, "add", "n.txt");
    git(other, "commit", "-q", "-m", "new on master");
    git(other, "push", "-q", "origin", "master");
    assert.equal(git(wt.dir, "rev-list", "--count", "HEAD..refs/remotes/origin/master"), "0");
    await tg.fetchBase(wt.dir, "master");
    assert.equal(git(wt.dir, "rev-list", "--count", "HEAD..refs/remotes/origin/master"), "1");
    await assert.rejects(tg.fetchBase(wt.dir, "bad;name"), /safe base/);
  } finally {
    r.cleanup();
  }
});

test("defaultGit kills a hung git after its timeout and says so; errors are redacted", async () => {
  const started = Date.now();
  await assert.rejects(
    tg.defaultGit(["-c", "alias.hang=!sleep 5", "hang"], process.cwd(), { timeoutMs: 200 }),
    /timed out after 0 s|timed out/,
  );
  assert.ok(Date.now() - started < 3000);
  await assert.rejects(
    tg.defaultGit(["ls-remote", "https://me:pw@127.0.0.1:1/x.git"], process.cwd(), { timeoutMs: 20000 }),
    (err) => !/me:pw/.test(err.message),
  );
});

test("fetchBase uses a forced refspec so a rewritten base doesn't fail the fetch", async () => {
  const calls = [];
  await tg.fetchBase("/x", "master", { git: async (args) => void calls.push(args) });
  assert.deepEqual(calls[0], ["fetch", "origin", "+refs/heads/master:refs/remotes/origin/master"]);
});
