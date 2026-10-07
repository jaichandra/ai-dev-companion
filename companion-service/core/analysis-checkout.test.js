const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const ac = require("./analysis-checkout.js");

const git = (cwd, ...args) => execFileSync("git", args, { cwd, encoding: "utf8" }).trim();

function makeRepos() {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "analysis-checkout-test-")));
  const seed = path.join(root, "seed");
  fs.mkdirSync(seed);
  git(seed, "init", "-q", "-b", "master");
  git(seed, "config", "user.email", "t@example.com");
  git(seed, "config", "user.name", "T");
  fs.writeFileSync(path.join(seed, "a.txt"), "one\n");
  git(seed, "add", "a.txt");
  git(seed, "commit", "-q", "-m", "init");
  const origin = path.join(root, "origin.git");
  execFileSync("git", ["clone", "-q", "--bare", seed, origin]);
  const clone = path.join(root, "sample-app");
  execFileSync("git", ["clone", "-q", origin, clone]);
  git(clone, "config", "user.email", "t@example.com");
  git(clone, "config", "user.name", "T");
  const checkouts = path.join(root, "state", "repos");
  return { root, seed, origin, clone, checkouts, cleanup: () => fs.rmSync(root, { recursive: true, force: true }) };
}

test("checkoutPath accepts PROJECT/repo only and stays under the root", () => {
  assert.equal(ac.checkoutPath("/s/repos", "ACME/sample-app"), path.join("/s/repos", "ACME", "sample-app"));
  for (const bad of ["sample-app", "a/b/c", "../x", "A/..", "A/.", "/etc", "A/b c", "", null, undefined]) {
    assert.equal(ac.checkoutPath("/s/repos", bad), null, String(bad));
  }
});

test("the first use makes a detached checkout of origin/master, apart from the user's clone", async () => {
  const r = makeRepos();
  try {
    const out = await ac.ensureAnalysisCheckout({ userClone: r.clone, repoKey: "ACME/sample-app", root: r.checkouts });
    assert.equal(out.dir, path.join(r.checkouts, "ACME", "sample-app"));
    assert.equal(out.branch, "master");
    assert.equal(out.fresh, true);
    assert.equal(fs.readFileSync(path.join(out.dir, "a.txt"), "utf8"), "one\n");
    assert.equal(git(out.dir, "rev-parse", "--abbrev-ref", "HEAD"), "HEAD");
    assert.equal(git(out.dir, "remote", "get-url", "origin"), r.origin);
    assert.equal(fs.existsSync(`${r.clone}.worktrees`), false);
  } finally {
    r.cleanup();
  }
});

test("a later use moves to the newest origin/master and ignores the user's own branch and edits", async () => {
  const r = makeRepos();
  try {
    await ac.ensureAnalysisCheckout({ userClone: r.clone, repoKey: "ACME/sample-app", root: r.checkouts });
    // The user is on a feature branch with an uncommitted edit; master moves on at origin.
    git(r.clone, "checkout", "-q", "-b", "feature/x");
    fs.writeFileSync(path.join(r.clone, "a.txt"), "user edit\n");
    const other = path.join(r.root, "other");
    execFileSync("git", ["clone", "-q", r.origin, other]);
    git(other, "config", "user.email", "t@example.com");
    git(other, "config", "user.name", "T");
    fs.writeFileSync(path.join(other, "a.txt"), "two\n");
    git(other, "commit", "-q", "-am", "second");
    git(other, "push", "-q", "origin", "master");

    const out = await ac.ensureAnalysisCheckout({ userClone: r.clone, repoKey: "ACME/sample-app", root: r.checkouts });
    assert.equal(out.fresh, true);
    assert.equal(fs.readFileSync(path.join(out.dir, "a.txt"), "utf8"), "two\n");
    assert.equal(fs.readFileSync(path.join(r.clone, "a.txt"), "utf8"), "user edit\n");
    assert.equal(git(r.clone, "rev-parse", "--abbrev-ref", "HEAD"), "feature/x");
  } finally {
    r.cleanup();
  }
});

test("it follows the remote's default branch even when the user's clone sits on another branch", async () => {
  const r = makeRepos();
  try {
    git(r.clone, "checkout", "-q", "-b", "feature/x");
    fs.writeFileSync(path.join(r.clone, "a.txt"), "feature\n");
    git(r.clone, "commit", "-q", "-am", "feature work");
    git(r.clone, "push", "-q", "origin", "feature/x");
    const out = await ac.ensureAnalysisCheckout({ userClone: r.clone, repoKey: "ACME/sample-app", root: r.checkouts });
    assert.equal(out.branch, "master");
    assert.equal(fs.readFileSync(path.join(out.dir, "a.txt"), "utf8"), "one\n");
  } finally {
    r.cleanup();
  }
});

test("a failed fetch keeps the last copy and says it is not fresh", async () => {
  const r = makeRepos();
  try {
    await ac.ensureAnalysisCheckout({ userClone: r.clone, repoKey: "ACME/sample-app", root: r.checkouts });
    fs.renameSync(r.origin, `${r.origin}.gone`);
    const out = await ac.ensureAnalysisCheckout({ userClone: r.clone, repoKey: "ACME/sample-app", root: r.checkouts });
    assert.equal(out.fresh, false);
    assert.equal(fs.readFileSync(path.join(out.dir, "a.txt"), "utf8"), "one\n");
  } finally {
    r.cleanup();
  }
});

test("two refreshes of the same checkout at once run one after the other", async () => {
  const r = makeRepos();
  try {
    const args = { userClone: r.clone, repoKey: "ACME/sample-app", root: r.checkouts };
    const [a, b] = await Promise.all([ac.ensureAnalysisCheckout(args), ac.ensureAnalysisCheckout(args)]);
    assert.equal(a.dir, b.dir);
    assert.equal(fs.readFileSync(path.join(a.dir, "a.txt"), "utf8"), "one\n");
  } finally {
    r.cleanup();
  }
});

test("a bad repo key is refused before anything is created", async () => {
  await assert.rejects(ac.ensureAnalysisCheckout({ userClone: "/nope", repoKey: "../x", root: "/nope/repos" }), /Not a repo key/);
});
