const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const { reviewWorktreePath, isRegisteredWorktree, provisionReviewWorktree } = require("./review-worktree.js");
const { APP_SLUG } = require("./app-slug.js");

// Worktrees live under $HOME/.ai-dev-companion — keep tests out of the real one.
process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "review-worktree-home-"));

/** A real, network-free git repo with one commit — `git worktree add
 * origin/<ref>` needs an actual commit to check out, unlike
 * core/prereqs.test.js's bare `git init` + fake remote. */
function makeFakeOrigin(dir) {
  fs.mkdirSync(dir, { recursive: true });
  execFileSync("git", ["init", "-q"], { cwd: dir });
  // Deliberately renaming the unborn branch explicitly rather than relying
  // on `git init`'s default (which follows the machine's global
  // init.defaultBranch) — keeps this test's expectations independent of
  // whatever that's set to.
  execFileSync("git", ["checkout", "-q", "-b", "main"], { cwd: dir });
  execFileSync("git", ["config", "user.email", "test@example.com"], { cwd: dir });
  execFileSync("git", ["config", "user.name", "Test"], { cwd: dir });
  fs.writeFileSync(path.join(dir, "README.md"), "hello\n");
  execFileSync("git", ["add", "README.md"], { cwd: dir });
  execFileSync("git", ["commit", "-q", "-m", "init"], { cwd: dir });
}

/** A real clone of `sourceDir` — gives us a repo with an actual "origin"
 * remote (a bare `git init` has none), so fetch/worktree-add against
 * origin/<ref> works without any network access. */
function cloneFrom(sourceDir, destDir) {
  execFileSync("git", ["clone", "-q", sourceDir, destDir]);
}

test("reviewWorktreePath is <stateDir>/<repo>.worktrees/pr-review", () => {
  const p = reviewWorktreePath("/Users/x/gitviews/sample-app");
  assert.equal(p, path.join(os.homedir(), `.${APP_SLUG}`, "sample-app.worktrees", "pr-review"));
});

test("isRegisteredWorktree is false before anything has been provisioned", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "review-worktree-test-"));
  try {
    const origin = path.join(root, "origin");
    const clone = path.join(root, "clone");
    makeFakeOrigin(origin);
    cloneFrom(origin, clone);
    const result = await isRegisteredWorktree(clone, reviewWorktreePath(clone));
    assert.equal(result, false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("provisionReviewWorktree creates a real worktree the first time", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "review-worktree-test-"));
  try {
    const origin = path.join(root, "origin");
    const clone = path.join(root, "clone");
    makeFakeOrigin(origin);
    cloneFrom(origin, clone);

    const { dir, created } = await provisionReviewWorktree(clone);
    assert.equal(created, true);
    assert.equal(dir, reviewWorktreePath(clone));
    assert.ok(fs.existsSync(path.join(dir, "README.md")));
    assert.equal(await isRegisteredWorktree(clone, dir), true);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("provisionReviewWorktree is idempotent, and leaves an off-branch worktree alone", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "review-worktree-test-"));
  try {
    const origin = path.join(root, "origin");
    const clone = path.join(root, "clone");
    makeFakeOrigin(origin);
    cloneFrom(origin, clone);

    const first = await provisionReviewWorktree(clone);
    // Simulate the user (or a real review) having since checked out a real
    // local branch by hand.
    execFileSync("git", ["checkout", "-q", "-B", "some-review-branch"], { cwd: first.dir });

    const second = await provisionReviewWorktree(clone);
    assert.equal(second.created, false);
    assert.equal(second.dir, first.dir);
    const branch = execFileSync("git", ["branch", "--show-current"], { cwd: first.dir, encoding: "utf8" }).trim();
    assert.equal(branch, "some-review-branch");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("provisionReviewWorktree recovers when the directory was deleted but git still registers it", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "review-worktree-test-"));
  try {
    const origin = path.join(root, "origin");
    const clone = path.join(root, "clone");
    makeFakeOrigin(origin);
    cloneFrom(origin, clone);

    const first = await provisionReviewWorktree(clone);
    fs.rmSync(first.dir, { recursive: true, force: true });

    const second = await provisionReviewWorktree(clone);
    assert.equal(second.created, true);
    assert.ok(fs.existsSync(second.dir));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
